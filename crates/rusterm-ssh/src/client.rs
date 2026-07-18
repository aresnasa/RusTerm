use std::sync::Arc;

use russh::client;
use russh::{ChannelMsg, Pty};
use tokio::sync::mpsc;

use rusterm_core::config::{SshAuth, SshConfig};
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};
use rusterm_core::terminal::TerminalSize;

use crate::known_hosts::{HostKeyPolicy, verify_server_key};

/// russh `Handler` carrying the per-connection state needed to verify
/// the server's host key against `known_hosts`.
///
/// The russh `client::Handler` trait is constructed *by us* before the
/// connection is established, so this is where we stash the host name and
/// the user's [`HostKeyPolicy`]. The actual verification logic lives in
/// [`crate::known_hosts::verify_server_key`].
#[derive(Debug, Clone)]
pub struct Handler {
    host: String,
    policy: HostKeyPolicy,
}

impl Handler {
    /// Build a handler for a connection to `host` with the given policy.
    ///
    /// `policy` is derived from `SshConfig::host_key_policy` by the caller
    /// (see [`SshClient::connect`]). We don't take the whole `SshConfig`
    /// here to avoid leaking secrets (e.g. password) into the handler —
    /// the handler is moved across tasks and the smaller its surface the
    /// better.
    pub fn new(host: String, policy: HostKeyPolicy) -> Self {
        Self { host, policy }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    /// Verify the server's host key against `known_hosts`.
    ///
    /// russh calls this with the server's presented public key. We return
    /// `Ok(true)` to accept, `Ok(false)` to reject. We MUST NOT return
    /// `Err` — russh's API contract treats that as a fatal protocol error
    /// and may panic or hang the connection, so even on internal failures
    /// we fail closed via `Ok(false)` and log the reason.
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        let outcome = verify_server_key(&self.host, server_public_key, self.policy, None);
        match outcome {
            crate::known_hosts::VerifyOutcome::Matched => {
                tracing::info!(
                    "[SSH] host key for {:?} matches known_hosts entry",
                    self.host
                );
                Ok(true)
            }
            crate::known_hosts::VerifyOutcome::Added => {
                // TOFU: first contact, key was recorded.
                Ok(true)
            }
            crate::known_hosts::VerifyOutcome::Mismatch {
                expected,
                presented,
            } => {
                // LIKELY MITM. Reject and log loudly — include both
                // fingerprints so the user can investigate which key is
                // the "real" one (e.g. via out-of-band verification).
                tracing::error!(
                    "[SSH] HOST KEY MISMATCH for {:?} — possible MITM! \
                     expected fingerprint {}, presented {}. Rejecting.",
                    self.host,
                    expected,
                    presented
                );
                Ok(false)
            }
            crate::known_hosts::VerifyOutcome::UnknownHost => {
                // Strict mode: host not in known_hosts → reject.
                tracing::warn!(
                    "[SSH] host {:?} not in known_hosts and policy is strict — rejecting. \
                     Pre-populate known_hosts (e.g. via ssh-keyscan) or relax to accept-new.",
                    self.host
                );
                Ok(false)
            }
            crate::known_hosts::VerifyOutcome::Skipped => {
                // Verification disabled — accept, but we already warned
                // inside verify_server_key.
                Ok(true)
            }
        }
    }
}

pub struct SshClient {
    config: SshConfig,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
}

impl SshClient {
    pub fn new(config: SshConfig, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self { config, event_tx }
    }

