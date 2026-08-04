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
use std::time::{Duration, Instant};

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
        connect_authenticated(config, client_cfg, crate::otp::OtpProvider::Manual),
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

/// Maximum retained output while matching bastion prompts. Keeping a tail is
/// enough for prompt matching and diagnostics, while a noisy menu cannot grow
/// memory without bound.
const BASTION_LOGIN_BUFFER: usize = 64 * 1024;
/// A prompt that produces no new output for this long is considered stuck.
const BASTION_LOGIN_IDLE_TIMEOUT: Duration = Duration::from_secs(12);
/// One expect step may run this long even if the server keeps producing noise.
const BASTION_LOGIN_STEP_TIMEOUT: Duration = Duration::from_secs(20);
/// Navigation can be replayed once because no business command has been sent.
const BASTION_LOGIN_ATTEMPTS: usize = 2;

fn remaining_timeout(started: Instant, total: Duration, phase: &str) -> anyhow::Result<Duration> {
    let remaining = total.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        anyhow::bail!("{phase} exceeded the total timeout of {total:?}");
    }
    Ok(remaining)
}

async fn reconnect_within(
    handle: &DirectHandle,
    started: Instant,
    total: Duration,
) -> anyhow::Result<DirectHandle> {
    let remaining = remaining_timeout(started, total, "bastion reconnect")?;
    tokio::time::timeout(remaining, handle.reconnect())
        .await
        .map_err(|_| anyhow::anyhow!("bastion reconnect timed out after {remaining:?}"))?
}

