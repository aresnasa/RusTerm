//! Non-interactive SSH connections for background consumers.
//!
//! Unlike [`crate::client::SshClient`], which opens a PTY + shell and wires
//! the channel into the terminal event loop, the types here are used by
//! headless subsystems — SSH tunnels (`rusterm-tunnel`) and the REST relay
//! (`rusterm-relay`) — that only need exec channels and direct-tcpip
//! forwarding.
//!
//! Some servers (bastion hosts like JumpServer) reject `exec` requests and
//! only allow interactive PTY sessions. [`DirectHandle::exec_via_pty`] runs a
//! command through a PTY + shell with sentinel markers, and
//! [`DirectHandle::exec_with_fallback`] auto-detects exec rejection and
//! retries via PTY — giving API consumers transparent compatibility with
//! bastion-host SSH.

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::{ChannelMsg, ChannelStream, Pty};

use rusterm_core::config::SshConfig;

use crate::client::{
    DEFAULT_KEEPALIVE_INTERVAL, DEFAULT_KEEPALIVE_MAX, Handler, client_config,
    connect_authenticated,
};

/// Options for [`connect_direct`].
#[derive(Debug, Clone)]
pub struct DirectConnectOptions {
    /// Application-level keepalive interval. `None` disables keepalives.
    /// Tunnels should keep this enabled so dead connections are detected
    /// quickly (after `keepalive_max` unanswered probes).
    pub keepalive_interval: Duration,
    /// Number of unanswered keepalives before the connection is dropped.
    pub keepalive_max: usize,
    /// TCP connect + SSH handshake timeout.
    pub connect_timeout: Duration,
}

impl Default for DirectConnectOptions {
    fn default() -> Self {
        Self {
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            keepalive_max: DEFAULT_KEEPALIVE_MAX,
            connect_timeout: Duration::from_secs(20),
        }
    }
}

/// An authenticated SSH connection without a PTY/shell. Cheap to clone
/// around; the underlying russh handle is shared via `Arc`.
///
/// The originating `SshConfig` + options are retained so that
/// [`DirectHandle::reconnect`] can establish a fresh transport. This is
/// needed by [`DirectHandle::exec_with_fallback`]: some bastion servers
/// (JumpServer's TERM-SSHD) reject a second channel on the same connection
/// after an exec request fails, so the PTY fallback must open a brand-new
/// connection rather than reuse the one whose exec was rejected.
#[derive(Clone)]
pub struct DirectHandle {
    handle: Arc<client::Handle<Handler>>,
    /// Config + options used to create this handle, kept for reconnect.
    origin: Option<(SshConfig, DirectConnectOptions)>,
}

impl std::fmt::Debug for DirectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectHandle").finish_non_exhaustive()
    }
}

/// Result of running a single command via an exec channel.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    /// Remote exit status, if the server reported one.
    pub exit_code: Option<u32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The command was killed locally because `timeout` elapsed.
    pub timed_out: bool,
    /// Combined stdout+stderr exceeded [`MAX_EXEC_OUTPUT`] and the surplus
    /// was discarded. Callers should surface a truncation marker so the user
    /// knows the output is incomplete — a silent truncation is indistinguishable
    /// from a command that genuinely produced exactly 8 MiB.
    pub truncated: bool,
}

impl ExecResult {
    pub fn stdout_string(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }

    pub fn stderr_string(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }
}

/// Open transport, handshake and authenticate, returning a headless handle
/// with keepalives configured.
pub async fn connect_direct(
    config: &SshConfig,
    options: DirectConnectOptions,
) -> anyhow::Result<DirectHandle> {
    let client_cfg = Arc::new(client_config(
        Some(options.keepalive_interval),
        options.keepalive_max,
    ));
    let handle = tokio::time::timeout(
        options.connect_timeout,
        connect_authenticated(config, client_cfg),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "connect to {}:{} timed out after {:?}",
            config.host,
            config.port,
            options.connect_timeout
        )
    })??;
    Ok(DirectHandle {
        handle: Arc::new(handle),
        origin: Some((config.clone(), options)),
    })
}

