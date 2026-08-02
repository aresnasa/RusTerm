//! Bridges between the REST relay / SSH tunnel subsystems and the app.
//!
//! Both subsystems are deliberately UI-agnostic (see their crates): the
//! relay needs a way to enumerate saved hosts and run commands on them, the
//! tunnel manager needs a way to resolve `connection_id` → `SshConfig`.
//! This module implements those two traits over the live [`AppState`]
//! signal, and owns the process-level runtimes (relay handle, tunnel
//! manager) the UI talks to.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use rusterm_core::config::{ConnectionConfig, ConnectionKind, SshConfig};
use rusterm_relay::{
    ExecOutcome, ExecutorError, HostInfo, RelayExecutor, RelayHandle, run as run_relay,
};
use rusterm_ssh::{DirectConnectOptions, ExecResult, connect_direct};
use rusterm_tunnel::{TunnelConnector, TunnelManager};
use zeroize::Zeroizing;

// ── Tokio runtime ────────────────────────────────────────────────────────

/// Dioxus spawns component futures on its own scheduler; in practice that
/// scheduler runs on tokio, but we don't want a panic the day it doesn't.
/// The relay server and tunnel supervisors need *a* tokio runtime — take
/// the current one if available, otherwise lazily create a private
/// multi-threaded runtime that lives for the process.
static FALLBACK_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

pub fn runtime_handle() -> tokio::runtime::Handle {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => FALLBACK_RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .thread_name("rusterm-relay-tunnel")
                    .build()
                    .expect("failed to create relay/tunnel runtime")
            })
            .handle()
            .clone(),
    }
}

// ── Shared connection registry ────────────────────────────────────────────
//
// Dioxus `Signal`s are !Sync (generational-box UnsyncStorage), so neither
// the relay executor nor the tunnel connector may hold one. Instead, every
// post-unlock mutation of `state.connections` also mirrors into this
// process-wide registry. Reads come straight from an `RwLock`, holding no
// lock across `.await`s.
static CONNECTION_REGISTRY: OnceLock<Arc<RwLock<Vec<ConnectionConfig>>>> = OnceLock::new();

pub fn connection_registry() -> Arc<RwLock<Vec<ConnectionConfig>>> {
    CONNECTION_REGISTRY
        .get_or_init(|| Arc::new(RwLock::new(Vec::new())))
        .clone()
}

/// Mirror the current connections into the shared registry. Called by the
/// UI whenever `state.connections` is set (on load, add, edit, delete).
pub fn sync_connection_registry(connections: Vec<ConnectionConfig>) {
    if let Ok(mut guard) = connection_registry().write() {
        *guard = connections;
    }
}

fn read_connections() -> Vec<ConnectionConfig> {
    connection_registry()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

// ── Short-lived sudo credential lease ───────────────────────────────────

const SUDO_CREDENTIAL_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SudoCredentialKey {
    connection_id: String,
    host: String,
    port: u16,
    username: String,
}

struct CachedSudoCredential {
    value: Zeroizing<String>,
    source_session_id: String,
    expires_at: Instant,
}

#[derive(Default)]
struct SudoCredentialCache {
    entries: RwLock<HashMap<SudoCredentialKey, CachedSudoCredential>>,
}

impl SudoCredentialCache {
    fn cache(
        &self,
        key: SudoCredentialKey,
        source_session_id: String,
        value: String,
        now: Instant,
    ) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                key,
                CachedSudoCredential {
                    value: Zeroizing::new(value),
                    source_session_id,
                    expires_at: now + SUDO_CREDENTIAL_TTL,
                },
            );
        }
    }

    fn get(&self, key: &SudoCredentialKey, now: Instant) -> Option<Zeroizing<String>> {
        let mut entries = self.entries.write().ok()?;
        if entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= now)
        {
            entries.remove(key);
            return None;
        }
        entries
            .get(key)
            .map(|entry| Zeroizing::new(entry.value.to_string()))
    }

    fn clear_for_session(&self, source_session_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.retain(|_, entry| entry.source_session_id != source_session_id);
        }
    }

    fn clear_key(&self, key: &SudoCredentialKey) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(key);
        }
    }
}