async fn close_pty_channel(channel: &mut russh::Channel<client::Msg>) {
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        let _ = channel.eof().await;
        let _ = channel.close().await;
    })
    .await;
}

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
    /// `stdin` (when supplied) is sent only after the command line. Before
    /// credential input (for example `sudo -S`) is queued, terminal echo is
    /// disabled and verified with a shell marker; the wrapper restores echo
    /// after the command finishes.
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

        let operation_started = Instant::now();
        let mut channel = self.open_pty_shell(timeout).await?;

        // Without a login script there is no prompt contract, but a short
        // grace period still avoids racing a newly-created ordinary shell.
        tokio::time::sleep(Duration::from_millis(400)).await;

        if stdin.is_some() {
            let remaining = remaining_timeout(operation_started, timeout, "stdin echo protection")?;
            self.disable_pty_echo_for_stdin(&mut channel, remaining)
                .await?;
        }

        let wrapped = if stdin.is_some() {
            format!(
                "{{ {command} ; }} ; __rc=$? ; stty echo; printf '\\n{tag}%d\\n' \"$__rc\"\r",
                command = command,
                tag = rc_tag,
            )
        } else {
            format!(
                "{{ {command} ; }} ; __rc=$? ; printf '\\n{tag}%d\\n' \"$__rc\"\r",
                command = command,
                tag = rc_tag,
            )
        };
        channel.data(wrapped.as_bytes()).await?;
        if let Some(stdin) = stdin {
            channel.data(stdin).await?;
        }

        let mut result = ExecResult::default();
        let command_timeout =
            remaining_timeout(operation_started, timeout, "PTY command execution")?;
        let timed_out = match tokio::time::timeout(
            command_timeout,
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
        close_pty_channel(&mut channel).await;
        Ok(result)
    }

    async fn open_pty_shell(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<russh::Channel<client::Msg>> {
        let setup_started = Instant::now();
        let mut channel = tokio::time::timeout(timeout, self.handle.channel_open_session())
            .await
            .map_err(|_| anyhow::anyhow!("opening PTY channel timed out after {timeout:?}"))??;

        let pty_remaining = match remaining_timeout(setup_started, timeout, "PTY request") {
            Ok(remaining) => remaining,
            Err(error) => {
                close_pty_channel(&mut channel).await;
                return Err(error);
            }
        };
        let pty = tokio::time::timeout(
            pty_remaining,
            channel.request_pty(
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
            ),
        )
        .await;
        match pty {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                close_pty_channel(&mut channel).await;
                return Err(error.into());
            }
            Err(_) => {
                close_pty_channel(&mut channel).await;
                anyhow::bail!("PTY request timed out after {pty_remaining:?}");
            }
        }

        let shell_remaining = match remaining_timeout(setup_started, timeout, "PTY shell request") {
            Ok(remaining) => remaining,
            Err(error) => {
                close_pty_channel(&mut channel).await;
                return Err(error);
            }
        };
        match tokio::time::timeout(shell_remaining, channel.request_shell(true)).await {
            Ok(Ok(())) => Ok(channel),
            Ok(Err(error)) => {
                close_pty_channel(&mut channel).await;
                Err(error.into())
            }
            Err(_) => {
                close_pty_channel(&mut channel).await;
                anyhow::bail!("PTY shell request timed out after {shell_remaining:?}")
            }
        }
    }

    async fn disable_pty_echo_for_stdin(
        &self,
        channel: &mut russh::Channel<client::Msg>,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let token = random_sentinel_hex();
        let (hide_echo, marker) = stdin_ready_probe(&token);
        channel.data(hide_echo.as_bytes()).await?;
        let hidden_steps = [rusterm_core::LoginStep::Expect {
            pattern: regex::escape(&marker),
        }];
        self.drive_pty_login(
            channel,
            &hidden_steps,
            timeout.min(BASTION_LOGIN_STEP_TIMEOUT),
            "stdin echo protection",
        )
        .await
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
    /// verifies that the target shell can execute a unique readiness probe;
    /// only then is the wrapped business command sent.
    ///
    /// `SendOneKey` steps require credential resolution that is not available
    /// in the headless relay path. They fail safely rather than being skipped,
    /// because skipping input could advance into the wrong interactive state.
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
        let operation_started = Instant::now();

        // Navigation is safe to retry because the business command is not sent
        // until both the script and an active-shell probe have completed.
        let mut active = self.clone();
        let mut ready_channel = None;
        let mut last_error = None;

        for attempt in 1..=BASTION_LOGIN_ATTEMPTS {
            tracing::info!(
                "[relay] bastion pre-command attempt {attempt}/{BASTION_LOGIN_ATTEMPTS}"
            );

            let setup_timeout = remaining_timeout(operation_started, timeout, "PTY setup")?
                .min(Duration::from_secs(10));
            let mut channel = match active.open_pty_shell(setup_timeout).await {
                Ok(channel) => channel,
                Err(error) => {
                    last_error = Some(anyhow::anyhow!(
                        "[bastion-pre-command] PTY setup failed on attempt {attempt}: {error:#}"
                    ));
                    if attempt < BASTION_LOGIN_ATTEMPTS {
                        match reconnect_within(&active, operation_started, timeout).await {
                            Ok(fresh) => {
                                active = fresh;
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                continue;
                            }
                            Err(reconnect_error) => {
                                last_error = Some(anyhow::anyhow!(
                                    "[bastion-pre-command] PTY setup failed: {error:#}; \
                                     reconnect failed: {reconnect_error:#}"
                                ));
                            }
                        }
                    }
                    break;
                }
            };

            let navigation_timeout =
                remaining_timeout(operation_started, timeout, "bastion navigation")?;
            let navigation = active
                .drive_pty_login(&mut channel, login_steps, navigation_timeout, "navigation")
                .await;

            let pre_command = match navigation {
                Ok(()) => {
                    // A completed menu script is not sufficient proof that a
                    // target shell exists. Emit a harmless marker whose full
                    // value does not occur in the echoed probe command, then
                    // wait for the shell to produce it. Until this succeeds the
                    // user's business command remains protected.
                    let token = random_sentinel_hex();
                    let (probe, marker) = shell_ready_probe(&token);
                    if let Err(error) = channel.data(probe.as_bytes()).await {
                        Err(anyhow::anyhow!("shell-ready probe send failed: {error:#}"))
                    } else {
                        let ready_steps = [rusterm_core::LoginStep::Expect {
                            pattern: regex::escape(&marker),
                        }];
                        match remaining_timeout(
                            operation_started,
                            timeout,
                            "shell-ready verification",
                        ) {
                            Ok(remaining) => {
                                active
                                    .drive_pty_login(
                                        &mut channel,
                                        &ready_steps,
                                        remaining.min(BASTION_LOGIN_STEP_TIMEOUT),
                                        "shell-ready verification",
                                    )
                                    .await
                            }
                            Err(error) => Err(error),
                        }
                    }
                }
                Err(error) => Err(error),
            };

            match pre_command {
                Ok(()) => {
                    tracing::info!(
                        "[relay] bastion target shell verified on attempt {attempt}; \
                         business command may now be sent"
                    );
                    ready_channel = Some(channel);
                    break;
                }
                Err(error) => {
                    close_pty_channel(&mut channel).await;
                    tracing::warn!(
                        "[relay] bastion pre-command attempt {attempt}/\
                         {BASTION_LOGIN_ATTEMPTS} failed: {error:#}"
                    );
                    last_error = Some(anyhow::anyhow!(
                        "[bastion-pre-command] attempt {attempt}/\
                         {BASTION_LOGIN_ATTEMPTS} failed: {error:#}"
                    ));
                    if attempt < BASTION_LOGIN_ATTEMPTS {
                        match reconnect_within(&active, operation_started, timeout).await {
                            Ok(fresh) => {
                                active = fresh;
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                            Err(reconnect_error) => {
                                last_error = Some(anyhow::anyhow!(
                                    "[bastion-pre-command] attempt {attempt} failed: {error:#}; \
                                     reconnect failed: {reconnect_error:#}"
                                ));
                                break;
                            }
                        }
                    }
                }
            }
        }

        let mut channel = ready_channel.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                anyhow::anyhow!("[bastion-pre-command] target shell was not reached")
            })
        })?;

        // From here on no automatic retry is allowed: the business command may
        // have side effects and must execute at most once.
        let sentinel = format!("RUSTERM_PTY_{}", random_sentinel_hex());
        let rc_tag = format!("{sentinel}_RC_");

        if stdin.is_some() {
            // Disable terminal echo and prove that the shell applied it before
            // sending credential stdin. This prevents a queued sudo password
            // from becoming an echoed shell command.
            let remaining = remaining_timeout(operation_started, timeout, "stdin echo protection")?;
            active
                .disable_pty_echo_for_stdin(&mut channel, remaining)
                .await?;
        }

        let wrapped = if stdin.is_some() {
            format!(
                "{{ {command} ; }} ; __rc=$? ; stty echo; printf '\\n{tag}%d\\n' \"$__rc\"\r",
                command = command,
                tag = rc_tag,
            )
        } else {
            format!(
                "{{ {command} ; }} ; __rc=$? ; printf '\\n{tag}%d\\n' \"$__rc\"\r",
                command = command,
                tag = rc_tag,
            )
        };
        channel.data(wrapped.as_bytes()).await?;
        if let Some(stdin) = stdin {
            channel.data(stdin).await?;
        }

        let mut result = ExecResult::default();
        let command_timeout =
            remaining_timeout(operation_started, timeout, "business command execution")?;
        let timed_out = match tokio::time::timeout(
            command_timeout,
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

        close_pty_channel(&mut channel).await;
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
        total_timeout: Duration,
        phase: &'static str,
    ) -> anyhow::Result<()> {
        use rusterm_core::LoginStep;

        let mut buf: Vec<u8> = Vec::new();
        let mut sent_values: Vec<String> = Vec::new();
        let mut step_idx = 0usize;
        let total_started = Instant::now();

        while step_idx < steps.len() {
            if total_started.elapsed() >= total_timeout {
                anyhow::bail!(login_wait_failure(
                    phase,
                    step_idx,
                    "<total deadline>",
                    total_started,
                    total_started,
                    &buf,
                    &sent_values,
                    "total deadline reached",
                ));
            }

            let expect_pattern: Option<&str> = match &steps[step_idx] {
                LoginStep::Expect { pattern } => Some(pattern.as_str()),
                _ => None,
            };

            if let Some(pattern) = expect_pattern {
                tracing::info!("[relay] {phase} step {step_idx}: expect {pattern:?}");
                let step_started = Instant::now();
                let mut last_output = step_started;

                loop {
                    if login_pattern_matches(pattern, &buf)? {
                        tracing::info!(
                            "[relay] {phase} step {step_idx}: expect {pattern:?} matched"
                        );
                        buf.clear();
                        break;
                    }

                    let total_remaining = total_timeout.saturating_sub(total_started.elapsed());
                    let step_remaining = BASTION_LOGIN_STEP_TIMEOUT
                        .min(total_timeout)
                        .saturating_sub(step_started.elapsed());
                    let idle_remaining = BASTION_LOGIN_IDLE_TIMEOUT
                        .min(total_timeout)
                        .saturating_sub(last_output.elapsed());
                    let wait_for = total_remaining.min(step_remaining).min(idle_remaining);

                    if wait_for.is_zero() {
                        let reason = if total_remaining.is_zero() {
                            "total deadline reached"
                        } else if step_remaining.is_zero() {
                            "step deadline reached"
                        } else {
                            "PTY became idle"
                        };
                        anyhow::bail!(login_wait_failure(
                            phase,
                            step_idx,
                            pattern,
                            step_started,
                            last_output,
                            &buf,
                            &sent_values,
                            reason,
                        ));
                    }

                    match tokio::time::timeout(wait_for, channel.wait()).await {
                        Err(_) => {
                            anyhow::bail!(login_wait_failure(
                                phase,
                                step_idx,
                                pattern,
                                step_started,
                                last_output,
                                &buf,
                                &sent_values,
                                "PTY became idle or a deadline elapsed",
                            ));
                        }
                        Ok(Some(ChannelMsg::Data { data }))
                        | Ok(Some(ChannelMsg::ExtendedData { data, .. })) => {
                            append_bounded_login_output(&mut buf, &data);
                            last_output = Instant::now();
                        }
                        Ok(Some(ChannelMsg::Eof)) | Ok(Some(ChannelMsg::Close)) | Ok(None) => {
                            anyhow::bail!(login_wait_failure(
                                phase,
                                step_idx,
                                pattern,
                                step_started,
                                last_output,
                                &buf,
                                &sent_values,
                                "PTY closed (eof=true)",
                            ));
                        }
                        Ok(Some(_)) => {}
                    }
                }
                step_idx += 1;
            }

            while step_idx < steps.len() {
                if total_started.elapsed() >= total_timeout {
                    anyhow::bail!(login_wait_failure(
                        phase,
                        step_idx,
                        "<action>",
                        total_started,
                        total_started,
                        &buf,
                        &sent_values,
                        "total deadline reached before action",
                    ));
                }

                match &steps[step_idx] {
                    LoginStep::Expect { .. } => break,
                    LoginStep::Send { text } => {
                        tracing::info!(
                            "[relay] {phase} step {step_idx}: send ({} bytes)",
                            text.len()
                        );
                        sent_values.push(text.clone());
                        let payload = login_send_bytes(text);
                        channel.data(payload.as_slice()).await.map_err(|error| {
                            anyhow::anyhow!(
                                "{phase} step {step_idx} failed to send input: {error:#}"
                            )
                        })?;
                        step_idx += 1;
                    }
                    LoginStep::Delay { ms } => {
                        tracing::info!("[relay] {phase} step {step_idx}: delay {ms}ms");
                        let delay = Duration::from_millis(*ms);
                        let remaining = total_timeout.saturating_sub(total_started.elapsed());
                        if delay > remaining {
                            anyhow::bail!(login_wait_failure(
                                phase,
                                step_idx,
                                "<delay>",
                                total_started,
                                total_started,
                                &buf,
                                &sent_values,
                                "delay exceeds total deadline",
                            ));
                        }
                        tokio::time::sleep(delay).await;
                        step_idx += 1;
                    }
                    LoginStep::SendOneKey { name } => {
                        // Silently skipping a credential step advances the
                        // state machine into an invalid state and can cause the
                        // next input to land in the wrong prompt. Fail before
                        // any business command is sent instead.
                        anyhow::bail!(
                            "{phase} step {step_idx}: send_onekey {name:?} is not \
                             supported by the headless relay"
                        );
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
        let mut closed_before_marker = false;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if raw.len() < MAX_EXEC_OUTPUT {
                        raw.extend_from_slice(&data);
                    } else {
                        result.truncated = true;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, ext: _ }) => {
                    // PTY sessions usually funnel stderr through Data; merge
                    // ExtendedData too for completeness.
                    if raw.len() < MAX_EXEC_OUTPUT {
                        raw.extend_from_slice(&data);
                    } else {
                        result.truncated = true;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    closed_before_marker = true;
                    break;
                }
                _ => {}
            }

            // The echoed wrapper contains `<rc_tag>%d`, so the tag alone is
            // not a completion signal. Wait for an actual decimal status and
            // its terminating newline; the parser also handles split chunks.
            if find_complete_rc_marker(&raw, rc_tag).is_some() {
                break;
            }
        }

        Self::extract_pty_output(&raw, rc_tag, result);
        if closed_before_marker && result.exit_code.is_none() {
            anyhow::bail!("PTY channel closed before command completion marker");
        }
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
    /// Strategy: accept only a complete marker containing decimal exit-code
    /// digits followed by `\r?\n`. The echoed wrapper contains
    /// `<rc_tag>%d`, which must never be interpreted as command completion.
    /// If the sentinel is absent, keep the entire raw stream as best-effort
    /// stdout; the caller decides whether premature channel closure is fatal.
    fn extract_pty_output(raw: &[u8], rc_tag: &str, result: &mut ExecResult) {
        let marker = find_complete_rc_marker(raw, rc_tag);

        let stdout = if let Some((tag_idx, exit_code)) = marker {
            result.exit_code = Some(exit_code);
            let before = String::from_utf8_lossy(&raw[..tag_idx]);
            // Drop trailing CR/LF and whitespace.
            let before = before.trim_end_matches(['\r', '\n', ' ']);
            // Strip the echoed wrapper command line(s). The marker `__rc=$?`
            // is unique to our wrapper, so any line containing it is the
            // echoed command — never real command output.
            let stripped = strip_echoed_wrapper(before);
            // PTY output also carries terminal-mode chatter (bracketed paste
            // `ESC[?2004l`, colour codes, ...) and CRLF endings from
            // `ONLCR`. Strip them so machine-readable output (YAML, JSON,
            // ...) survives a redirect like `rusterm ... > out.yaml`.
            sanitize_pty_output(&stripped)
        } else {
            // No valid sentinel — never strip at an echoed `%d` placeholder.
            // Still clean ANSI/CR from the raw best-effort output so a
            // truncated capture doesn't carry escape sequences either.
            sanitize_pty_output(&String::from_utf8_lossy(raw))
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
    /// failure markers seen in the wild. The supplied `timeout` is an overall
    /// deadline shared by the exec attempt, reconnect, login navigation and
    /// PTY command, so a stuck bastion cannot multiply the API timeout.
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
        // A configured login script explicitly identifies an interactive
        // bastion. Do not speculatively execute the business command through
        // an exec channel first: a server may run it but omit ExitStatus, and
        // replaying through PTY would duplicate side effects.
        if !login_steps.is_empty() {
            return self
                .exec_via_pty_with_login(command, stdin, timeout, login_steps)
                .await;
        }

        let operation_started = Instant::now();
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
        let reconnect = reconnect_within(self, operation_started, timeout).await;
        let remaining = remaining_timeout(operation_started, timeout, "PTY fallback")?;
        match reconnect {
            Ok(fresh) => fresh.exec_via_pty(command, stdin, remaining).await,
            Err(e) => {
                tracing::warn!(
                    "[relay] reconnect for PTY fallback failed ({e:#}); \
                     attempting PTY on same connection"
                );
                self.exec_via_pty(command, stdin, remaining).await
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

/// Strip ANSI escape sequences from raw PTY bytes, returning clean UTF-8 text.
/// Mirrors the UI's `strip_ansi` so bastion menu prompts (which are wrapped
/// in OSC title sequences and CSI colour codes) can be reliably matched by
/// `expect` regexes in login scripts.
///
/// The regex covers the escape sequences we encounter on real PTY output:
///   - OSC  (`ESC ] ... BEL` / `ESC ] ... ESC \`) — terminal title sets
///   - CSI  (`ESC [ <params> <intermediates> <final>`) — colour, cursor,
///           and DEC private modes such as `ESC[?2004h` / `ESC[?2004l`
///           (bracketed paste enable/disable, emitted by bash on PTY init)
///   - Charset designators (`ESC ( B`, `ESC ) 0`, ...)
///   - Single-character ESC sequences (`ESC 7`, `ESC 8`, `ESC =`, ...)
pub fn strip_ansi_pty(data: &[u8]) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()*+][A-Za-z0-9]|\x1b[@-_]",
        )
        .expect("static ANSI-stripping regex must compile")
    });
    let raw = String::from_utf8_lossy(data);
    re.replace_all(&raw, "").to_string()
}

/// Normalize a PTY-captured command body into clean, machine-readable text.
///
/// PTY-backed executions inherit the shell's terminal init chatter and cooked
/// output processing — even after the echoed wrapper line is removed, the
/// captured stdout still contains:
///
///   - `ESC[?2004l` / `ESC[?2004h` — bracketed paste mode transitions that
///     bash emits during interactive-session init on a PTY.
///   - Other CSI/OSC sequences — title sets, colour codes, cursor moves.
///   - `\r\n` line endings — sshd's PTY runs with `OPOST | ONLCR`, so every
///     `\n` the command writes becomes `\r\n` on the wire. A file redirected
///     from the API's stdout (e.g. `kubectl -o yaml > out.yaml`) would
///     otherwise carry CRLF endings and a leading escape sequence.
///
/// This function strips ANSI escape sequences (reusing [`strip_ansi_pty`])
/// and normalizes line endings to `\n`:
///   - `\r\n` → `\n`
///   - stray `\r` not followed by `\n` → `\n` (rare; e.g. some shells emit
///     a bare CR to redraw a prompt line)
///
/// Tabs (`\t`) and other control characters that may legitimately appear in
/// captured output (YAML uses spaces, but the same code path runs commands
/// like `cat` over a binary file) are preserved — only CSI/OSC escapes and
/// CR are touched.
pub fn sanitize_pty_output(s: &str) -> String {
    let stripped = strip_ansi_pty(s.as_bytes());
    // Walk once: convert `\r\n` → `\n`, lone `\r` → `\n`. Cheaper than a
    // regex replace and avoids re-scanning the whole string for each pattern.
    let mut out = String::with_capacity(stripped.len());
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\r' {
            // Always emit `\n`; if the next byte is `\n` we skip it so we
            // don't double the newline (handles CRLF).
            out.push('\n');
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Fast path: copy a run of non-CR bytes verbatim.
        let run_end = bytes[i..]
            .iter()
            .position(|&c| c == b'\r')
            .map(|p| i + p)
            .unwrap_or(bytes.len());
        out.push_str(&stripped[i..run_end]);
        i = run_end;
    }
    out
}

/// Encode an interactive login answer exactly like pressing Enter in a PTY.
/// `ICRNL` maps carriage return to the line feed expected by the remote menu.
fn login_send_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\r');
    bytes
}