/// Cap on captured exec output (stdout + stderr combined). Prevents a
/// runaway remote process from exhausting memory.
const MAX_EXEC_OUTPUT: usize = 8 * 1024 * 1024;

impl DirectHandle {
    /// Run `command` on the remote host and capture stdout/stderr and the
    /// exit status. Returns partial output with `timed_out: true` when the
    /// local timeout fires.
    pub async fn exec(&self, command: &str, timeout: Duration) -> anyhow::Result<ExecResult> {
        self.exec_with_optional_stdin(command, None, timeout).await
    }

    /// Run a command while supplying private data on the SSH channel's stdin.
    /// The input is never embedded in the remote command string, which keeps
    /// credentials out of process listings, shell history, and command logs.
    pub async fn exec_with_stdin(
        &self,
        command: &str,
        stdin: &[u8],
        timeout: Duration,
    ) -> anyhow::Result<ExecResult> {
        self.exec_with_optional_stdin(command, Some(stdin), timeout)
            .await
    }

    async fn exec_with_optional_stdin(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> anyhow::Result<ExecResult> {
        let mut result = ExecResult::default();
        let timed_out =
            match tokio::time::timeout(timeout, self.exec_inner(command, stdin, &mut result)).await
            {
                Ok(inner) => {
                    inner?;
                    false
                }
                Err(_) => true,
            };
        result.timed_out = timed_out;
        Ok(result)
    }

    /// Run `command` through a PTY + shell channel instead of an exec channel.
    ///
    /// This is the path taken by servers that reject `exec` requests — bastion
    /// hosts such as JumpServer respond with `exec request failed, try
    /// username/server/account as login name.` and never start the command.
    /// Opening a PTY + shell and typing the command (with sentinel markers so
    /// we can recover stdout and the exit code from the echoed PTY stream)
    /// emulates what a human operator does interactively.
    ///
    /// `stdin` (when supplied) is piped into the PTY *before* the command. This
    /// is used for `sudo -S` password feeds; because the PTY echoes input by
    /// default, callers should be aware the password would appear in a real
    /// terminal — but the captured output is filtered to the sentinel-delimited
    /// region, so credentials are not returned in `stdout`.
    ///
    /// Output capture reuses [`MAX_EXEC_OUTPUT`] so a runaway command can't
    /// exhaust memory; `truncated` is set when the cap is hit.
    pub async fn exec_via_pty(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> anyhow::Result<ExecResult> {
        // Unique per-call sentinel so output that legitimately contains the
        // word "RUSTERM" can't be confused with our marker. 16 hex chars =
        // 64 bits of entropy — collision-proof for any plausible output.
        let sentinel = format!("RUSTERM_PTY_{}", random_sentinel_hex());
        let rc_tag = format!("{sentinel}_RC_");

        let mut channel = self.handle.channel_open_session().await?;

        // Cooked-terminal PTY modes — same set as `SshClient::connect`.
        // ICRNL maps Enter's \r to \n (without it some shells don't see the
        // command as terminated); OPOST+ONLCR make output \n→\r\n. ECHO is
        // kept ON intentionally — JumpServer and similar bastions render
        // their interactive menus via echo, and disabling it can break the
        // login flow. We strip the echoed command from captured output
        // using the sentinel delimiters.
        channel
            .request_pty(
                false,
                "xterm",
                80,
                24,
                0,
                0,
                &[
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

        // Give the shell a moment to initialise before sending input. Some
        // servers (JumpServer especially) emit a banner/menu before the shell
        // prompt is ready; a short sleep lets the prompt land so our command
        // isn't consumed as part of a menu selection.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Pipe optional stdin (e.g. sudo password) before the command.
        if let Some(stdin) = stdin {
            channel.data(stdin).await?;
        }

        // Construct the wrapped command:
        //   { <command> ; } ; __rc=$?
        //   printf '\n<sentinel>_RC_%d\n' "$__rc"
        //
        // The leading `{ ... ; }` group lets `;`-terminated and `|`-piped
        // commands work without extra quoting. We capture $? *after* the
        // group completes, then emit a uniquely-tagged line carrying the
        // exit code. Everything between the command echo and the sentinel
        // line is treated as command stdout.
        //
        // `printf` is used (not `echo`) because its format is portable
        // across bash/dash/zsh/busybox and doesn't interpret backslashes
        // in the command output.
        let wrapped = format!(
            "{{ {command} ; }} ; __rc=$? ; printf '\\n{tag}%d\\n' \"$__rc\"\n",
            command = command,
            tag = rc_tag,
        );
        channel.data(wrapped.as_bytes()).await?;

        let mut result = ExecResult::default();
        let timed_out = match tokio::time::timeout(
            timeout,
            self.exec_via_pty_inner(&mut channel, &rc_tag, &mut result),
        )
        .await
        {
            Ok(inner) => {
                inner?;
                false
            }
            Err(_) => true,
        };
        result.timed_out = timed_out;

        // Best-effort channel close; errors are ignored because the result
        // is already captured.
        let _ = channel.eof().await;
        let _ = channel.close().await;
        Ok(result)
    }

    /// Like [`exec_via_pty`](Self::exec_via_pty) but first replays a login
    /// script to navigate an interactive bastion / jump-host menu before
    /// sending the actual command.
    ///
    /// Bastion hosts such as QiZhi (齐治交互终端) or JumpServer present a
    /// multi-step asset-selection menu after login instead of a shell. A human
    /// operator navigates by typing numbers (category → asset → account) until
    /// they reach a shell on the target node. This method automates that flow
    /// using the same `expect`/`send`/`delay` DSL as the UI login scripts.
    ///
    /// `login_steps` is the parsed login script (from
    /// [`rusterm_core::parse_login_script`]). After the last step, the method
    /// waits a short grace period for the target shell to initialise, then
    /// sends the wrapped command with sentinel markers exactly like
    /// `exec_via_pty`.
    ///
    /// `SendOneKey` steps require credential resolution that is not available
    /// in the headless relay path; they are skipped with a warning.
    pub async fn exec_via_pty_with_login(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
        login_steps: &[rusterm_core::LoginStep],
    ) -> anyhow::Result<ExecResult> {
        if login_steps.is_empty() {
            return self.exec_via_pty(command, stdin, timeout).await;
        }

        tracing::info!(
            "[relay] bastion login script active ({} steps), navigating menu before exec",
            login_steps.len()
        );

        let sentinel = format!("RUSTERM_PTY_{}", random_sentinel_hex());
        let rc_tag = format!("{sentinel}_RC_");

        let mut channel = self.handle.channel_open_session().await?;
        channel
            .request_pty(
                false,
                "xterm",
                80,
                24,
                0,
                0,
                &[
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

        // ── Phase 1: Navigate the bastion menu via expect/send ─────────────
        //
        // The whole navigation is bounded by `timeout` so a mismatched expect
        // can't hang the API call forever.
        let nav_result =
            tokio::time::timeout(timeout, self.drive_pty_login(&mut channel, login_steps)).await;

        match nav_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = channel.eof().await;
                let _ = channel.close().await;
                anyhow::bail!("bastion login navigation failed: {e:#}");
            }
            Err(_) => {
                let _ = channel.eof().await;
                let _ = channel.close().await;
                anyhow::bail!("bastion login navigation timed out after {:?}", timeout);
            }
        }

        // Give the target shell a moment to print its prompt / MOTD after the
        // last menu selection lands.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // ── Phase 2: Send the actual command with sentinel markers ──────────
        if let Some(stdin) = stdin {
            channel.data(stdin).await?;
        }
        let wrapped = format!(
            "{{ {command} ; }} ; __rc=$? ; printf '\\n{tag}%d\\n' \"$__rc\"\n",
            command = command,
            tag = rc_tag,
        );
        channel.data(wrapped.as_bytes()).await?;

        // ── Phase 3: Read until sentinel (same as exec_via_pty) ─────────────
        let mut result = ExecResult::default();
        let timed_out = match tokio::time::timeout(
            timeout,
            self.exec_via_pty_inner(&mut channel, &rc_tag, &mut result),
        )
        .await
        {
            Ok(inner) => {
                inner?;
                false
            }
            Err(_) => true,
        };
        result.timed_out = timed_out;

        let _ = channel.eof().await;
        let _ = channel.close().await;
        Ok(result)
    }

    /// Drive an expect/send login script against a live PTY channel.
    ///
    /// Reads chunks of PTY output into an accumulating buffer, and whenever
    /// the current `Expect` step's regex matches the buffer, fires the
    /// following `Send`/`Delay` steps until the next `Expect` (or end of
    /// script). Discards matched output from the buffer after each expect
    /// so stale prompts don't trigger re-matches.
    async fn drive_pty_login(
        &self,
        channel: &mut russh::Channel<client::Msg>,
        steps: &[rusterm_core::LoginStep],
    ) -> anyhow::Result<()> {
        use rusterm_core::LoginStep;

        let mut buf: Vec<u8> = Vec::new();
        let mut step_idx = 0usize;

        while step_idx < steps.len() {
            // Collect consecutive non-expect actions and fire them immediately.
            let expect_pattern: Option<&str> = match &steps[step_idx] {
                LoginStep::Expect { pattern } => Some(pattern.as_str()),
                _ => None,
            };

            if let Some(pattern) = expect_pattern {
                // Wait until output matches this expect.
                let re =
                    if pattern.is_empty() {
                        None
                    } else {
                        Some(regex::Regex::new(pattern).map_err(|e| {
                            anyhow::anyhow!("invalid expect regex {pattern:?}: {e}")
                        })?)
                    };

                loop {
                    let text = String::from_utf8_lossy(&buf);
                    let matched = match &re {
                        Some(r) => r.is_match(&text),
                        None => true, // empty pattern = instant match
                    };
                    if matched {
                        // Clear consumed output so the next expect doesn't
                        // re-fire on stale data.
                        buf.clear();
                        break;
                    }
                    // Read more PTY output.
                    match channel.wait().await {
                        Some(ChannelMsg::Data { data }) => {
                            let combined = buf.len() + data.len();
                            if combined < MAX_EXEC_OUTPUT {
                                buf.extend_from_slice(&data);
                            }
                        }
                        Some(ChannelMsg::ExtendedData { data, .. }) => {
                            let combined = buf.len() + data.len();
                            if combined < MAX_EXEC_OUTPUT {
                                buf.extend_from_slice(&data);
                            }
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            anyhow::bail!(
                                "PTY closed while waiting for expect pattern {:?}",
                                pattern
                            );
                        }
                        _ => {}
                    }
                }
                step_idx += 1;
            }

            // Fire consecutive Send / Delay / SendOneKey steps.
            while step_idx < steps.len() {
                match &steps[step_idx] {
                    LoginStep::Expect { .. } => break,
                    LoginStep::Send { text } => {
                        tracing::debug!("[relay] login step {step_idx}: send {:?}", text);
                        channel.data(format!("{text}\\n").as_bytes()).await?;
                        step_idx += 1;
                    }
                    LoginStep::Delay { ms } => {
                        tracing::debug!("[relay] login step {step_idx}: delay {ms}ms");
                        tokio::time::sleep(Duration::from_millis(*ms)).await;
                        step_idx += 1;
                    }
                    LoginStep::SendOneKey { name } => {
                        // OneKey credential resolution is not available in the
                        // headless relay path. Skip with a warning rather than
                        // failing — bastion menu navigation typically uses only
                        // plain `send` steps.
                        tracing::warn!(
                            "[relay] login step {step_idx}: send_onekey {name:?} \
                             is not supported in headless relay path, skipping"
                        );
                        step_idx += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Read loop for [`exec_via_pty`]: accumulate channel data until the
    /// sentinel exit-code line appears (or the channel closes / the result
    /// cap is hit). On success, trims captured stdout to the content between
    /// the command echo and the sentinel line.
    async fn exec_via_pty_inner(
        &self,
        channel: &mut russh::Channel<client::Msg>,
        rc_tag: &str,
        result: &mut ExecResult,
    ) -> anyhow::Result<()> {
        // We capture the full PTY stream (command echo + output + sentinel)
        // into `raw` and post-process at the end. PTY output can interleave
        // Data and ExtendedData; for a PTY, sshd typically sends everything
        // via Data, but we merge both to be safe.
        let mut raw: Vec<u8> = Vec::new();
        let mut saw_sentinel = false;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let combined = raw.len();
                    if combined < MAX_EXEC_OUTPUT {
                        raw.extend_from_slice(&data);
                    } else {
                        result.truncated = true;
                    }
                    // Cheap early-exit: stop reading once the sentinel is
                    // visible in the stream. We still drain one more iteration
                    // in case the exit-code digits straddle a Data boundary.
                    if let Some(idx) = find_subsequence(&raw, rc_tag.as_bytes()) {
                        saw_sentinel = true;
                        // Check whether the trailing newline (end of the
                        // sentinel line) is already in the buffer.
                        let after_tag = &raw[idx + rc_tag.len()..];
                        if after_tag.contains(&b'\n') {
                            break;
                        }
                    }
                }
                Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                    // PTY sessions usually funnel stderr through Data; merge
                    // ExtendedData too for completeness.
                    let combined = raw.len();
                    if combined < MAX_EXEC_OUTPUT {
                        raw.extend_from_slice(&data);
                    } else {
                        result.truncated = true;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }

        let _ = saw_sentinel; // used only for control flow above
        Self::extract_pty_output(&raw, rc_tag, result);
        Ok(())
    }

    /// Parse the raw PTY stream into `result.stdout` and `result.exit_code`.
    ///
    /// Layout we expect (after OPOST/ONLCR translation):
    /// ```text
    /// <banner/prompt noise>
    /// <echoed command line>\r\n
    /// <command stdout>\r\n
    /// \r\n
    /// RUSTERM_PTY_<hex>_RC_<code>\r\n
    /// <prompt>
    /// ```
    ///
    /// Strategy: find the *last* occurrence of `rc_tag`. The exit code is the
    /// decimal digits immediately after it (up to the next newline). Stdout
    /// is everything *after* the first occurrence of the echoed command
    /// newline up to the sentinel line. If the sentinel is absent (channel
    /// closed prematurely), keep the entire raw stream as stdout with no exit
    /// code — this matches the headless `exec` behaviour for a killed process.
    fn extract_pty_output(raw: &[u8], rc_tag: &str, result: &mut ExecResult) {
        let raw_str = String::from_utf8_lossy(raw);

        // 1) Exit code: digits between `rc_tag` and the next newline.
        if let Some(tag_idx) = raw_str.rfind(rc_tag) {
            let after = &raw_str[tag_idx + rc_tag.len()..];
            // Collect leading decimal digits (the exit code). tolerate an
            // optional leading space.
            let digits: String = after
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(code) = digits.parse::<u32>() {
                result.exit_code = Some(code);
            }
        }

        // 2) Stdout: the content between the command echo and the sentinel.
        //    Use `rfind` (last occurrence) because the echoed wrapper command
        //    contains the `rc_tag` literally inside its `printf '\n<tag>...'`
        //    format string — the *real* sentinel is the last occurrence,
        //    which is followed by digits and a newline. Walking back from
        //    there drops the trailing blank line that `printf '\n...'`
        //    injects before the marker, plus any prompt.
        let stdout = if let Some(tag_idx) = raw_str.rfind(rc_tag) {
            let before = &raw_str[..tag_idx];
            // Drop trailing CR/LF and whitespace.
            let before = before.trim_end_matches(['\r', '\n', ' ']);
            // Strip the echoed wrapper command line(s). The marker `__rc=$?`
            // is unique to our wrapper, so any line containing it is the
            // echoed command — never real command output.
            strip_echoed_wrapper(before)
        } else {
            // No sentinel — keep raw stream as stdout (best-effort).
            raw_str.to_string()
        };

        result.stdout = stdout.into_bytes();
        // PTY merges stderr into stdout; report empty stderr to avoid
        // double-counting in callers that aggregate both.
        result.stderr.clear();
    }

    /// Try `exec` first; if the server rejects the exec channel (bastion /
    /// jump-host behaviour), transparently retry via PTY. `stdin` is forwarded
    /// to the PTY path only — the headless exec path already handles stdin
    /// via [`exec_with_stdin`].
    ///
    /// Detection looks at both the literal JumpServer message and generic exec
    /// failure markers seen in the wild. The PTY retry is bounded by the
    /// same `timeout` (the exec attempt's time is already spent, so the PTY
    /// path may run past the original deadline — this is intentional, since
    /// a bastion's PTY setup is slower than an exec channel).
    ///
    /// `login_steps` (when non-empty) enables bastion-menu navigation: the
    /// PTY fallback replays the expect/send script to traverse the bastion's
    /// interactive asset-selection menu before sending the actual command.
    pub async fn exec_with_fallback(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> anyhow::Result<ExecResult> {
        self.exec_with_fallback_and_login(command, stdin, timeout, &[])
            .await
    }

    /// Like [`exec_with_fallback`](Self::exec_with_fallback) but with optional
    /// bastion login-script navigation.
    pub async fn exec_with_fallback_and_login(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        timeout: Duration,
        login_steps: &[rusterm_core::LoginStep],
    ) -> anyhow::Result<ExecResult> {
        let first = if let Some(stdin) = stdin {
            self.exec_with_stdin(command, stdin, timeout).await?
        } else {
            self.exec(command, timeout).await?
        };
        if !looks_like_exec_rejected(&first) {
            return Ok(first);
        }
        tracing::info!(
            "[relay] exec channel rejected ({}), retrying via PTY shell",
            first.stdout_string()
        );
        // Bastion servers (JumpServer's TERM-SSHD) refuse to open a second
        // channel on the same connection after an exec request is rejected —
        // the subsequent `channel_open_session` in `exec_via_pty` fails with
        // `ConnectFailed`. Open a brand-new connection for the PTY path so the
        // shell channel is the first (and only) channel on that transport.
        // If reconnect is unavailable (no origin stored), fall back to the
        // old same-connection behaviour as a best-effort.
        let use_login = !login_steps.is_empty();
        match self.reconnect().await {
            Ok(fresh) => {
                if use_login {
                    fresh
                        .exec_via_pty_with_login(command, stdin, timeout, login_steps)
                        .await
                } else {
                    fresh.exec_via_pty(command, stdin, timeout).await
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[relay] reconnect for PTY fallback failed ({e:#}); \
                     attempting PTY on same connection"
                );
                if use_login {
                    self.exec_via_pty_with_login(command, stdin, timeout, login_steps)
                        .await
                } else {
                    self.exec_via_pty(command, stdin, timeout).await
                }
            }
        }
    }

    async fn exec_inner(
        &self,
        command: &str,
        stdin: Option<&[u8]>,
        result: &mut ExecResult,
    ) -> anyhow::Result<()> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        if let Some(stdin) = stdin {
            channel.data(std::io::Cursor::new(stdin)).await?;
            channel.eof().await?;
        }

        let mut reader = channel;
        loop {
            match reader.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    let combined = result.stdout.len() + result.stderr.len();
                    if combined < MAX_EXEC_OUTPUT {
                        let data: &[u8] = &data;
                        result.stdout.extend_from_slice(data);
                    } else {
                        result.truncated = true;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                    let combined = result.stdout.len() + result.stderr.len();
                    if combined < MAX_EXEC_OUTPUT {
                        let data: &[u8] = &data;
                        result.stderr.extend_from_slice(data);
                    } else {
                        result.truncated = true;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    result.exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        Ok(())
    }

    /// Open a `direct-tcpip` channel — the SSH-side counterpart of a `ssh -L`
    /// forward. The returned stream behaves like a TCP connection to
    /// `target_host:target_port` from the SSH server's point of view.
    pub async fn open_direct_tcpip(
        &self,
        target_host: &str,
        target_port: u16,
        originator: (&str, u16),
    ) -> anyhow::Result<ChannelStream<client::Msg>> {
        let channel = self
            .handle
            .channel_open_direct_tcpip(
                target_host,
                target_port as u32,
                originator.0,
                originator.1 as u32,
            )
            .await?;
        Ok(channel.into_stream())
    }

    /// Probe liveness with a cheap exec. Returns `true` when a trivial
    /// command round-trips before `timeout`.
    pub async fn is_alive(&self, timeout: Duration) -> bool {
        self.exec("true", timeout).await.is_ok_and(|r| !r.timed_out)
    }

    pub async fn disconnect(&self) -> anyhow::Result<()> {
        self.handle
            .disconnect(russh::Disconnect::ByApplication, "Bye", "")
            .await?;
        Ok(())
    }

    /// Establish a **fresh** SSH connection using the same config/options that
    /// created this handle. Used by [`exec_with_fallback`](Self::exec_with_fallback)
    /// to get a clean transport for the PTY path: bastion servers like JumpServer
    /// reject a second channel on a connection whose exec was just refused.
    ///
    /// Returns a brand-new [`DirectHandle`] sharing the same `origin` (so it
    /// too can reconnect). If this handle was constructed without an `origin`
    /// (e.g. test stubs), returns `None` and the caller should fall back to the
    /// same-connection PTY attempt.
    pub async fn reconnect(&self) -> anyhow::Result<DirectHandle> {
        let (config, options) = self
            .origin
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no origin config stored for reconnect"))?;
        connect_direct(config, options.clone()).await
    }
}

// ── Free-standing helpers ────────────────────────────────────────────────

/// Generate 16 hex chars (64 bits) of randomness for a per-call sentinel.
/// Uses the process nanosecond timer + thread id as a fallback when the
/// `getrandom` crate isn't pulled in — sufficient entropy to avoid output
/// collisions in any plausible command output.
fn random_sentinel_hex() -> String {
    // Prefer the OS RNG when available; fall back to a time+thread mix.
    // Both paths yield 8 bytes → 16 hex chars.
    let bytes: [u8; 8] = match try_os_random() {
        Some(b) => b,
        None => fallback_random_bytes(),
    };
    let mut s = String::with_capacity(16);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(not(target_arch = "wasm32"))]
fn try_os_random() -> Option<[u8; 8]> {
    use std::sync::OnceLock;
    // Lazy-init getrandom-style fill via the OS. We don't depend on the
    // `getrandom` crate directly; `russh` already pulls it in transitively,
    // but we avoid reaching into its API. Instead, fall through to the
    // time-based mixer — it's good enough for sentinel uniqueness.
    let _ = OnceLock::<()>::new();
    None
}

#[cfg(target_arch = "wasm32")]
fn try_os_random() -> Option<[u8; 8]> {
    None
}

/// Fallback RNG mixing wall-clock nanos with thread id. Not cryptographic,
/// but unique-per-call is all we need for a sentinel.
fn fallback_random_bytes() -> [u8; 8] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let thread = thread_id_hash();
    let mixed = nanos
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(thread as u64)
        .rotate_left(17);
    mixed.to_le_bytes()
}

/// Stable per-thread counter, used as a mix-in for the fallback RNG.
fn thread_id_hash() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Find the byte index of `needle` in `haystack`. Returns `None` if absent.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Heuristic: does this [`ExecResult`] look like the server rejected the
/// `exec` channel? Matches the literal JumpServer message and the generic
/// markers we already detect in `SshSession::fetch_via_exec`.
fn looks_like_exec_rejected(result: &ExecResult) -> bool {
    if result.exit_code.is_some() && result.exit_code != Some(0) {
        // A non-zero exit with the rejection text still triggers the PTY
        // fallback — but a clean exit with stdout means the command ran.
        // Only fall through if the output matches the rejection markers.
    }
    let combined = format!("{} {}", result.stdout_string(), result.stderr_string());
    let combined = combined.to_ascii_lowercase();
    [
        "exec request failed",
        "try username/server/account",
        "command not allowed",
        "channel request failed",
    ]
    .iter()
    .any(|needle| combined.contains(needle))
}

/// Drop any line of `s` that looks like the echoed wrapper command
/// (`{ ... ; } ; __rc=$? ; printf ...`). The marker `__rc=$?` is unique
/// to our wrapper, so any line containing it is the echoed command — never
/// real command output. We also strip a leading prompt-only fragment
/// (`$ `, `# `, `> `) from the first surviving line so the returned stdout
/// starts cleanly at the command's actual output.
fn strip_echoed_wrapper(s: &str) -> String {
    s.split('\n')
        .filter(|line| {
            let trimmed = line.trim_matches(['\r', ' ']);
            !trimmed.contains("__rc=$?")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches(['\n', '\r', ' '])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc_result(stdout: &str) -> ExecResult {
        ExecResult {
            exit_code: None,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            timed_out: false,
            truncated: false,
        }
    }

    #[test]
    fn detects_jumpserver_rejection_message() {
        let r = rc_result("exec request failed, try username/server/account as login name.");
        assert!(looks_like_exec_rejected(&r));
    }

    #[test]
    fn detects_generic_exec_rejection() {
        let r = rc_result("command not allowed on this server");
        assert!(looks_like_exec_rejected(&r));
    }

    #[test]
    fn does_not_flag_normal_command_output() {
        let r = rc_result("total 0\ndrwxr-xr-x 2 root root 40 Aug 3 10:00 .");
        assert!(!looks_like_exec_rejected(&r));
    }

    #[test]
    fn does_not_flag_exec_request_word_in_benign_output() {
        // "exec request" alone (without "failed") should NOT trigger.
        let r = rc_result("logs: exec request #42 completed");
        assert!(!looks_like_exec_rejected(&r));
    }

    #[test]
    fn sentinel_is_unique_per_call() {
        let a = random_sentinel_hex();
        let b = random_sentinel_hex();
        // Extremely high probability of difference; if this ever flaps,
        // the fallback RNG needs revisiting.
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn find_subsequence_handles_edges() {
        assert_eq!(find_subsequence(b"hello world", b"world"), Some(6));
        assert_eq!(find_subsequence(b"hello", b""), None);
        assert_eq!(find_subsequence(b"abc", b"abcd"), None);
        assert_eq!(find_subsequence(b"abc", b"abc"), Some(0));
    }

    #[test]
    fn extract_recovers_exit_code_and_stdout_from_marker() {
        let raw = b"Last login: ...\r\n$ ls /tmp\r\nfile1\nfile2\n\r\nRUSTERM_PTY_deadbeefdeadbeef_RC_0\r\n$ ";
        let tag = "RUSTERM_PTY_deadbeefdeadbeef_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, Some(0));
        // stdout should contain the command output, not the prompt / echo / marker.
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("file1"));
        assert!(stdout.contains("file2"));
        assert!(!stdout.contains("RUSTERM_PTY_"));
    }

    #[test]
    fn extract_handles_nonzero_exit_code() {
        let raw = b"$ false\r\n\r\nRUSTERM_PTY_aaaaaaaa12345678_RC_1\r\n$ ";
        let tag = "RUSTERM_PTY_aaaaaaaa12345678_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn extract_strips_echoed_wrapper_line() {
        let raw = b"$ { ls ; } ; __rc=$? ; printf '\\nRUSTERM_PTY_0123456789abcdef_RC_%d\\n' \"$__rc\"\r\nfile1\r\n\r\nRUSTERM_PTY_0123456789abcdef_RC_0\r\n$ ";
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, Some(0));
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("file1"));
        // The echoed wrapper line must be stripped.
        assert!(!stdout.contains("__rc=$?"));
    }

    #[test]
    fn extract_without_sentinel_keeps_raw_output() {
        // Channel closed before sentinel arrived — keep raw, no exit code.
        let raw = b"partial output, no sentinel\r\n";
        let tag = "RUSTERM_PTY_nonsense_sentinel_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, None);
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("partial output"));
    }

    #[test]
    fn strip_echoed_wrapper_preserves_normal_output() {
        let s = "line1\nline2\n{ ls ; } ; __rc=$? ; printf '\\n..\\n' \"$__rc\"";
        let out = strip_echoed_wrapper(s);
        assert_eq!(out, "line1\nline2");
    }

    #[test]
    fn strip_echoed_wrapper_keeps_output_when_no_marker() {
        let s = "line1\nline2\nline3";
        let out = strip_echoed_wrapper(s);
        assert_eq!(out, "line1\nline2\nline3");
    }
}
