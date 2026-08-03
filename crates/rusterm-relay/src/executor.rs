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
