//! The bridge between the HTTP layer and whatever backend actually runs
//! commands on saved SSH hosts. Defined as a trait so the relay crate never
//! imports `rusterm-ui` internals; the app layer provides `SshExecutor`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;

/// One saved SSH host as advertised by `GET /api/v1/hosts`.
#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// Outcome of executing one command remotely.
#[derive(Debug, Clone, Serialize)]
pub struct ExecOutcome {
    pub exit_code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
    pub duration_ms: u64,
}

/// Split a host selector of the form `{host_id}@{live_session_id}` into
/// its base host id and optional live-session id.
///
/// The API panel emits composite selectors so a request can target one
/// specific terminal tab — with jumpserver/bastion hosts, several tabs of
/// the same saved connection may sit on *different* target nodes, and the
/// session suffix is what disambiguates them. A selector without `@` (or
/// with an empty half) is returned unchanged as the base id.
pub fn split_host_selector(selector: &str) -> (&str, Option<&str>) {
    match selector.split_once('@') {
        Some((base, session)) if !base.is_empty() && !session.is_empty() => (base, Some(session)),
        _ => (selector, None),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("unknown host id: {0}")]
    UnknownHost(String),
    #[error("SSH connect failed: {0}")]
    Connect(String),
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("elevation required: {0}")]
    ElevationRequired(String),
}

/// One event on a streaming execution (`exec_stream`). Chunks arrive in
/// output order as the remote produces them; the stream always ends with
/// exactly one terminal event — `Done` (the command finished, possibly by
/// timeout) or `Failed` (the command's status became unknowable after it was
/// sent, e.g. the live PTY closed mid-run).
#[derive(Debug, Clone)]
pub enum ExecStreamEvent {
    /// A slice of remote output, in arrival order. PTY-backed executions
    /// merge stderr into stdout; the buffered fallback emits stdout and
    /// stderr as separate chunks (stdout first).
    Chunk(String),
    /// Terminal event: the command finished.
    Done {
        exit_code: Option<u32>,
        timed_out: bool,
        truncated: bool,
        duration_ms: u64,
    },
    /// Terminal event: the command was dispatched but its outcome is
    /// unknown (transport died mid-run). Chunks already emitted are valid.
    Failed { message: String },
}

/// Implemented by the app layer (`SshExecutor`) and by test doubles.
#[async_trait]
pub trait RelayExecutor: Send + Sync + std::fmt::Debug {
    /// All saved hosts, regardless of account permissions — filtering by
    /// `allowed_hosts` happens in the route handler.
    async fn list_hosts(&self) -> Vec<HostInfo>;

    /// Run `command` on `host_id`. The command has already passed the
    /// validator. `timeout` is the hard local deadline for the whole exec.
    async fn exec(
        &self,
        host_id: &str,
        command: &str,
        elevated: bool,
        timeout: Duration,
    ) -> Result<ExecOutcome, ExecutorError>;

    /// Run `command` on `host_id`, streaming output as it arrives instead
    /// of buffering until completion. Errors *before* anything ran are
    /// returned as `Err` (so the HTTP layer can still answer with a proper
    /// status code); once the stream starts, completion or failure is
    /// reported in-band via [`ExecStreamEvent::Done`] /
    /// [`ExecStreamEvent::Failed`].
    ///
    /// The default implementation is a buffered fallback: it awaits the
    /// plain [`RelayExecutor::exec`] and replays the outcome as one or two
    /// chunks plus `Done`. Executors with access to incremental output
    /// (live PTY taps) override it for true streaming.
    async fn exec_stream(
        &self,
        host_id: &str,
        command: &str,
        elevated: bool,
        timeout: Duration,
    ) -> Result<tokio::sync::mpsc::Receiver<ExecStreamEvent>, ExecutorError> {
        let outcome = self.exec(host_id, command, elevated, timeout).await?;
        Ok(buffered_exec_stream(outcome))
    }
}

/// Replay a buffered [`ExecOutcome`] as a short event stream: stdout chunk,
/// stderr chunk (each only when non-empty), then `Done`. Used by the default
/// `exec_stream` and by executors falling back to a buffered path.
pub fn buffered_exec_stream(outcome: ExecOutcome) -> tokio::sync::mpsc::Receiver<ExecStreamEvent> {
    // Capacity covers every event we send below, so the un-consumed sends
    // can't block (the receiver may not be polled yet).
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    if !outcome.stdout.is_empty() {
        let _ = tx.try_send(ExecStreamEvent::Chunk(outcome.stdout));
    }
    if !outcome.stderr.is_empty() {
        let _ = tx.try_send(ExecStreamEvent::Chunk(outcome.stderr));
    }
    let _ = tx.try_send(ExecStreamEvent::Done {
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        truncated: outcome.truncated,
        duration_ms: outcome.duration_ms,
    });
    rx
}

/// A `RelayExecutor` that rejects everything. Used when SSH host management
/// is unavailable (tests, degraded startup) so the relay still serves
/// `/health` and returns a clean 4xx instead of panicking.
#[derive(Debug, Default)]
pub struct NullExecutor;

#[async_trait]
impl RelayExecutor for NullExecutor {
    async fn list_hosts(&self) -> Vec<HostInfo> {
        Vec::new()
    }

    async fn exec(
        &self,
        host_id: &str,
        _command: &str,
        _elevated: bool,
        _timeout: Duration,
    ) -> Result<ExecOutcome, ExecutorError> {
        Err(ExecutorError::UnknownHost(host_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::split_host_selector;

    #[test]
    fn split_host_selector_handles_plain_and_composite_forms() {
        assert_eq!(split_host_selector("host-1"), ("host-1", None));
        assert_eq!(
            split_host_selector("host-1@tab-42"),
            ("host-1", Some("tab-42"))
        );
        // Only the first `@` splits — the session id keeps the rest.
        assert_eq!(split_host_selector("a@b@c"), ("a", Some("b@c")));
    }

    #[test]
    fn split_host_selector_treats_degenerate_forms_as_plain_ids() {
        assert_eq!(split_host_selector("host-1@"), ("host-1@", None));
        assert_eq!(split_host_selector("@tab-42"), ("@tab-42", None));
        assert_eq!(split_host_selector("@"), ("@", None));
        assert_eq!(split_host_selector(""), ("", None));
    }
}