static SUDO_CREDENTIALS: OnceLock<SudoCredentialCache> = OnceLock::new();

fn sudo_credentials() -> &'static SudoCredentialCache {
    SUDO_CREDENTIALS.get_or_init(SudoCredentialCache::default)
}

fn sudo_credential_key(connection: &ConnectionConfig) -> Option<SudoCredentialKey> {
    let ConnectionKind::Ssh(ssh) = &connection.kind else {
        return None;
    };
    Some(SudoCredentialKey {
        connection_id: connection.id.clone(),
        host: ssh.host.clone(),
        port: ssh.port,
        username: ssh.username.clone(),
    })
}

/// Cache the credential the user explicitly submitted to a sudo prompt. The
/// lease is process-local, bound to the saved connection plus host identity,
/// expires with the normal sudo window, and is zeroized when removed.
pub(crate) fn cache_sudo_credential(
    connection: &ConnectionConfig,
    source_session_id: &str,
    credential: &str,
) {
    let Some(key) = sudo_credential_key(connection) else {
        return;
    };
    sudo_credentials().cache(
        key,
        source_session_id.to_string(),
        credential.to_string(),
        Instant::now(),
    );
}

/// A repeated sudo password prompt means the submitted value was rejected.
/// Revoke only the lease originating from that runtime session.
pub(crate) fn clear_sudo_credential_for_session(source_session_id: &str) {
    sudo_credentials().clear_for_session(source_session_id);
}

// ── SSH config helpers ───────────────────────────────────────────────────

/// Extract the `SshConfig` of a saved connection, if it is one.
fn ssh_config_of(conn: &ConnectionConfig) -> Option<&SshConfig> {
    match &conn.kind {
        ConnectionKind::Ssh(ssh) => Some(ssh),
        _ => None,
    }
}

fn to_host_info(conn: &ConnectionConfig, ssh: &SshConfig) -> HostInfo {
    HostInfo {
        id: conn.id.clone(),
        name: conn.name.clone(),
        host: ssh.host.clone(),
        port: ssh.port,
        username: ssh.username.clone(),
    }
}

// ── Relay executor ───────────────────────────────────────────────────────

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn sudo_command(command: &str, read_password_from_stdin: bool) -> String {
    let mode = if read_password_from_stdin {
        "-S -p ''"
    } else {
        "-n"
    };
    format!("sudo {mode} -- sh -lc {}", shell_single_quote(command),)
}