    /// Drive a keyboard-interactive auth exchange to completion using `password`
    /// as the answer to every prompt. This is the standard "PAM password via
    /// keyboard-interactive" pattern: the server sends one prompt (e.g.
    /// "Password: ") and we reply with the password. Loops because some servers
    /// send multiple (empty) prompts before the real one.
    async fn auth_keyboard_interactive(
        handle: &mut client::Handle<Handler>,
        username: &str,
        password: &str,
    ) -> anyhow::Result<client::AuthResult> {
        use client::KeyboardInteractiveAuthResponse;
        let mut response = handle
            .authenticate_keyboard_interactive_start(username, None::<String>)
            .await?;
        loop {
            match response {
                KeyboardInteractiveAuthResponse::Success => {
                    return Ok(client::AuthResult::Success);
                }
                KeyboardInteractiveAuthResponse::Failure { .. } => {
                    // AuthResult::Failure carries (remaining_methods, partial_success);
                    // synthesize one so the caller's `matches!(.., Success)` check
                    // fails the same way as a plain password-auth failure.
                    return Ok(client::AuthResult::Failure {
                        remaining_methods: russh::MethodSet::empty(),
                        partial_success: false,
                    });
                }
                KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                    // Reply with the password for every prompt the server sends.
                    // Most PAM keyboard-interactive flows use a single "Password:"
                    // prompt, but some servers (e.g. Google's) send a second OTP
                    // prompt we cannot answer — sending the password there is
                    // harmless and lets the server reject it explicitly.
                    let answers: Vec<String> =
                        prompts.iter().map(|_p| password.to_string()).collect();
                    response = handle
                        .authenticate_keyboard_interactive_respond(answers)
                        .await?;
                }
            }
        }
    }

    pub async fn connect(
        &self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> anyhow::Result<(Session, SshSession)> {
        let config = Arc::new(client::Config::default());

        // Derive the host-key verification policy from the user's config.
        // Unknown / empty values fall back to AcceptNew (TOFU) inside
        // `HostKeyPolicy::parse`.
        let policy = HostKeyPolicy::parse(&self.config.host_key_policy);
        let handler = Handler::new(self.config.host.clone(), policy);

        let mut handle = client::connect(
            config,
            (self.config.host.as_str(), self.config.port),
            handler,
        )
        .await?;

        match &self.config.auth {
            SshAuth::Password { password } => {
                let result = handle
                    .authenticate_password(&self.config.username, password.as_str())
                    .await?;
                if !matches!(result, client::AuthResult::Success) {
                    // Some servers (notably jump hosts / bastions and PAM-configured
                    // Linux boxes) reject "password" auth but accept the same
                    // credential via "keyboard-interactive". Try it as a fallback
                    // before giving up — this is what OpenSSH's ssh client does too.
                    tracing::info!(
                        "[SSH] password auth returned {:?}, trying keyboard-interactive fallback for {}@{}",
                        result,
                        self.config.username,
                        self.config.host
                    );
                    let ki_result = Self::auth_keyboard_interactive(
                        &mut handle,
                        &self.config.username,
                        password,
                    )
                    .await?;
                    if !matches!(ki_result, client::AuthResult::Success) {
                        anyhow::bail!(
                            "SSH authentication failed (tried password then keyboard-interactive)"
                        );
                    }
                }
            }
            SshAuth::Key {
                private_key_path,
                passphrase,
            } => {
                let expanded_path = if private_key_path.starts_with("~/") {
                    if let Some(home) = dirs::home_dir() {
                        home.join(&private_key_path[2..])
                            .to_string_lossy()
                            .to_string()
                    } else {
                        private_key_path.clone()
                    }
                } else {
                    private_key_path.clone()
                };
                let key_data = match std::fs::read_to_string(&expanded_path) {
                    Ok(s) => s,
                    Err(e) => {
                        anyhow::bail!("Failed to read private key '{}': {}", expanded_path, e);
                    }
                };
                let key = match russh::keys::ssh_key::PrivateKey::from_openssh(&key_data) {
                    Ok(k) => k,
                    Err(e) => {
                        anyhow::bail!("Failed to parse private key '{}': {}", expanded_path, e);
                    }
                };
                let key = if let Some(pass) = passphrase {
                    match key.decrypt(pass.as_bytes()) {
                        Ok(k) => k,
                        Err(_) => {
                            anyhow::bail!(
                                "Failed to decrypt private key '{}' — wrong passphrase?",
                                expanded_path
                            );
                        }
                    }
                } else {
                    key
                };

                let best_rsa_hash = handle.best_supported_rsa_hash().await?;
                let hash_alg = best_rsa_hash.flatten();

                let key_with_alg = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
                let result = handle
                    .authenticate_publickey(&self.config.username, key_with_alg)
                    .await?;
                if !matches!(result, client::AuthResult::Success) {
                    anyhow::bail!(
                        "SSH public-key authentication failed (result: {:?})",
                        result
                    );
                }
            }
            SshAuth::Agent => {
                anyhow::bail!("SSH agent auth not yet supported");
            }
        }

        let handle = Arc::new(handle);

        let channel = handle.channel_open_session().await?;

        channel
            .request_pty(
                false,
                self.config.terminal_type.as_str(),
                size.cols as u32,
                size.rows as u32,
                0,
                0,
                &[
                    // Standard cooked-terminal modes (what OpenSSH sends). The
                    // remote sshd applies these to the PTY. ICRNL maps Enter's
                    // \r to \n so line-oriented programs (shells, `read`,
                    // interactive menus) see a newline — without it some servers
                    // leave ICRNL off and \r doesn't terminate input, so the
                    // program hangs / re-prompts (e.g. a login-account menu
                    // ignoring a typed "2"). OPOST+ONLCR make output \n→\r\n.
                    (Pty::ECHO, 1),
                    (Pty::ICANON, 1),
                    (Pty::ISIG, 1),
                    (Pty::IEXTEN, 1),
                    (Pty::ICRNL, 1),
                    (Pty::OPOST, 1),
                    (Pty::ONLCR, 1),
                    (Pty::ECHOE, 1),
                    (Pty::ECHOK, 1),
                    (Pty::ECHOCTL, 1),
                    (Pty::ECHOKE, 1),
                ],
            )
            .await?;

        channel.request_shell(true).await?;

        let (read_half, write_half) = channel.split();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            self.config.host.clone(),
            SessionType::Ssh,
            input_tx,
            resize_tx,
            close_tx,
        );

        // Shared guard: only one task may send Disconnected
        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Output reader: forward data from SSH channel to event channel
        let sid_read = session_id.clone();
        let evt_read = self.event_tx.clone();
        let disconnected_read = disconnected.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            // Track the last Data message to dedup against ExtendedData.
            // Some SSH servers echo the same content via both Data and
            // ExtendedData channels; we skip the ExtendedData duplicate.
            // We do NOT dedup between consecutive Data messages, as that
            // would drop legitimate identical output (e.g., repeated prompts
            // after Ctrl+C) that contains critical \r\n sequences.
            let mut last_data: Option<Vec<u8>> = None;

            while let Some(msg) = reader.wait().await {
                let bytes = match msg {
                    ChannelMsg::Data { data } => {
                        let bytes = data.to_vec();
                        last_data = Some(bytes.clone());
                        bytes
                    }
                    ChannelMsg::ExtendedData { data, ext } => {
                        if ext != 1 {
                            continue;
                        }
                        let bytes = data.to_vec();
                        // Skip if identical to the last Data message
                        if last_data.as_ref() == Some(&bytes) {
                            tracing::debug!(
                                "[SSH] skipping ExtendedData duplicate of Data ({} bytes)",
                                bytes.len()
                            );
                            continue;
                        }
                        bytes
                    }
                    ChannelMsg::Eof => {
                        continue;
                    }
                    ChannelMsg::Close => break,
                    _ => continue,
                };

                let _ = evt_read.send(SessionEvent::Output(sid_read.clone(), bytes));
            }
            if disconnected_read
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                let _ = evt_read.send(SessionEvent::Disconnected(
                    sid_read,
                    "Session closed".to_string(),
                ));
            }
        });

        // Input/resize/close writer
        let sid_write = session_id.clone();
        let evt_write = self.event_tx.clone();
        let disconnected_write = disconnected.clone();
        tokio::spawn(async move {
            let mut consecutive_errors = 0u32;
            loop {
                tokio::select! {
                    Some(data) = input_rx.recv() => {
                        if let Err(e) = write_half.data_bytes(data).await {
                            consecutive_errors += 1;
                            tracing::warn!("[SSH] write failed for {} (attempt {consecutive_errors}): {e}", &sid_write[..sid_write.len().min(8)]);
                            if consecutive_errors >= 3 {
                                tracing::error!("[SSH] too many write errors for {}, closing input writer", &sid_write[..sid_write.len().min(8)]);
                                break;
                            }
                            // Brief pause before retrying to avoid tight error loop
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        } else {
                            consecutive_errors = 0;
                        }
                    }
                    Some((cols, rows, pw, ph)) = resize_rx.recv() => {
                        if let Err(e) = write_half.window_change(cols as u32, rows as u32, pw, ph).await {
                            tracing::warn!("[SSH] window_change failed for {}: {e}", &sid_write[..sid_write.len().min(8)]);
                            // Don't break on resize failure — it's not critical
                        }
                    }
                    Some(_) = close_rx.recv() => {
                        let _ = write_half.eof().await;
                        break;
                    }
                    else => break,
                }
            }
            if disconnected_write
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                let _ = evt_write.send(SessionEvent::Disconnected(
                    sid_write,
                    "Session closed".to_string(),
                ));
            }
        });

        let _ = self
            .event_tx
            .send(SessionEvent::Connected(session_id.clone()));

        Ok((
            session,
            SshSession {
                handle,
                session_id,
                event_tx: self.event_tx.clone(),
                disconnected,
            },
        ))
    }
}