fn login_pattern_matches(pattern: &str, data: &[u8]) -> anyhow::Result<bool> {
    if pattern.is_empty() {
        return Ok(true);
    }
    let regex = regex::Regex::new(pattern)
        .map_err(|error| anyhow::anyhow!("invalid expect regex {pattern:?}: {error}"))?;
    Ok(regex.is_match(&strip_ansi_pty(data)))
}

/// Retain the newest prompt-sized tail. Menu redraws and banners can be very
/// noisy; dropping old bytes is safe because expects only concern current UI.
fn append_bounded_login_output(buffer: &mut Vec<u8>, data: &[u8]) {
    if data.len() >= BASTION_LOGIN_BUFFER {
        buffer.clear();
        buffer.extend_from_slice(&data[data.len() - BASTION_LOGIN_BUFFER..]);
        return;
    }
    let overflow = buffer
        .len()
        .saturating_add(data.len())
        .saturating_sub(BASTION_LOGIN_BUFFER);
    if overflow > 0 {
        buffer.drain(..overflow);
    }
    buffer.extend_from_slice(data);
}

fn output_tail(data: &[u8], max_chars: usize, redactions: &[String]) -> String {
    let mut clean = strip_ansi_pty(data);
    for value in redactions.iter().filter(|value| !value.is_empty()) {
        clean = clean.replace(value, "***");
    }
    clean
        .chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn bastion_menu_hint(data: &[u8]) -> &'static str {
    let clean = strip_ansi_pty(data);
    if clean.contains("请选择登录账号") {
        "login-account menu"
    } else if clean.contains("请选择目标资产") {
        "target-asset menu"
    } else if clean.contains("资产分类列表") || clean.contains("请选择资产分类") {
        "asset-category menu"
    } else {
        "unknown prompt"
    }
}

