//! Non-interactive SSH connections for background consumers.
//!
//! Unlike [`crate::client::SshClient`], which opens a PTY + shell and wires
//! the channel into the terminal event loop, the types here are used by
//! headless subsystems — SSH tunnels (`rusterm-tunnel`) and the REST relay
//! (`rusterm-relay`) — that only need exec channels and direct-tcpip
//! forwarding.

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::{ChannelMsg, ChannelStream};

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
#[derive(Clone)]
pub struct DirectHandle {
    handle: Arc<client::Handle<Handler>>,
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
}