type Handle = client::Handle<Handler>;

#[derive(Clone)]
pub struct SshSession {
    handle: Arc<Handle>,
    session_id: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    disconnected: Arc<std::sync::atomic::AtomicBool>,
}

impl SshSession {
    pub async fn disconnect(&self) -> anyhow::Result<()> {
        if self
            .disconnected
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            self.handle
                .disconnect(russh::Disconnect::AuthCancelledByUser, "Bye", "")
                .await?;
            let _ = self.event_tx.send(SessionEvent::Disconnected(
                self.session_id.clone(),
                "User disconnected".to_string(),
            ));
        }
        Ok(())
    }

    /// Fetch remote shell history. Tries exec channel first, then falls back
    /// to a shell channel for restricted servers (jump servers / bastion hosts
    /// that block exec requests).
    pub async fn fetch_remote_history(&self) -> anyhow::Result<Vec<String>> {
        match self.fetch_via_exec().await {
            Ok(cmds) => return Ok(cmds),
            Err(e) => {
                tracing::warn!("[SSH] Exec channel failed ({}), trying shell fallback", e);
            }
        }
        self.fetch_via_shell().await
    }

    /// Try to fetch history via an exec channel (fast, non-interactive).
    async fn fetch_via_exec(&self) -> anyhow::Result<Vec<String>> {
        tracing::info!("[SSH] Opening exec channel to fetch remote history");
        let channel = self.handle.channel_open_session().await?;

        let cmd = r#"
if [ -f ~/.bash_history ]; then tail -5000 ~/.bash_history 2>/dev/null; fi
if [ -f ~/.zsh_history ]; then tail -5000 ~/.zsh_history 2>/dev/null; fi
if [ -f ~/.local/share/fish/fish_history ]; then head -5000 ~/.local/share/fish/fish_history 2>/dev/null; fi
"#;
        channel.exec(true, cmd).await?;

        let mut output = Vec::new();
        let mut reader = channel;

        loop {
            match reader.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    output.extend_from_slice(&data);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
            if output.len() > 5 * 1024 * 1024 {
                break;
            }
        }

        let raw = String::from_utf8_lossy(&output);

        // Detect exec channel rejection (jump servers / bastion hosts).
        if raw.contains("exec request failed")
            || raw.contains("try username/server/account")
            || raw.contains("command not allowed")
        {
            anyhow::bail!("exec channel rejected by server");
        }

        let parsed = parse_remote_history(&raw);
        tracing::info!(
            "[SSH] Exec: parsed {} unique remote history commands",
            parsed.len()
        );
        Ok(parsed)
    }

    /// Fallback: fetch history via a shell channel. Opens a second session,
    /// requests a shell, sends a command with markers, and captures the output
    /// between the markers. Works on servers that block exec but allow shell.
    async fn fetch_via_shell(&self) -> anyhow::Result<Vec<String>> {
        tracing::info!("[SSH] Opening shell channel (fallback) to fetch remote history");
        let channel = self.handle.channel_open_session().await?;
        channel.request_shell(true).await?;

        // Wait for the shell to start, then send the command with markers.
        // The markers let us extract just the history content, skipping the
        // prompt and command echo.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let cmd = "echo __RUSTERM_HIST_START__; tail -5000 ~/.bash_history 2>/dev/null; tail -5000 ~/.zsh_history 2>/dev/null; head -5000 ~/.local/share/fish/fish_history 2>/dev/null; echo __RUSTERM_HIST_END__; exit\n";
        channel.data(cmd.as_bytes()).await?;

        let mut output = Vec::new();
        let mut reader = channel;

        loop {
            match reader.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    output.extend_from_slice(&data);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
            if output.len() > 5 * 1024 * 1024 {
                break;
            }
        }

        let raw = String::from_utf8_lossy(&output);
        tracing::info!("[SSH] Shell fallback raw output: {} bytes", output.len());

        // Extract content between markers to skip prompt + command echo
        let start_marker = "__RUSTERM_HIST_START__";
        let end_marker = "__RUSTERM_HIST_END__";
        let extracted: String = {
            let raw_str = raw.as_ref();
            if let (Some(start), Some(end)) = (raw_str.find(start_marker), raw_str.find(end_marker))
            {
                if end > start {
                    raw_str[start + start_marker.len()..end].to_string()
                } else {
                    raw_str.to_string()
                }
            } else {
                // Markers not found — use raw output (might include prompt noise)
                raw_str.to_string()
            }
        };

        let parsed = parse_remote_history(&extracted);
        tracing::info!(
            "[SSH] Shell fallback: parsed {} unique remote history commands",
            parsed.len()
        );
        Ok(parsed)
    }
}