fn login_wait_failure(
    phase: &str,
    step_idx: usize,
    pattern: &str,
    step_started: Instant,
    last_output: Instant,
    data: &[u8],
    redactions: &[String],
    reason: &str,
) -> String {
    format!(
        "{phase} step {step_idx} stuck waiting for {pattern:?}: {reason}; \
         waited={:.1}s idle_for={:.1}s state={}; last_output={:?}",
        step_started.elapsed().as_secs_f32(),
        last_output.elapsed().as_secs_f32(),
        bastion_menu_hint(data),
        output_tail(data, 500, redactions),
    )
}

/// Disable PTY echo and print the marker only if `stty` succeeded. The full
/// marker is split across the printf format and argument so input echo cannot
/// satisfy the expectation.
fn stdin_ready_probe(token: &str) -> (String, String) {
    let marker = format!("RUSTERM_STDIN_READY_{token}");
    let command = format!("stty -echo && printf '\\nRUSTERM_STDIN_READY_%s\\n' '{token}'\r");
    debug_assert!(!command.contains(&marker));
    (command, marker)
}

/// Build a shell-readiness probe whose full marker does not appear in the
/// echoed command line. Seeing `marker` therefore proves a shell executed it.
fn shell_ready_probe(token: &str) -> (String, String) {
    let marker = format!("RUSTERM_READY_{token}");
    let command = format!("printf '\\nRUSTERM_READY_%s\\n' '{token}'\r");
    debug_assert!(!command.contains(&marker));
    (command, marker)
}