fn sudo_authorization_failed(result: &ExecResult) -> bool {
    if result.exit_code == Some(0) {
        return false;
    }
    let stderr = result.stderr_string().to_ascii_lowercase();
    [
        "password is required",
        "a password is required",
        "no password was provided",
        "incorrect password",
        "sorry, try again",
        "authentication failure",
        "interactive authentication is required",
        "a terminal is required",
        "no tty present",
        "must have a tty",
        "is not in the sudoers",
        "may not run sudo",
        "not allowed to execute",
        "not permitted to execute",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

/// Runs validated relay commands against saved SSH hosts. One fresh SSH
/// connection per request: simple, honest about cost, and naturally
/// rebound after remote host changes — the rate limiter keeps the cost of
/// connection-per-call bounded for abusive clients.
#[derive(Debug, Default)]
pub struct AppRelayExecutor;

#[async_trait]
impl RelayExecutor for AppRelayExecutor {
    async fn list_hosts(&self) -> Vec<HostInfo> {
        read_connections()
            .iter()
            .filter_map(|conn| ssh_config_of(conn).map(|ssh| to_host_info(conn, ssh)))
            .collect()
    }

    async fn exec(
        &self,
        host_id: &str,
        command: &str,
        elevated: bool,
        timeout: Duration,
    ) -> Result<ExecOutcome, ExecutorError> {
        let (credential_key, ssh) = read_connections()
            .into_iter()
            .find_map(|conn| {
                if conn.id == host_id || conn.name == host_id {
                    let key = sudo_credential_key(&conn)?;
                    match conn.kind {
                        ConnectionKind::Ssh(ssh) => Some((key, ssh)),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| ExecutorError::UnknownHost(host_id.to_string()))?;

        let started = Instant::now();
        let handle = connect_direct(&ssh, DirectConnectOptions::default())
            .await
            .map_err(|e| ExecutorError::Connect(format!("{e:#}")))?;

        let result = if elevated {
            let non_interactive_command = sudo_command(command, false);
            let first = handle
                .exec(&non_interactive_command, timeout)
                .await
                .map_err(|e| ExecutorError::Exec(format!("{e:#}")))?;
            if !sudo_authorization_failed(&first) {
                first
            } else {
                let Some(credential) = sudo_credentials().get(&credential_key, Instant::now())
                else {
                    let _ = handle.disconnect().await;
                    return Err(ExecutorError::ElevationRequired(
                        "No reusable sudo authorization is available for this host. Run sudo once in its RusTerm session with OneKey enabled, then retry."
                            .to_string(),
                    ));
                };
                let stdin = Zeroizing::new(format!("{}\n", credential.as_str()));
                let password_command = sudo_command(command, true);
                let second = handle
                    .exec_with_stdin(&password_command, stdin.as_bytes(), timeout)
                    .await
                    .map_err(|e| ExecutorError::Exec(format!("{e:#}")))?;
                if sudo_authorization_failed(&second) {
                    sudo_credentials().clear_key(&credential_key);
                    let _ = handle.disconnect().await;
                    return Err(ExecutorError::ElevationRequired(
                        "The reusable sudo credential was rejected or sudo policy denied this command. Re-authorize sudo in the target RusTerm session."
                            .to_string(),
                    ));
                }
                second
            }
        } else {
            handle
                .exec(command, timeout)
                .await
                .map_err(|e| ExecutorError::Exec(format!("{e:#}")))?
        };
        let _ = handle.disconnect().await;

        Ok(ExecOutcome {
            exit_code: result.exit_code,
            stdout: result.stdout_string(),
            stderr: result.stderr_string(),
            timed_out: result.timed_out,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

// ── Tunnel connector ─────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct AppTunnelConnector;

#[async_trait]
impl TunnelConnector for AppTunnelConnector {
    async fn resolve(&self, connection_id: &str) -> anyhow::Result<Option<SshConfig>> {
        Ok(read_connections().into_iter().find_map(|conn| {
            if conn.id == connection_id || conn.name == connection_id {
                match conn.kind {
                    ConnectionKind::Ssh(ssh) => Some(ssh),
                    _ => None,
                }
            } else {
                None
            }
        }))
    }
}

// ── Runtime holders on AppState ──────────────────────────────────────────

/// The live relay server, if running. Inside an `Arc<RwLock>` so the
/// `Clone` bound on `AppState` doesn't propagate onto [`RelayHandle`]
/// (which owns a oneshot sender and a `JoinHandle`).
#[derive(Clone)]
pub struct RelayRuntime(pub Arc<RwLock<Option<RelayHandle>>>);

impl std::fmt::Debug for RelayRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRuntime")
            .field(
                "running",
                &self.0.read().map(|g| g.is_some()).unwrap_or(false),
            )
            .finish_non_exhaustive()
    }
}

impl Default for RelayRuntime {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(None)))
    }
}

impl RelayRuntime {
    pub fn bound_addr(&self) -> Option<std::net::SocketAddr> {
        self.0.read().ok()?.as_ref().map(|h| h.bound_addr)
    }

    pub fn is_running(&self) -> bool {
        self.0.read().map(|g| g.is_some()).unwrap_or(false)
    }
}

// ── Lifecycle helpers called by the UI ──────────────────────────────────

/// Build the tunnel manager for this app session and load `tunnels.json`.
/// Does NOT start any tunnel — call `autostart` after connections load.
pub fn init_tunnel_manager() -> Arc<TunnelManager> {
    let connector = Arc::new(AppTunnelConnector);
    let manager = TunnelManager::with_runtime(connector, runtime_handle());
    if let Err(e) = manager.load_from_disk() {
        tracing::warn!("[tunnel] failed to load tunnels.json: {e:#}");
    }
    manager
}

/// Start the relay with the given config. Errors are returned as strings
/// for direct display in the UI (e.g. "port already in use").
pub fn start_relay(
    config: rusterm_relay::RelayConfig,
    runtime: RelayRuntime,
) -> Result<(), String> {
    if runtime.is_running() {
        return Ok(());
    }
    let executor: Arc<dyn RelayExecutor> = Arc::new(AppRelayExecutor);
    let handle = runtime_handle();
    let (result_tx, result_rx) = std::sync::mpsc::channel::<anyhow::Result<RelayHandle>>();
    handle.spawn(async move {
        let _ = result_tx.send(run_relay(config, executor).await);
    });
    // Bind is effectively instant; wait on the channel with a generous
    // deadline so a wedged runtime can't freeze the UI thread.
    match result_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(relay_handle)) => {
            let addr = relay_handle.bound_addr;
            if let Ok(mut guard) = runtime.0.write() {
                *guard = Some(relay_handle);
            }
            tracing::info!("[relay] started on {addr}");
            Ok(())
        }
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(e) => Err(format!("relay start timed out or runtime is wedged: {e}")),
    }
}

/// Stop the relay. The graceful shutdown is scheduled on the runtime and
/// not awaited synchronously — dropping the oneshot inside `shutdown()` is
/// enough to stop accepting connections, and the serving task exits on its
/// own within milliseconds.
pub fn stop_relay(runtime: RelayRuntime) {
    let relay = {
        let Ok(mut guard) = runtime.0.write() else {
            return;
        };
        guard.take()
    };
    if let Some(handle) = relay {
        let runtime = runtime_handle();
        runtime.spawn(async move {
            handle.shutdown().await;
        });
        tracing::info!("[relay] stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_command_quotes_untrusted_command_without_embedding_a_password() {
        let command = "printf '%s' \"$HOME\"";
        let wrapped = sudo_command(command, true);
        assert_eq!(
            wrapped,
            "sudo -S -p '' -- sh -lc 'printf '\"'\"'%s'\"'\"' \"$HOME\"'"
        );
        assert!(!wrapped.contains("secret"));
    }

    #[test]
    fn sudo_failure_detection_does_not_treat_command_exit_one_as_auth_failure() {
        let command_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"application error".to_vec(),
            timed_out: false,
        };
        assert!(!sudo_authorization_failed(&command_failure));

        let auth_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"sudo: a password is required".to_vec(),
            timed_out: false,
        };
        assert!(sudo_authorization_failed(&auth_failure));

        let interactive_auth_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"sudo: interactive authentication is required\n".to_vec(),
            timed_out: false,
        };
        assert!(sudo_authorization_failed(&interactive_auth_failure));
    }

    fn key(connection_id: &str, host: &str) -> SudoCredentialKey {
        SudoCredentialKey {
            connection_id: connection_id.to_string(),
            host: host.to_string(),
            port: 22,
            username: "ops".to_string(),
        }
    }

    #[test]
    fn sudo_credentials_are_host_bound_expiring_and_revocable_by_session() {
        let cache = SudoCredentialCache::default();
        let now = Instant::now();
        let host_a = key("connection-a", "host-a");
        let host_b = key("connection-b", "host-b");
        cache.cache(
            host_a.clone(),
            "session-a".to_string(),
            "secret-a".to_string(),
            now,
        );
        cache.cache(
            host_b.clone(),
            "session-b".to_string(),
            "secret-b".to_string(),
            now,
        );

        assert_eq!(
            cache.get(&host_a, now).map(|value| value.to_string()),
            Some("secret-a".to_string())
        );
        assert_eq!(
            cache.get(&host_b, now).map(|value| value.to_string()),
            Some("secret-b".to_string())
        );
        assert!(cache.get(&key("connection-c", "host-c"), now).is_none());
        assert!(
            cache
                .get(&key("connection-a", "replacement-host"), now)
                .is_none(),
            "editing a connection to another host must not reuse its old credential"
        );

        cache.clear_for_session("session-a");
        assert!(cache.get(&host_a, now).is_none());
        assert_eq!(
            cache.get(&host_b, now).map(|value| value.to_string()),
            Some("secret-b".to_string())
        );

        assert!(
            cache
                .get(
                    &host_b,
                    now + SUDO_CREDENTIAL_TTL + Duration::from_millis(1)
                )
                .is_none()
        );
    }
}