/// Parse remote shell history output (bash + zsh + fish formats).
pub fn parse_remote_history(raw: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut current_cmd = String::new();
    let mut in_zsh_entry = false;

    for line in raw.lines() {
        let trimmed = line.trim();

        // Fish: `- cmd: command`
        if let Some(cmd) = trimmed.strip_prefix("- cmd:") {
            flush_cmd(&mut current_cmd, &mut seen, &mut commands);
            in_zsh_entry = false;
            let c = cmd.trim().to_string();
            if !c.is_empty() && seen.insert(c.clone()) {
                commands.push(c);
            }
            continue;
        }

        // Fish metadata
        if trimmed.starts_with("when:")
            || trimmed.starts_with("paths:")
            || trimmed.starts_with("  - /")
        {
            continue;
        }

        // zsh extended: `: timestamp:duration;command`
        if trimmed.starts_with(':') {
            flush_cmd(&mut current_cmd, &mut seen, &mut commands);
            if let Some(rest) = trimmed.strip_prefix(':') {
                if let Some(semicolon_pos) = rest.find(';') {
                    let cmd = &rest[semicolon_pos + 1..];
                    if !cmd.is_empty() {
                        current_cmd = cmd.to_string();
                        in_zsh_entry = true;
                    }
                } else {
                    current_cmd = rest.to_string();
                    in_zsh_entry = true;
                }
            }
            continue;
        }

        // Multi-line zsh continuation
        if in_zsh_entry && !current_cmd.is_empty() {
            current_cmd.push('\n');
            current_cmd.push_str(line);
            continue;
        }

        // Plain bash line
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let c = trimmed.to_string();
            if seen.insert(c.clone()) {
                commands.push(c);
            }
        }
    }

    flush_cmd(&mut current_cmd, &mut seen, &mut commands);
    commands
}

fn flush_cmd(
    current: &mut String,
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) {
    if !current.is_empty() {
        let c = current.trim().to_string();
        if !c.is_empty() && seen.insert(c.clone()) {
            out.push(c);
        }
        current.clear();
    }
}