/// Generate 16 hex chars (64 bits) of randomness for a per-call sentinel.
/// Uses the process nanosecond timer + thread id as a fallback when the
/// `getrandom` crate isn't pulled in — sufficient entropy to avoid output
/// collisions in any plausible command output.
pub fn random_sentinel_hex() -> String {
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

/// Find the last complete PTY exit-code marker.
///
/// A valid marker is exactly `<rc_tag><digits>\r?\n`. In particular, this
/// rejects the `<rc_tag>%d\n` format string echoed as part of the shell
/// wrapper before the business command has run.
pub fn find_complete_rc_marker(raw: &[u8], rc_tag: &str) -> Option<(usize, u32)> {
    let tag = rc_tag.as_bytes();
    if tag.is_empty() || tag.len() >= raw.len() {
        return None;
    }

    let mut search_from = 0;
    let mut last_complete = None;
    while search_from + tag.len() < raw.len() {
        let Some(relative) = find_subsequence(&raw[search_from..], tag) else {
            break;
        };
        let tag_idx = search_from + relative;
        let mut cursor = tag_idx + tag.len();
        let digits_start = cursor;
        let mut exit_code = 0_u32;
        let mut overflowed = false;

        while cursor < raw.len() && raw[cursor].is_ascii_digit() {
            match exit_code
                .checked_mul(10)
                .and_then(|value| value.checked_add(u32::from(raw[cursor] - b'0')))
            {
                Some(value) => exit_code = value,
                None => overflowed = true,
            }
            cursor += 1;
        }

        let has_digits = cursor > digits_start;
        if cursor < raw.len() && raw[cursor] == b'\r' {
            cursor += 1;
        }
        if has_digits && !overflowed && cursor < raw.len() && raw[cursor] == b'\n' {
            last_complete = Some((tag_idx, exit_code));
        }

        search_from = tag_idx + tag.len();
    }

    last_complete
}

/// Heuristic: does this [`ExecResult`] look like the server rejected the
/// `exec` channel? Matches the literal JumpServer message and the generic
/// markers we already detect in `SshSession::fetch_via_exec`.
fn looks_like_exec_rejected(result: &ExecResult) -> bool {
    // Any reported exit status proves the server accepted and ran the exec
    // request. Never replay such a command through PTY, even if its legitimate
    // output happens to contain one of our rejection phrases.
    if result.exit_code.is_some() {
        return false;
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
pub fn strip_echoed_wrapper(s: &str) -> String {
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
    fn never_replays_a_command_that_reported_an_exit_status() {
        for exit_code in [0, 1, 126] {
            let mut result = rc_result("command not allowed");
            result.exit_code = Some(exit_code);
            assert!(!looks_like_exec_rejected(&result));
        }
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
    fn complete_rc_marker_rejects_echoed_wrapper_placeholder() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let raw = b"$ { uname -a ; } ; __rc=$? ; printf '\\nRUSTERM_PTY_0123456789abcdef_RC_%d\\n' \"$__rc\"\r\n";

        assert_eq!(find_complete_rc_marker(raw, tag), None);
    }

    #[test]
    fn complete_rc_marker_selects_real_marker_after_echo() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let raw = b"$ printf 'RUSTERM_PTY_0123456789abcdef_RC_%d\\n' \"$__rc\"\r\ncommand output\r\nRUSTERM_PTY_0123456789abcdef_RC_0\r\n";

        assert_eq!(
            find_complete_rc_marker(raw, tag),
            Some((raw.len() - tag.len() - 3, 0))
        );
    }

    #[test]
    fn complete_rc_marker_waits_for_digits_and_newline_across_chunks() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let mut raw = format!("output\r\n{tag}").into_bytes();
        assert_eq!(find_complete_rc_marker(&raw, tag), None);

        raw.extend_from_slice(b"12");
        assert_eq!(find_complete_rc_marker(&raw, tag), None);

        raw.extend_from_slice(b"3\r");
        assert_eq!(find_complete_rc_marker(&raw, tag), None);

        raw.push(b'\n');
        assert_eq!(find_complete_rc_marker(&raw, tag), Some((8, 123)));
    }

    #[test]
    fn complete_rc_marker_parses_nonzero_exit_code() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let raw = format!("{tag}127\n");

        assert_eq!(find_complete_rc_marker(raw.as_bytes(), tag), Some((0, 127)));
    }

    #[test]
    fn complete_rc_marker_survives_trailing_prompt_output() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let raw = format!("{tag}0\r\n{}", "prompt ".repeat(20));

        assert_eq!(find_complete_rc_marker(raw.as_bytes(), tag), Some((0, 0)));
    }

    #[test]
    fn complete_rc_marker_rejects_similar_output() {
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let raw = b"RUSTERM_PTY_0123456789abcdef_RC_not-a-code\nRUSTERM_PTY_other_RC_0\n";

        assert_eq!(find_complete_rc_marker(raw, tag), None);
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
        // PTY ONLCR must not survive — every `\r\n` should be `\n`.
        assert!(
            !stdout.contains('\r'),
            "expected CRLF to be normalized to LF, got: {stdout:?}"
        );
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
        let raw = b"$ { ls ; } ; __rc=$? ; printf '\nRUSTERM_PTY_0123456789abcdef_RC_%d\n' \"$__rc\"\r\nfile1\r\n\r\nRUSTERM_PTY_0123456789abcdef_RC_0\r\n$ ";
        let tag = "RUSTERM_PTY_0123456789abcdef_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, Some(0));
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(stdout.contains("file1"));
        // The echoed wrapper line must be stripped.
        assert!(!stdout.contains("__rc=$?"));
        // CRLF must be normalized to LF.
        assert!(!stdout.contains('\r'));
    }

    #[test]
    fn extract_strips_bracketed_paste_and_ansi_from_pty_init() {
        // Real-world shape: bash emits `ESC[?2004l` (disable bracketed paste)
        // during interactive-session init on a PTY, plus a title-set OSC
        // sequence. The ONLCR pty mode also turns every `\n` into `\r\n`.
        let raw = b"\x1b[?2004l\r\napiVersion: v1\r\nkind: Node\r\n\r\nRUSTERM_PTY_deadbeefdeadbeef_RC_0\r\n$ ";
        let tag = "RUSTERM_PTY_deadbeefdeadbeef_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        assert_eq!(result.exit_code, Some(0));
        let stdout = String::from_utf8_lossy(&result.stdout);
        // The `ESC[?2004l` escape is stripped; the `\r\n` that followed it
        // becomes a `\n` (the literal newline bash emitted after the
        // sequence). The command's actual output begins on the next line.
        assert_eq!(stdout, "\napiVersion: v1\nkind: Node");
        // No control characters should survive.
        assert!(!stdout.contains('\x1b'));
        assert!(!stdout.contains('\r'));
    }

    #[test]
    fn extract_strips_osc_title_set_from_output() {
        // OSC sequence `ESC]0;<title>\x07` is emitted by many shells after
        // every command. It must not appear in captured stdout.
        let raw = b"\x1b]0;user@host\x07hello\r\n\r\nRUSTERM_PTY_aaaaaaaa12345678_RC_0\r\n";
        let tag = "RUSTERM_PTY_aaaaaaaa12345678_RC_";
        let mut result = ExecResult::default();
        DirectHandle::extract_pty_output(raw, tag, &mut result);
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert_eq!(stdout, "hello");
        assert!(!stdout.contains("user@host"));
        assert!(!stdout.contains('\x1b'));
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

    #[test]
    fn sanitize_strips_bracketed_paste_disable_sequence() {
        // `ESC[?2004l` is exactly what bash emits when it disables bracketed
        // paste on PTY init — the original bug report.
        let s = "\x1b[?2004lapiVersion: v1\r\nkind: Node\r\n";
        assert_eq!(sanitize_pty_output(s), "apiVersion: v1\nkind: Node\n");
    }

    #[test]
    fn sanitize_strips_bracketed_paste_enable_and_disable() {
        // Some shells emit `ESC[?2004h` on init, then `ESC[?2004l` on exit.
        let s = "\x1b[?2004h\x1b[?2004ldata\r\n";
        assert_eq!(sanitize_pty_output(s), "data\n");
    }

    #[test]
    fn sanitize_normalizes_crlf_to_lf() {
        assert_eq!(sanitize_pty_output("a\r\nb\r\nc"), "a\nb\nc");
    }

    #[test]
    fn sanitize_converts_lone_cr_to_lf() {
        // Some terminals use a bare `\r` to redraw the current line
        // (progress bars). Convert to `\n` so the captured output is at
        // least line-based rather than collapsing onto one line.
        assert_eq!(sanitize_pty_output("a\rb\rc"), "a\nb\nc");
    }

    #[test]
    fn sanitize_preserves_tabs() {
        // YAML uses spaces, not tabs, but other commands (`cat -T`, TSV
        // output, ...) legitimately emit `\t`. Don't mangle them.
        assert_eq!(sanitize_pty_output("col1\tcol2\r\n"), "col1\tcol2\n");
    }

    #[test]
    fn sanitize_strips_osc_title_set() {
        // `ESC]0;<title>\x07` (BEL-terminated OSC) — terminal title set.
        assert_eq!(
            sanitize_pty_output("\x1b]0;user@host\x07hello\n"),
            "hello\n"
        );
    }

    #[test]
    fn sanitize_strips_osc_with_st_terminator() {
        // `ESC]0;<title>\x1b\\` (ST-terminated OSC) — xterm title set.
        assert_eq!(sanitize_pty_output("\x1b]0;title\x1b\\hello\n"), "hello\n");
    }

    #[test]
    fn sanitize_strips_csi_colour_codes() {
        // `ESC[32m...ESC[0m` — green text + reset.
        assert_eq!(sanitize_pty_output("\x1b[32mgreen\x1b[0m\r\n"), "green\n");
    }

    #[test]
    fn sanitize_preserves_normal_output_verbatim() {
        assert_eq!(
            sanitize_pty_output("plain text\nmore\n"),
            "plain text\nmore\n"
        );
    }

    #[test]
    fn sanitize_handles_empty_input() {
        assert_eq!(sanitize_pty_output(""), "");
    }

    #[test]
    fn sanitize_does_not_corrupt_multibyte_utf8() {
        // Chinese characters in bastion prompts must survive.
        let s = "请选择目标资产\r\n";
        assert_eq!(sanitize_pty_output(s), "请选择目标资产\n");
    }

    #[test]
    fn login_send_is_a_real_enter_not_literal_backslash_n() {
        let bytes = login_send_bytes("/cao");
        assert_eq!(bytes, b"/cao\r");
        assert!(!bytes.ends_with(br"\n"));
    }

    #[test]
    fn expect_matches_fragmented_ansi_wrapped_prompt() {
        let mut buffer = Vec::new();
        append_bounded_login_output(
            &mut buffer,
            b"\x1b]0;xuchao@host\x07\x1b[32m\xe8\xaf\xb7\xe9\x80\x89",
        );
        assert!(!login_pattern_matches("请选择目标资产", &buffer).unwrap());
        append_bounded_login_output(
            &mut buffer,
            b"\xe6\x8b\xa9\xe7\x9b\xae\xe6\xa0\x87\xe8\xb5\x84\xe4\xba\xa7\xef\xbc\x9a\x1b[0m",
        );
        assert!(login_pattern_matches("请选择目标资产", &buffer).unwrap());
    }

    #[test]
    fn login_output_buffer_keeps_recent_tail_when_noisy() {
        let mut buffer = vec![b'a'; BASTION_LOGIN_BUFFER - 2];
        append_bounded_login_output(&mut buffer, b"PROMPT");
        assert_eq!(buffer.len(), BASTION_LOGIN_BUFFER);
        assert!(buffer.ends_with(b"PROMPT"));
    }

    #[test]
    fn diagnostic_tail_redacts_values_sent_by_login_script() {
        let sent = vec!["super-secret".to_string()];
        let tail = output_tail(b"Password: super-secret\r\nNext prompt", 500, &sent);
        assert!(!tail.contains("super-secret"));
        assert!(tail.contains("Password: ***"));
    }

    #[test]
    fn stdin_ready_marker_requires_successful_echo_suppression() {
        let (probe, marker) = stdin_ready_probe("0123456789abcdef");
        assert!(probe.contains("stty -echo && printf"));
        assert!(!probe.contains(&marker));
    }

    #[test]
    fn shell_ready_marker_is_not_present_in_echoed_probe_command() {
        let (probe, marker) = shell_ready_probe("0123456789abcdef");
        assert!(probe.ends_with('\r'));
        assert!(!probe.contains(&marker));
        assert_eq!(marker, "RUSTERM_READY_0123456789abcdef");
    }

    #[test]
    fn timeout_diagnostic_identifies_current_bastion_menu_and_tail() {
        let now = Instant::now();
        let error = login_wait_failure(
            "navigation",
            4,
            "请选择目标资产",
            now,
            now,
            "请选择目标资产：".as_bytes(),
            &[],
            "PTY became idle",
        );
        assert!(error.contains("navigation step 4"));
        assert!(error.contains("target-asset menu"));
        assert!(error.contains("last_output"));
    }
}
