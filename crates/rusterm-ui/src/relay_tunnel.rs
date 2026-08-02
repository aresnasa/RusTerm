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
use rusterm_db::{Database, RelayHistoryEntry};
use rusterm_relay::{
    ExecOutcome, ExecutorError, HistoryCursor, HistoryPage, HistoryQuery, HostInfo, RelayExecutor,
    RelayHandle, RelayHistoryRecord, RelayHistoryStore, run as run_relay,
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
//
// The lease TTL is a local upper bound, not a mirror of the remote sudo
// `timestamp_timeout`. A longer window (30 min) accommodates hosts whose
// sudoers raise the timeout above the 15-min distro default, and the lease
// is *refreshed* on every successful API use (see [`SudoCredentialCache::touch`])
// so that frequent API calls keep it alive far beyond the initial write.
// The remote sudo timestamp is the real authority: if it expires, the
// submitted password is rejected and [`SudoCredentialCache::clear_key`] drops
// the lease.
const SUDO_CREDENTIAL_TTL: Duration = Duration::from_secs(30 * 60);

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

/// Outcome of a sudo credential lookup, distinguishing the three failure
/// modes the elevated executor must report differently.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SudoCredentialLookup {
    /// A live lease was found and the credential is returned.
    Hit(Zeroizing<String>),
    /// A lease existed but the TTL elapsed; it has been evicted.
    Expired,
    /// No lease was ever cached for this host key.
    Missing,
}

