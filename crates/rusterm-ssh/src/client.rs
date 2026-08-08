use std::sync::Arc;

use russh::client;
use russh::{ChannelMsg, Pty};
use tokio::sync::mpsc;

use rusterm_core::config::{SshAuth, SshConfig};
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};
use rusterm_core::terminal::TerminalSize;

use crate::otp::OtpProvider;

const INTERACTIVE_PTY_MODES: &[(Pty, u32)] = &[
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
];

use crate::known_hosts::{HostKeyPolicy, verify_server_key};
use crate::sftp::{SftpClient, map_sftp_error, map_ssh_error};
use crate::transport::connect_transport;

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
    otp_provider: OtpProvider,
}

impl SshClient {
    pub fn new(config: SshConfig, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self {
            config,
            event_tx,
            otp_provider: OtpProvider::Manual,
        }
    }

    /// Replace the OTP / MFA code provider used during keyboard-interactive
    /// authentication. Defaults to [`OtpProvider::Manual`]. Call this before
    /// [`connect`](Self::connect) with a provider built from the user's
    /// `otp_webhook` setting to enable automatic OTP auto-fill.
    pub fn with_otp_provider(mut self, provider: OtpProvider) -> Self {
        self.otp_provider = provider;
        self
    }

    /// Mutable setter for the OTP provider — used when the app updates the
    /// setting after the client has been constructed (e.g. the user edits
    /// the webhook config while a reconnect is queued).
    pub fn set_otp_provider(&mut self, provider: OtpProvider) {
        self.otp_provider = provider;
    }