impl SudoCredentialCache {
    fn cache(
        &self,
        key: SudoCredentialKey,
        source_session_id: String,
        value: String,
        now: Instant,
    ) {
        let expires_at = now + SUDO_CREDENTIAL_TTL;
        tracing::info!(
            "[SUDO-LEASE] write connection_id={} host={} port={} user={} session={} expires_in_secs={}",
            key.connection_id,
            key.host,
            key.port,
            key.username,
            source_session_id,
            SUDO_CREDENTIAL_TTL.as_secs()
        );
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(
                key,
                CachedSudoCredential {
                    value: Zeroizing::new(value),
                    source_session_id,
                    expires_at,
                },
            );
        }
    }

    #[cfg(test)]
    fn get(&self, key: &SudoCredentialKey, now: Instant) -> Option<Zeroizing<String>> {
        match self.get_with_status(key, now) {
            SudoCredentialLookup::Hit(v) => Some(v),
            SudoCredentialLookup::Expired | SudoCredentialLookup::Missing => None,
        }
    }

    fn get_with_status(&self, key: &SudoCredentialKey, now: Instant) -> SudoCredentialLookup {
        let mut entries = match self.entries.write() {
            Ok(g) => g,
            Err(_) => return SudoCredentialLookup::Missing,
        };
        let Some(entry) = entries.get(key) else {
            return SudoCredentialLookup::Missing;
        };
        if entry.expires_at <= now {
            tracing::info!(
                "[SUDO-LEASE] miss expired connection_id={} host={} port={} user={} session={}",
                key.connection_id,
                key.host,
                key.port,
                key.username,
                entry.source_session_id
            );
            entries.remove(key);
            return SudoCredentialLookup::Expired;
        }
        tracing::info!(
            "[SUDO-LEASE] hit connection_id={} host={} port={} user={} session={}",
            key.connection_id,
            key.host,
            key.port,
            key.username,
            entry.source_session_id
        );
        SudoCredentialLookup::Hit(Zeroizing::new(entry.value.to_string()))
    }

    /// Refresh the lease TTL after the elevated executor successfully used
    /// the cached credential. Without this, a long-lived API workflow that
    /// makes calls every few minutes would still see the lease expire 15
    /// minutes after the *initial* OneKey submission, even though the remote
    /// sudo timestamp is being kept alive by each successful `sudo -S`.
    fn touch(&self, key: &SudoCredentialKey, now: Instant) {
        let expires_at = now + SUDO_CREDENTIAL_TTL;
        let mut entries = match self.entries.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(entry) = entries.get_mut(key) {
            entry.expires_at = expires_at;
            tracing::info!(
                "[SUDO-LEASE] refresh connection_id={} host={} port={} user={} session={} expires_in_secs={}",
                key.connection_id,
                key.host,
                key.port,
                key.username,
                entry.source_session_id,
                SUDO_CREDENTIAL_TTL.as_secs()
            );
        }
    }

    fn clear_for_session(&self, source_session_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            let before = entries.len();
            entries.retain(|_, entry| entry.source_session_id != source_session_id);
            let removed = before - entries.len();
            if removed > 0 {
                tracing::info!(
                    "[SUDO-LEASE] clear_for_session session={} removed={}",
                    source_session_id,
                    removed
                );
            }
        }
    }

    fn clear_key(&self, key: &SudoCredentialKey) {
        let mut removed = false;
        if let Ok(mut entries) = self.entries.write() {
            removed = entries.remove(key).is_some();
        }
        if removed {
            tracing::info!(
                "[SUDO-LEASE] clear_key connection_id={} host={} port={} user={}",
                key.connection_id,
                key.host,
                key.port,
                key.username
            );
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

// ── Relay command-history store ─────────────────────────────────────────────
//
// Persists every command dispatched by the relay into the same SQLite DB
// the rest of the app uses (`rusterm.db` under the data dir). Like
// `AppRelayExecutor`, the DB is opened on demand per call — matching the
// app's existing pattern (see `app.rs` history lookups) and avoiding a
// long-lived shared handle whose lifecycle would need wiring through
// `AppState`. Each `Database::open` is cheap (file + schema check), and the
// relay is rate-limited so per-call opens are bounded.

/// Resolve the shared SQLite path used everywhere else in the app. Returns
/// `None` only when the platform has no data dir (extremely rare); callers
/// fall back to the in-memory `NullHistoryStore` in that case.
fn shared_db_path() -> Option<std::path::PathBuf> {
    let dir = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    Some(dir.join("rusterm").join("rusterm.db"))
}

async fn open_db() -> Option<Database> {
    let path = shared_db_path()?;
    match Database::open(Some(path)).await {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!("[relay] failed to open history DB: {e:#}");
            None
        }
    }
}

/// `RelayHistoryStore` backed by `rusterm_db::Database`. Records land in the
/// `relay_history` table (added in `rusterm-db`'s schema) and are queryable
/// via `GET /api/v1/history`.
#[derive(Debug, Default)]
pub struct AppRelayHistoryStore;

#[async_trait]
impl RelayHistoryStore for AppRelayHistoryStore {
    async fn record(&self, record: RelayHistoryRecord) -> anyhow::Result<()> {
        let db = open_db()
            .await
            .ok_or_else(|| anyhow::anyhow!("history DB unavailable"))?;
        db.save_relay_history(RelayHistoryEntry {
            id: record.id,
            account: record.account,
            host_id: record.host_id,
            command: record.command,
            elevated: record.elevated,
            exit_code: record.exit_code,
            duration_ms: record.duration_ms.map(|d| d as i64),
            timed_out: record.timed_out,
            created_at: record.created_at,
        })
        .await
    }

    async fn list(
        &self,
        query: &HistoryQuery,
        before: Option<&HistoryCursor>,
    ) -> anyhow::Result<HistoryPage> {
        let db = open_db()
            .await
            .ok_or_else(|| anyhow::anyhow!("history DB unavailable"))?;
        let page = db
            .list_relay_history(
                query.account.as_deref(),
                query.host_id.as_deref(),
                query.query.as_deref(),
                before
                    .map(|c| rusterm_db::RelayHistoryCursor {
                        created_at: c.created_at.clone(),
                        id: c.id.clone(),
                    })
                    .as_ref(),
                query.limit.unwrap_or(50),
            )
            .await?;
        Ok(HistoryPage {
            entries: page
                .entries
                .into_iter()
                .map(|e| RelayHistoryRecord {
                    id: e.id,
                    account: e.account,
                    host_id: e.host_id,
                    command: e.command,
                    elevated: e.elevated,
                    exit_code: e.exit_code,
                    duration_ms: e.duration_ms.map(|d| d as u64),
                    timed_out: e.timed_out,
                    created_at: e.created_at,
                    // `success` isn't stored separately in the DB; derive it
                    // from exit_code so the listing flags failures correctly.
                    success: e.exit_code == Some(0),
                })
                .collect(),
            next_cursor: page.next_cursor.map(|c| HistoryCursor {
                created_at: c.created_at,
                id: c.id,
            }),
        })
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let db = open_db()
            .await
            .ok_or_else(|| anyhow::anyhow!("history DB unavailable"))?;
        db.delete_relay_history(id).await
    }
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
                let lookup = sudo_credentials().get_with_status(&credential_key, Instant::now());
                let credential = match lookup {
                    SudoCredentialLookup::Hit(value) => value,
                    SudoCredentialLookup::Expired => {
                        let _ = handle.disconnect().await;
                        return Err(ExecutorError::ElevationRequired(crate::i18n::t(
                            "relay.sudo_authorization_expired",
                        )));
                    }
                    SudoCredentialLookup::Missing => {
                        let _ = handle.disconnect().await;
                        return Err(ExecutorError::ElevationRequired(crate::i18n::t(
                            "relay.sudo_authorization_unavailable",
                        )));
                    }
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
                    return Err(ExecutorError::ElevationRequired(crate::i18n::t(
                        "relay.sudo_authorization_rejected",
                    )));
                }
                // The remote sudo timestamp was just refreshed by this
                // successful `sudo -S`; mirror that by refreshing the local
                // lease too, so subsequent API calls within the new window
                // don't need to re-prompt the user.
                sudo_credentials().touch(&credential_key, Instant::now());
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
            truncated: result.truncated,
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
    // Persist executed commands so they can be retrieved via
    // `GET /api/v1/history` and re-run. Falls back to a no-op store when the
    // data dir can't be resolved (degraded mode) so the relay still starts.
    let history: Arc<dyn RelayHistoryStore> = match shared_db_path() {
        Some(_) => Arc::new(AppRelayHistoryStore),
        None => {
            tracing::warn!("[relay] no data dir available; command history disabled");
            Arc::new(rusterm_relay::NullHistoryStore)
        }
    };
    let handle = runtime_handle();
    let (result_tx, result_rx) = std::sync::mpsc::channel::<anyhow::Result<RelayHandle>>();
    handle.spawn(async move {
        let _ = result_tx.send(run_relay(config, executor, history).await);
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
        Err(e) => Err(crate::i18n::tf(
            "relay.start_timeout_or_runtime_wedged",
            &[("error", &e)],
        )),
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

    /// Reproduces the user's 2026-08-02 incident: OneKey auto-submitted a
    /// remembered sudo password at 15:28, the API tried to use it at 15:46
    /// (18 minutes later), and got `elevation_required`. The local lease
    /// TTL was 15 minutes and there was no refresh-on-use, so the lease had
    /// expired ~3 minutes before the API call.
    ///
    /// This test pins the *failure* mode (expired → `Expired` status) so the
    /// fix below has a red signal to turn green.
    #[test]
    fn sudo_lease_expires_after_ttl_when_never_refreshed() {
        let cache = SudoCredentialCache::default();
        let now = Instant::now();
        let host = key("connection-1", "bidbot-prod");
        cache.cache(
            host.clone(),
            "session-04a5aa98".to_string(),
            "p@ssw0rd".to_string(),
            now,
        );

        // Within the window: hit.
        assert!(matches!(
            cache.get_with_status(&host, now + Duration::from_secs(60)),
            SudoCredentialLookup::Hit(_)
        ));

        // One nanosecond before the TTL boundary: still alive (the eviction
        // predicate is `expires_at <= now`, so the boundary itself is the
        // first expired instant).
        assert!(matches!(
            cache.get_with_status(&host, now + SUDO_CREDENTIAL_TTL - Duration::from_nanos(1)),
            SudoCredentialLookup::Hit(_)
        ));

        // Exactly at the TTL boundary with no intervening refresh: expired
        // and evicted. This is the 15:46 API failure (the lease was written
        // at 15:28 with a 15-min TTL, so at 15:43:08 it expired; the API
        // call at 15:46 found nothing).
        assert_eq!(
            cache.get_with_status(&host, now + SUDO_CREDENTIAL_TTL),
            SudoCredentialLookup::Expired
        );
        // Subsequent lookups report Missing (the entry was evicted).
        assert_eq!(
            cache.get_with_status(&host, now + SUDO_CREDENTIAL_TTL + Duration::from_millis(2)),
            SudoCredentialLookup::Missing
        );
    }

    /// Pins the fix: when the elevated executor successfully uses a cached
    /// credential, it calls `touch` to refresh the TTL. A long-lived API
    /// workflow that makes calls every few minutes keeps the lease alive
    /// far beyond the initial write's expiry, mirroring the remote sudo
    /// timestamp that each successful `sudo -S` refreshes.
    #[test]
    fn sudo_lease_is_refreshed_on_successful_use_via_touch() {
        let cache = SudoCredentialCache::default();
        let now = Instant::now();
        let host = key("connection-1", "bidbot-prod");
        cache.cache(
            host.clone(),
            "session-04a5aa98".to_string(),
            "p@ssw0rd".to_string(),
            now,
        );

        // Simulate the API using the credential at t = TTL - 1 min (just
        // before the initial expiry). The executor calls `touch` on success.
        let first_use = now + SUDO_CREDENTIAL_TTL - Duration::from_secs(60);
        assert!(matches!(
            cache.get_with_status(&host, first_use),
            SudoCredentialLookup::Hit(_)
        ));
        cache.touch(&host, first_use);

        // Without touch, the lease would expire at `now + TTL`. With touch,
        // the new expiry is `first_use + TTL`. A lookup at `now + TTL +
        // 30s` (past the original expiry, within the refreshed one) must
        // still hit.
        let after_original_expiry = now + SUDO_CREDENTIAL_TTL + Duration::from_secs(30);
        assert!(matches!(
            cache.get_with_status(&host, after_original_expiry),
            SudoCredentialLookup::Hit(_)
        ));

        // A second successful use refreshes again.
        let second_use = first_use + SUDO_CREDENTIAL_TTL - Duration::from_secs(60);
        assert!(matches!(
            cache.get_with_status(&host, second_use),
            SudoCredentialLookup::Hit(_)
        ));
        cache.touch(&host, second_use);

        // Past the *first* refreshed expiry, within the second refreshed one.
        let past_first_refresh = first_use + SUDO_CREDENTIAL_TTL + Duration::from_secs(30);
        assert!(past_first_refresh < second_use + SUDO_CREDENTIAL_TTL);
        assert!(matches!(
            cache.get_with_status(&host, past_first_refresh),
            SudoCredentialLookup::Hit(_)
        ));
    }

    /// `touch` on a non-existent key is a no-op (defensive: the executor
    /// only calls touch after a hit, but a stray call must not panic or
    /// insert an empty entry).
    #[test]
    fn sudo_lease_touch_on_missing_key_is_a_noop() {
        let cache = SudoCredentialCache::default();
        let now = Instant::now();
        let host = key("connection-1", "ghost");
        cache.touch(&host, now);
        assert_eq!(
            cache.get_with_status(&host, now),
            SudoCredentialLookup::Missing
        );
    }

    /// Bridges the OneKey submission path to the elevated executor lookup
    /// path. `cache_sudo_credential` is called with the *saved*
    /// `ConnectionConfig` stored in `session_configs[session_id]`; the
    /// executor derives its lookup key from `read_connections()`, which is
    /// the same saved `ConnectionConfig` mirrored into the process
    /// registry. Both paths must derive the same `SudoCredentialKey`, or
    /// the API will report `elevation_required` even when the user just ran
    /// sudo in their terminal.
    ///
    /// This test pins that the key derivation is identical for a given saved
    /// SSH connection, regardless of which entry point computes it.
    #[test]
    fn sudo_credential_key_is_identical_for_onekey_cache_and_executor_lookup() {
        let saved = ConnectionConfig {
            id: "42725271-d0cb-4118-98c3-2198e6b3c654".to_string(),
            name: "bidbot-prod".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "bidbot.example.com".to_string(),
                port: 22,
                username: "aresnasa".to_string(),
                auth: rusterm_core::config::SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy: None,
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: rusterm_core::config::default_host_key_policy(),
            }),
            group: None,
            tags: Vec::new(),
            onekey: true,
            login_script: None,
        };

        // OneKey submission path: `cache_sudo_credential(connection, session_id, value)`.
        // We can't call the free fn here because it writes to the process-wide
        // static cache; instead, derive the key the same way it does and write
        // to a local cache, then derive the *same* key the executor would and
        // read it back.
        let cache = SudoCredentialCache::default();
        let now = Instant::now();
        let write_key = sudo_credential_key(&saved).expect("SSH connection yields a key");
        cache.cache(
            write_key.clone(),
            "runtime-session-04a5aa98".to_string(),
            "p@ssw0rd".to_string(),
            now,
        );

        // Executor lookup path: `read_connections()` returns the same saved
        // config; the executor derives the key with the same function.
        let lookup_key = sudo_credential_key(&saved).expect("key derivation is deterministic");
        assert_eq!(write_key, lookup_key);
        assert!(matches!(
            cache.get_with_status(&lookup_key, now + Duration::from_secs(60)),
            SudoCredentialLookup::Hit(_)
        ));
    }

    /// End-to-end cache test through the public `cache_sudo_credential`
    /// entry point (the one `apply_onekey_popup` and `on_onekey_select`
    /// call after a sudo OneKey submission). Verifies that a credential
    /// written via the public API is immediately retrievable with the key
    /// the executor derives from the same saved connection — closing the
    /// loop between the OneKey submission path and the elevated executor
    /// lookup path.
    #[test]
    fn cache_sudo_credential_public_api_round_trips_through_executor_lookup() {
        // Unique connection id so this test can't collide with the
        // process-wide static cache used by other tests.
        let saved = ConnectionConfig {
            id: "round-trip-9c4e".to_string(),
            name: "round-trip-host".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "round-trip.example".to_string(),
                port: 2222,
                username: "rtuser".to_string(),
                auth: rusterm_core::config::SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy: None,
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: rusterm_core::config::default_host_key_policy(),
            }),
            group: None,
            tags: Vec::new(),
            onekey: true,
            login_script: None,
        };

        // Clean up any prior entry for this key (idempotent across re-runs).
        let key = sudo_credential_key(&saved).expect("SSH connection yields a key");
        sudo_credentials().clear_key(&key);

        // OneKey submission path writes via the public entry point.
        cache_sudo_credential(&saved, "rt-session", "rt-secret");

        // Executor lookup path derives the same key and finds the credential.
        let now = Instant::now();
        match sudo_credentials().get_with_status(&key, now) {
            SudoCredentialLookup::Hit(value) => {
                assert_eq!(value.as_str(), "rt-secret");
            }
            other => panic!("expected Hit, got {:?}", other),
        }

        // Cleanup: remove the entry so it doesn't leak to other tests.
        sudo_credentials().clear_key(&key);
    }
}