    pub async fn connect(
        &self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> anyhow::Result<(Session, SshSession)> {
        let config = Arc::new(interactive_client_config());

        let handle = connect_authenticated(&self.config, config, self.otp_provider.clone()).await?;

        spawn_interactive_channel(
            Arc::new(handle),
            self.config.terminal_type.as_str(),
            self.config.host.clone(),
            self.event_tx.clone(),
            session_id,
            size,
        )
        .await
    }
}

/// Open an interactive shell channel (PTY + shell + IO forwarder tasks) on an
/// already-authenticated transport. Shared by [`SshClient::connect`] (fresh
/// transport + auth) and [`SshSession::clone_channel`] (a new terminal tab on
/// the same transport — used to duplicate JumpServer/koko sessions without a
/// second login or OTP round-trip).
async fn spawn_interactive_channel(
    handle: Arc<Handle>,
    terminal_type: &str,
    host: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    session_id: SessionId,
    size: TerminalSize,
) -> anyhow::Result<(Session, SshSession)> {
    {
        let channel = handle.channel_open_session().await?;

        channel
            .request_pty(
                false,
                terminal_type,
                size.cols as u32,
                size.rows as u32,
                0,
                0,
                // Standard cooked-terminal modes (what OpenSSH sends). ICRNL
                // makes Enter work for shells and bastion menus; OPOST+ONLCR
                // preserves normal terminal output line endings.
                INTERACTIVE_PTY_MODES,
            )
            .await?;

        channel.request_shell(true).await?;

        let (read_half, write_half) = channel.split();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            host.clone(),
            SessionType::Ssh,
            input_tx,
            resize_tx,
            close_tx,
        );

        // Shared guard: only one task may send Disconnected
        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Optional output tap for relay exec reuse.
        let output_tap: Arc<parking_lot::RwLock<Option<mpsc::UnboundedSender<Vec<u8>>>>> =
            Arc::new(parking_lot::RwLock::new(None));
        let output_tap_lock = Arc::new(tokio::sync::Mutex::new(()));
        let relay_exec_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let relay_exec_reusable = Arc::new(std::sync::atomic::AtomicBool::new(true));

        // Output reader: forward data from SSH channel to event channel
        let sid_read = session_id.clone();
        let evt_read = event_tx.clone();
        let disconnected_read = disconnected.clone();
        let tap_read = output_tap.clone();
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

                let _ = evt_read.send(SessionEvent::Output(sid_read.clone(), bytes.clone()));

                // Forward to output tap if one is installed (relay exec
                // reuse path). Cloning the bytes is cheap compared to the
                // network round-trip and keeps the UI display unaffected.
                if let Some(tap) = tap_read.read().as_ref() {
                    let _ = tap.send(bytes);
                }
            }
            // Close any active relay tap as soon as the SSH output stream
            // ends. Otherwise the receiver remains open through the sender
            // stored in `output_tap` and waits until its full request timeout.
            *tap_read.write() = None;
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
        let evt_write = event_tx.clone();
        let disconnected_write = disconnected.clone();
        tokio::spawn(async move {
            let mut disconnect_reason = "Session closed".to_string();
            loop {
                tokio::select! {
                    Some(data) = input_rx.recv() => {
                        let payload_len = data.len();
                        let ends_with_cr = data.last() == Some(&b'\r');
                        let sid_short = &sid_write[..sid_write.len().min(8)];
                        if let Err(e) = write_half.data_bytes(data).await {
                            // A failed SSH channel write is definitive for this
                            // interactive channel. Keeping the UI Connected would
                            // accept and lose later keystrokes, which looks like a
                            // locally echoed command that never runs remotely.
                            disconnect_reason = format!("SSH input failed: {e}");
                            tracing::error!(
                                "[SSH] write failed for {} payload_len={} ends_with_cr={}: {e}",
                                sid_short,
                                payload_len,
                                ends_with_cr
                            );
                            break;
                        } else {
                            // trace (not info): this fires on every keystroke,
                            // and `info`-level logging of every key would
                            // dominate the trace output during fast typing
                            // and add measurable overhead in debug builds.
                            tracing::trace!(
                                "[SSH] write succeeded for {} payload_len={} ends_with_cr={}",
                                sid_short,
                                payload_len,
                                ends_with_cr
                            );
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
                let _ = evt_write.send(SessionEvent::Disconnected(sid_write, disconnect_reason));
            }
        });

        let _ = event_tx.send(SessionEvent::Connected(session_id.clone()));

        Ok((
            session,
            SshSession {
                handle,
                session_id,
                host,
                event_tx,
                disconnected,
                output_tap,
                output_tap_lock,
                relay_exec_active,
                relay_exec_reusable,
            },
        ))
    }
}

type Handle = client::Handle<Handler>;

/// Default keepalive interval for non-interactive connections (tunnels,
/// relays). Keeps NAT/firewall state alive and lets russh detect dead
/// connections by dropping the connection after `keepalive_max`
/// unanswered keepalives.
pub const DEFAULT_KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
pub const DEFAULT_KEEPALIVE_MAX: usize = 3;

/// Build a `russh::client::Config` with application-layer keepalives
/// configured. Used by both interactive sessions and background
/// (tunnel/relay) connections.
pub fn client_config(
    keepalive_interval: Option<std::time::Duration>,
    keepalive_max: usize,
) -> client::Config {
    let mut config = client::Config::default();
    config.keepalive_interval = keepalive_interval;
    config.keepalive_max = keepalive_max;
    config
}

fn interactive_client_config() -> client::Config {
    client_config(Some(DEFAULT_KEEPALIVE_INTERVAL), DEFAULT_KEEPALIVE_MAX)
}

/// Establish transport + SSH handshake + user authentication, returning a
/// ready-to-use russh client handle. Shared by [`SshClient::connect`]
/// (interactive PTY sessions) and background consumers (tunnels, REST
/// relay) that only need exec/direct-tcpip channels.
///
/// `otp_provider` supplies MFA codes when the server issues an OTP prompt
/// during keyboard-interactive auth. Pass [`OtpProvider::Manual`] to disable
/// auto-fetch and let the server reject the auth (the interactive UI owns
/// the manual-entry fallback path through the OneKey credential popup).
pub async fn connect_authenticated(
    config: &SshConfig,
    client_config: Arc<client::Config>,
    otp_provider: OtpProvider,
) -> anyhow::Result<client::Handle<Handler>> {
    // Derive the host-key verification policy from the user's config.
    // Unknown / empty values fall back to AcceptNew (TOFU) inside
    // `HostKeyPolicy::parse`.
    let policy = HostKeyPolicy::parse(&config.host_key_policy);
    let handler = Handler::new(config.host.clone(), policy);

    let stream = connect_transport(&config.host, config.port, config.proxy.as_ref()).await?;
    let mut handle = client::connect_stream(client_config, stream, handler).await?;

    authenticate(&mut handle, config, &otp_provider).await?;
    Ok(handle)
}

/// Authenticate a freshly-connected handle according to `config.auth`.
/// Password auth falls back to keyboard-interactive (matches OpenSSH).
async fn authenticate(
    handle: &mut client::Handle<Handler>,
    config: &SshConfig,
    otp_provider: &OtpProvider,
) -> anyhow::Result<()> {
    match &config.auth {
        SshAuth::Password { password } => {
            let result = handle
                .authenticate_password(&config.username, password.as_str())
                .await?;
            if !matches!(result, client::AuthResult::Success) {
                // Some servers (notably jump hosts / bastions and PAM-configured
                // Linux boxes) reject "password" auth but accept the same
                // credential via "keyboard-interactive". Try it as a fallback
                // before giving up — this is what OpenSSH's ssh client does too.
                tracing::info!(
                    "[SSH] password auth returned {:?}, trying keyboard-interactive fallback for {}@{}",
                    result,
                    config.username,
                    config.host
                );
                let ki_result =
                    auth_keyboard_interactive(handle, &config.username, password, otp_provider)
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
                .authenticate_publickey(&config.username, key_with_alg)
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
    Ok(())
}

/// Drive a keyboard-interactive auth exchange to completion using `password`
/// as the answer to every prompt. This is the standard "PAM password via
/// keyboard-interactive" pattern: the server sends one prompt (e.g.
/// "Password: ") and we reply with the password. Loops because some servers
/// send multiple (empty) prompts before the real one.
///
/// When an OTP / MFA prompt is detected (see [`looks_like_otp_prompt`]) and
/// `otp_provider` is an automatic provider, the code is fetched and submitted
/// instead of the password. If the provider returns `None` (e.g. `Manual` or
/// no fresh code available), the password is sent as before so the server
/// can reject it explicitly — the interactive UI then surfaces the prompt
/// through the OneKey credential popup for manual entry.
async fn auth_keyboard_interactive(
    handle: &mut client::Handle<Handler>,
    username: &str,
    password: &str,
    otp_provider: &OtpProvider,
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
                // Reply to each prompt. Most PAM keyboard-interactive flows use
                // a single "Password:" prompt — we answer with the password.
                //
                // OTP / MFA prompts (sent as a second factor by JumpServer and
                // other bastions) are answered with a code fetched from the
                // configured webhook provider. If no automatic provider is
                // configured (or it returns no code), we send the password so
                // the server rejects the auth explicitly and the interactive
                // UI can surface a manual OTP prompt through the OneKey popup.
                let mut answers: Vec<String> = Vec::with_capacity(prompts.len());
                for p in &prompts {
                    if looks_like_otp_prompt(&p.prompt) {
                        match otp_provider.fetch_code().await {
                            Ok(Some(code)) => {
                                tracing::info!(
                                    "[SSH] OTP prompt {:?} answered via webhook provider",
                                    p.prompt
                                );
                                answers.push(code);
                                continue;
                            }
                            Ok(None) => {
                                tracing::info!(
                                    "[SSH] OTP prompt {:?} but provider returned no code; falling back to password",
                                    p.prompt
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "[SSH] OTP provider fetch failed for prompt {:?}: {}",
                                    p.prompt,
                                    e
                                );
                            }
                        }
                    }
                    answers.push(password.to_string());
                }
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

/// Heuristic to decide whether a keyboard-interactive prompt is asking for
/// an OTP / MFA code rather than a password. JumpServer's MFA prompt is
/// typically `"MFA code:"` or `"请输入MFA认证码"`; other bastions use
/// `"OTP:"`, `"Verification code:"`, etc. We match case-insensitively on
/// the common keywords. A `false` result means "treat it as a password
/// prompt" (the safe default — sending the password to an OTP prompt is
/// harmless because the server will reject it).
/// Must stay in sync with the ssh auth-path gate at `SshClient::connect`:
/// OTP markers short-circuit to the provider; anything else falls through
/// to the normal Password flow. `pub` so the UI tty watcher (issue #129)
/// reuses the exact same marker list instead of drifting copies.
pub fn looks_like_otp_prompt(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    // Strong OTP markers — a prompt containing any of these is an OTP prompt.
    const OTP_MARKERS: &[&str] = &[
        "otp",
        "mfa",
        "totp",
        "verification code",
        "verify code",
        "authenticator",
        "2fa",
        "two-factor",
        "two factor",
        "2nd password",
        "second password",
        "2nd pwd",
        "second pwd",
        "二次验证",
        "动态密码",
        "验证码",
        "认证码",
        "第二密码",
        "二次密码",
    ];
    OTP_MARKERS.iter().any(|m| lower.contains(m))
}

#[derive(Debug, PartialEq, Eq)]
enum SubsystemReplyAction {
    Ready,
    ContinueWaiting,
    Error(crate::sftp::SftpError),
}

fn classify_subsystem_reply(reply: Option<ChannelMsg>) -> SubsystemReplyAction {
    match reply {
        Some(ChannelMsg::Success) => SubsystemReplyAction::Ready,
        Some(ChannelMsg::Failure) => {
            SubsystemReplyAction::Error(crate::sftp::SftpError::SubsystemRejected)
        }
        Some(ChannelMsg::WindowAdjusted { .. }) => SubsystemReplyAction::ContinueWaiting,
        Some(ChannelMsg::Close | ChannelMsg::Eof) | None => {
            SubsystemReplyAction::Error(crate::sftp::SftpError::ChannelClosed)
        }
        Some(other) => SubsystemReplyAction::Error(
            crate::sftp::SftpError::UnexpectedSubsystemReply(format!("{other:?}")),
        ),
    }
}

/// Exclusive capture of an interactive session's output for one relay
/// command. Holding this guard serializes relay commands per SSH session;
/// dropping it removes the tap on every exit path.
pub struct OutputTapGuard {
    session: SshSession,
    receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl OutputTapGuard {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.receiver.recv().await
    }
}

impl Drop for OutputTapGuard {
    fn drop(&mut self) {
        self.session.remove_output_tap();
        self.session
            .relay_exec_active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Clone)]
pub struct SshSession {
    handle: Arc<Handle>,
    session_id: String,
    /// Remote host name (used to label duplicated terminal sessions).
    host: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
    disconnected: Arc<std::sync::atomic::AtomicBool>,
    /// Optional output tap installed by the relay executor to capture
    /// command output from the live interactive PTY. When `Some`, the
    /// output reader task forwards every data chunk here in addition to
    /// the normal UI event channel.
    output_tap: Arc<parking_lot::RwLock<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Serializes relay commands that share this interactive PTY.
    output_tap_lock: Arc<tokio::sync::Mutex<()>>,
    /// Lets the UI reject manual keystrokes while a relay transaction owns
    /// the PTY, preventing a partially typed line from mixing with the API
    /// command.
    relay_exec_active: Arc<std::sync::atomic::AtomicBool>,
    /// Cleared when a sent command finishes without a sentinel. The shell's
    /// state is then unknown, so later API requests must not inject another
    /// command into this PTY.
    relay_exec_reusable: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for SshSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SshSession")
            .field("session_id", &self.session_id)
            .field(
                "disconnected",
                &self.disconnected.load(std::sync::atomic::Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl SshSession {
    pub async fn open_sftp(&self) -> Result<SftpClient, crate::sftp::SftpError> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(map_ssh_error)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(map_ssh_error)?;

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                match classify_subsystem_reply(channel.wait().await) {
                    SubsystemReplyAction::Ready => return Ok(()),
                    SubsystemReplyAction::ContinueWaiting => {}
                    SubsystemReplyAction::Error(error) => return Err(error),
                }
            }
        })
        .await
        .map_err(|_| crate::sftp::SftpError::Timeout)??;

        let session = russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(map_sftp_error)?;
        Ok(SftpClient::new(session))
    }

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

    /// The session/tab id used as the key in `ssh_sessions`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// True once this session's interactive channel has ended (closed by the
    /// peer, a write failure, or an explicit disconnect). A disconnected
    /// session cannot serve as the source of a channel clone.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Duplicate this session onto a fresh interactive channel of the SAME
    /// authenticated transport — no new TCP connection, handshake, login or
    /// OTP is performed. This is the fast path behind "copy session" for
    /// interactive-bastion (JumpServer/koko) tabs: the bastion sees a second
    /// shell channel on the already-logged-in connection and drops the user
    /// back at its main menu, from which RusTerm replays the recorded
    /// establishment ops to land on the same target host.
    ///
    /// The clone gets its own `Session` (input/resize/close senders) and its
    /// own `SessionEvent` stream (the caller's `event_tx`), fully independent
    /// of the source tab's channel: closing either tab only closes its own
    /// channel; the shared transport dies with the last `SshSession` handle.
    pub async fn clone_channel(
        &self,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
        new_session_id: SessionId,
        size: TerminalSize,
        terminal_type: &str,
    ) -> anyhow::Result<(Session, SshSession)> {
        if self.is_disconnected() {
            anyhow::bail!("cannot clone a channel from a disconnected SSH session");
        }
        spawn_interactive_channel(
            self.handle.clone(),
            terminal_type,
            self.host.clone(),
            event_tx,
            new_session_id,
            size,
        )
        .await
    }

    /// Begin one exclusive relay transaction on this interactive PTY.
    /// Concurrent relay calls wait for the previous guard to drop rather
    /// than replacing its tap and stealing its output.
    pub async fn begin_output_tap(&self) -> anyhow::Result<OutputTapGuard> {
        let lock = self.output_tap_lock.clone().lock_owned().await;
        if self.disconnected.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("SSH session is disconnected");
        }
        if !self
            .relay_exec_reusable
            .load(std::sync::atomic::Ordering::Acquire)
        {
            anyhow::bail!("SSH session relay state is unknown after an incomplete command");
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        *self.output_tap.write() = Some(sender);
        self.relay_exec_active
            .store(true, std::sync::atomic::Ordering::Release);

        // Close the small race where the reader ended after the first check
        // but before the tap was installed.
        if self.disconnected.load(std::sync::atomic::Ordering::Acquire) {
            self.remove_output_tap();
            self.relay_exec_active
                .store(false, std::sync::atomic::Ordering::Release);
            anyhow::bail!("SSH session disconnected while installing output tap");
        }

        Ok(OutputTapGuard {
            session: self.clone(),
            receiver,
            _lock: lock,
        })
    }

    fn remove_output_tap(&self) {
        *self.output_tap.write() = None;
    }

    /// True while a relay command owns this PTY. UI input should be held
    /// back during this brief interval to avoid mixing two command lines.
    pub fn relay_exec_active(&self) -> bool {
        self.relay_exec_active
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Prevent future API commands from reusing this PTY after a command was
    /// sent but its completion marker could not be observed.
    pub fn mark_relay_exec_unusable(&self) {
        self.relay_exec_reusable
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Fetch remote shell history. Tries exec channel first, then falls back
    /// to a shell channel for restricted servers (jump servers / bastion hosts
    /// that block exec requests).
    pub async fn fetch_remote_history(&self) -> anyhow::Result<Vec<String>> {
        const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

        match tokio::time::timeout(ATTEMPT_TIMEOUT, self.fetch_via_exec()).await {
            Ok(Ok(cmds)) => return Ok(cmds),
            Ok(Err(error)) => {
                tracing::warn!("[SSH] Exec channel failed ({error}), trying PTY shell fallback");
            }
            Err(_) => {
                tracing::warn!("[SSH] Exec history import timed out, trying PTY shell fallback");
            }
        }

        tokio::time::timeout(ATTEMPT_TIMEOUT, self.fetch_via_shell())
            .await
            .map_err(|_| anyhow::anyhow!("PTY shell history import timed out"))?
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
        channel
            .request_pty(false, "xterm-256color", 80, 24, 0, 0, INTERACTIVE_PTY_MODES)
            .await?;
        channel.request_shell(true).await?;

        // Wait for the shell to start, then send the command with markers.
        // The markers let us extract just the history content, skipping the
        // prompt and command echo.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let cmd = "echo __RUSTERM_HIST_START__; tail -5000 ~/.bash_history 2>/dev/null; tail -5000 ~/.zsh_history 2>/dev/null; head -5000 ~/.local/share/fish/fish_history 2>/dev/null; echo __RUSTERM_HIST_END__; exit\r";
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

#[cfg(test)]
mod history_shell_tests {
    use std::sync::{Arc, Mutex};

    use russh::server;
    use russh::{Channel, ChannelId};
    use tokio::net::TcpListener;

    use super::*;

    const TEST_SERVER_KEY: &str = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDNotGW1Df1gWTlX1nm2z14o5dyI2dMk3NF8fWwunRfIAAAAKCLYE7bi2BO
2wAAAAtzc2gtZWQyNTUxOQAAACDNotGW1Df1gWTlX1nm2z14o5dyI2dMk3NF8fWwunRfIA
AAAEBRCDM1Phz340R2RR59Pc8j0B6x5FdNCpdW03IjTg3A6s2i0ZbUN/WBZOVfWebbPXij
l3IjZ0yTc0Xx9bC6dF8gAAAAG2FyZXNuYXNhQEZyYW5rcy1NNU1heC5sb2NhbAEC
-----END OPENSSH PRIVATE KEY-----"#;

    #[derive(Clone)]
    struct PtyRequiredHistoryServer {
        events: Arc<Mutex<Vec<&'static str>>>,
        pty_requested: bool,
    }

    impl server::Handler for PtyRequiredHistoryServer {
        type Error = russh::Error;

        async fn auth_none(&mut self, _user: &str) -> Result<server::Auth, Self::Error> {
            Ok(server::Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _channel: Channel<server::Msg>,
            _session: &mut server::Session,
        ) -> Result<bool, Self::Error> {
            self.events.lock().unwrap().push("open");
            Ok(true)
        }

        async fn exec_request(
            &mut self,
            channel: ChannelId,
            _command: &[u8],
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            self.events.lock().unwrap().push("exec-rejected");
            session.channel_success(channel)?;
            session.data(
                channel,
                b"exec request failed, try username/server/account as login name.\n".to_vec(),
            )?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }

        async fn pty_request(
            &mut self,
            channel: ChannelId,
            term: &str,
            cols: u32,
            rows: u32,
            _pixel_width: u32,
            _pixel_height: u32,
            modes: &[(Pty, u32)],
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            assert_eq!(term, "xterm-256color");
            assert!(cols > 0 && rows > 0);
            assert!(modes.contains(&(Pty::ICRNL, 1)));
            self.events.lock().unwrap().push("pty");
            self.pty_requested = true;
            session.channel_success(channel)?;
            Ok(())
        }

        async fn shell_request(
            &mut self,
            channel: ChannelId,
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            self.events.lock().unwrap().push("shell");
            if self.pty_requested {
                session.channel_success(channel)?;
            } else {
                session.channel_failure(channel)?;
            }
            Ok(())
        }

        async fn data(
            &mut self,
            channel: ChannelId,
            _data: &[u8],
            session: &mut server::Session,
        ) -> Result<(), Self::Error> {
            self.events.lock().unwrap().push("data");
            session.data(
                channel,
                b"__RUSTERM_HIST_START__\necho from-history\n__RUSTERM_HIST_END__\n".to_vec(),
            )?;
            session.eof(channel)?;
            session.close(channel)?;
            Ok(())
        }
    }

    #[test]
    fn interactive_sessions_enable_keepalive_detection() {
        let config = interactive_client_config();

        assert_eq!(config.keepalive_interval, Some(DEFAULT_KEEPALIVE_INTERVAL));
        assert_eq!(config.keepalive_max, DEFAULT_KEEPALIVE_MAX);
    }

    #[tokio::test]
    async fn history_shell_requests_pty_before_starting_restricted_shell() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let server_handler = PtyRequiredHistoryServer {
            events: events.clone(),
            pty_requested: false,
        };

        let mut server_config = server::Config::default();
        server_config.auth_rejection_time = std::time::Duration::ZERO;
        server_config
            .keys
            .push(russh::keys::ssh_key::PrivateKey::from_openssh(TEST_SERVER_KEY).unwrap());
        let server_config = Arc::new(server_config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            server::run_stream(server_config, socket, server_handler)
                .await
                .unwrap();
        });

        let client_handler = Handler::new(address.ip().to_string(), HostKeyPolicy::Disabled);
        let mut handle =
            client::connect(Arc::new(client::Config::default()), address, client_handler)
                .await
                .unwrap();
        assert!(
            handle
                .authenticate_none("test-user")
                .await
                .unwrap()
                .success()
        );

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let ssh_session = SshSession {
            handle: Arc::new(handle),
            session_id: "history-test".to_string(),
            host: "127.0.0.1".to_string(),
            event_tx,
            disconnected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            output_tap: Arc::new(parking_lot::RwLock::new(None)),
            output_tap_lock: Arc::new(tokio::sync::Mutex::new(())),
            relay_exec_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            relay_exec_reusable: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };

        let history = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ssh_session.fetch_remote_history(),
        )
        .await
        .expect("history shell timed out")
        .expect("PTY-requiring history shell failed");

        assert_eq!(history, vec!["echo from-history"]);
        assert_eq!(
            *events.lock().unwrap(),
            ["open", "exec-rejected", "open", "pty", "shell", "data"]
        );

        let first_tap = ssh_session.begin_output_tap().await.unwrap();
        assert!(ssh_session.relay_exec_active());
        let second_session = ssh_session.clone();
        let second_tap =
            tokio::spawn(async move { second_session.begin_output_tap().await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !second_tap.is_finished(),
            "concurrent tap replaced the active tap"
        );
        drop(first_tap);
        let second_tap = tokio::time::timeout(std::time::Duration::from_secs(1), second_tap)
            .await
            .expect("second tap did not acquire the session lock")
            .unwrap();
        assert!(ssh_session.relay_exec_active());
        drop(second_tap);
        assert!(!ssh_session.relay_exec_active());

        ssh_session
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await
            .unwrap();
        server_task.await.unwrap();
    }

    /// Channel cloning: a second interactive shell channel opens on the same
    /// authenticated transport — no new TCP connect, no auth — and the clone
    /// reports its own session id while the source stays connected.
    #[tokio::test]
    async fn clone_channel_reuses_authenticated_transport_for_new_interactive_shell() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let server_handler = PtyRequiredHistoryServer {
            events: events.clone(),
            pty_requested: false,
        };

        let mut server_config = server::Config::default();
        server_config.auth_rejection_time = std::time::Duration::ZERO;
        server_config
            .keys
            .push(russh::keys::ssh_key::PrivateKey::from_openssh(TEST_SERVER_KEY).unwrap());
        let server_config = Arc::new(server_config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            server::run_stream(server_config, socket, server_handler)
                .await
                .unwrap();
        });

        let client_handler = Handler::new(address.ip().to_string(), HostKeyPolicy::Disabled);
        let mut handle =
            client::connect(Arc::new(client::Config::default()), address, client_handler)
                .await
                .unwrap();
        assert!(
            handle
                .authenticate_none("test-user")
                .await
                .unwrap()
                .success()
        );

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = SshSession {
            handle: Arc::new(handle),
            session_id: "source-tab".to_string(),
            host: "jump.example.com".to_string(),
            event_tx,
            disconnected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            output_tap: Arc::new(parking_lot::RwLock::new(None)),
            output_tap_lock: Arc::new(tokio::sync::Mutex::new(())),
            relay_exec_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            relay_exec_reusable: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        assert!(!source.is_disconnected());

        // Clone a fresh interactive channel. Its events must flow into the
        // CALLER's event channel (the source's event_tx here) and carry the
        // NEW session id.
        let (clone_event_tx, mut clone_event_rx) = mpsc::unbounded_channel();
        let size = TerminalSize {
            cols: 120,
            rows: 32,
            pixel_width: 0,
            pixel_height: 0,
        };
        let (clone_session, clone_ssh_session) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            source.clone_channel(
                clone_event_tx,
                "clone-tab".to_string(),
                size,
                "xterm-256color",
            ),
        )
        .await
        .expect("clone_channel timed out")
        .expect("clone_channel failed");

        assert_eq!(clone_session.id, "clone-tab");
        assert_eq!(clone_ssh_session.session_id(), "clone-tab");
        assert!(!clone_ssh_session.is_disconnected());
        assert!(
            !source.is_disconnected(),
            "cloning must not disturb the source channel"
        );

        // The server saw exactly one channel open + pty + shell — on the
        // SAME connection (the mock accepts only one TCP stream; a second
        // TCP connect would hang forever). Poll briefly: the server records
        // the request before replying, but the reply may reach the client
        // before the recorder's push is observable from this task.
        let events_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            {
                let recorded = events.lock().unwrap();
                if recorded.as_slice() == ["open", "pty", "shell"] {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < events_deadline,
                "expected [open, pty, shell], got {:?}",
                events.lock().unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The Connected event for the clone lands on the clone's stream,
        // not the source's.
        match clone_event_rx.recv().await {
            Some(SessionEvent::Connected(id)) => assert_eq!(id, "clone-tab"),
            other => panic!("expected Connected(clone-tab), got {other:?}"),
        }
        assert!(event_rx.try_recv().is_err(), "source stream stays silent");

        // Closing the clone only tears down ITS channel (input writer task
        // exits and emits Disconnected for the clone id); the source
        // transport stays usable.
        clone_session.close().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(100), clone_event_rx.recv())
                .await
            {
                Ok(Some(SessionEvent::Disconnected(id, _))) => {
                    assert_eq!(id, "clone-tab");
                    break;
                }
                _ if std::time::Instant::now() < deadline => continue,
                _ => panic!("clone channel did not report its own Disconnected"),
            }
        }
        assert!(clone_ssh_session.is_disconnected());
        assert!(
            !source.is_disconnected(),
            "closing the clone must not disconnect the source"
        );

        // The shared transport still serves the source (another clone).
        let (second_clone_tx, mut second_clone_rx) = mpsc::unbounded_channel();
        let (_s2, second) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            source.clone_channel(
                second_clone_tx,
                "second-clone".to_string(),
                size,
                "xterm-256color",
            ),
        )
        .await
        .expect("second clone timed out")
        .expect("transport must survive clone close");
        assert!(matches!(
            second_clone_rx.recv().await,
            Some(SessionEvent::Connected(id)) if id == "second-clone"
        ));

        // A DISCONNECTED source must refuse to clone.
        source
            .disconnected
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (third_tx, _third_rx) = mpsc::unbounded_channel();
        let err = source
            .clone_channel(third_tx, "doomed".to_string(), size, "xterm-256color")
            .await
            .expect_err("disconnected source must not clone");
        assert!(err.to_string().contains("disconnected"));

        second
            .handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await
            .unwrap();
        server_task.await.unwrap();
    }
}

#[cfg(test)]
mod sftp_subsystem_reply_tests {
    use super::*;

    #[test]
    fn window_adjustment_is_non_terminal_while_waiting_for_subsystem_reply() {
        assert_eq!(
            classify_subsystem_reply(Some(ChannelMsg::WindowAdjusted {
                new_size: 2_097_152
            })),
            SubsystemReplyAction::ContinueWaiting
        );
    }

    #[test]
    fn success_and_failure_are_terminal_subsystem_replies() {
        assert_eq!(
            classify_subsystem_reply(Some(ChannelMsg::Success)),
            SubsystemReplyAction::Ready
        );
        assert_eq!(
            classify_subsystem_reply(Some(ChannelMsg::Failure)),
            SubsystemReplyAction::Error(crate::sftp::SftpError::SubsystemRejected)
        );
    }

    #[test]
    fn closed_channel_is_reported_as_closed_not_unexpected() {
        assert_eq!(
            classify_subsystem_reply(Some(ChannelMsg::Close)),
            SubsystemReplyAction::Error(crate::sftp::SftpError::ChannelClosed)
        );
        assert_eq!(
            classify_subsystem_reply(None),
            SubsystemReplyAction::Error(crate::sftp::SftpError::ChannelClosed)
        );
    }
}

#[cfg(test)]
mod otp_prompt_tests {
    use super::looks_like_otp_prompt;

    #[test]
    fn detects_english_otp_prompts() {
        assert!(looks_like_otp_prompt("OTP: "));
        assert!(looks_like_otp_prompt("Enter your MFA code: "));
        assert!(looks_like_otp_prompt("Verification code: "));
        assert!(looks_like_otp_prompt("TOTP: "));
        assert!(looks_like_otp_prompt("2FA code:"));
        assert!(looks_like_otp_prompt("Two-factor authentication code:"));
        assert!(looks_like_otp_prompt("Authenticator code:"));
    }

    #[test]
    fn detects_chinese_otp_prompts() {
        // JumpServer with MFA enabled typically surfaces one of these.
        assert!(looks_like_otp_prompt("请输入MFA认证码: "));
        assert!(looks_like_otp_prompt("请输入验证码: "));
        assert!(looks_like_otp_prompt("动态密码: "));
        assert!(looks_like_otp_prompt("二次验证码: "));
    }

    #[test]
    fn does_not_flag_plain_password_prompts() {
        // A regular password prompt must NOT be treated as an OTP prompt —
        // otherwise we'd skip the password and try to fetch a code that
        // doesn't exist, breaking normal PAM password auth.
        assert!(!looks_like_otp_prompt("Password: "));
        assert!(!looks_like_otp_prompt("password for user: "));
        assert!(!looks_like_otp_prompt("Enter passphrase: "));
        assert!(!looks_like_otp_prompt("[sudo] password for user: "));
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert!(looks_like_otp_prompt("otp:"));
        assert!(looks_like_otp_prompt("Otp:"));
        assert!(looks_like_otp_prompt("OTP:"));
        assert!(looks_like_otp_prompt("mfa code:"));
        assert!(looks_like_otp_prompt("MFA Code:"));
    }

    #[test]
    fn detects_jumpserver_2nd_password_prompt() {
        // JumpServer MFA commonly shows a second-stage "2nd Password:" prompt
        // after the primary password. This must be detected as OTP.
        assert!(looks_like_otp_prompt("2nd Password: "));
        assert!(looks_like_otp_prompt("2nd password:"));
        assert!(looks_like_otp_prompt("Second Password: "));
        assert!(looks_like_otp_prompt("second password:"));
        assert!(looks_like_otp_prompt("2nd pwd: "));
        assert!(looks_like_otp_prompt("second pwd: "));
        assert!(looks_like_otp_prompt("第二密码: "));
        assert!(looks_like_otp_prompt("二次密码: "));
    }
}
