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
    RelayHandle, RelayHistoryRecord, RelayHistoryStore, run as run_relay, split_host_selector,
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

// ── Live session registry (for relay exec reuse) ─────────────────────────
//
// When a user has an SSH session open in the UI (already navigated through
// the bastion menu to the target node), the relay executor can reuse that
// live PTY instead of opening a fresh connection. This registry mirrors
// the UI's `ssh_sessions` + `session_configs` maps so the executor can find
// a connected session by host_id.

/// A live session entry available for command reuse.
#[derive(Clone)]
pub struct LiveSessionEntry {
    /// The SSH session handle (holds the authenticated russh client +
    /// output tap mechanism).
    pub session: rusterm_ssh::SshSession,
    /// The input sender for injecting keystrokes into the PTY.
    pub input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// The connection id this session was opened from.
    pub connection_id: String,
    /// Whether the connection either has no login script or its login
    /// script ran to completion in this session. Sessions whose script
    /// failed/aborted (the operator may have navigated the bastion menu
    /// manually) are still published so a session-qualified selector can
    /// target the exact tab, but plain selectors skip them: their PTY may
    /// be sitting on an unknown node or still at the bastion menu.
    pub login_script_completed: bool,
}

static SESSION_REGISTRY: OnceLock<RwLock<Vec<LiveSessionEntry>>> = OnceLock::new();

fn session_registry() -> &'static RwLock<Vec<LiveSessionEntry>> {
    SESSION_REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// Mirror live sessions into the shared registry. Called by the UI whenever
/// sessions are opened, closed, or their connection mapping changes.
pub fn sync_session_registry(entries: Vec<LiveSessionEntry>) {
    if let Ok(mut guard) = session_registry().write() {
        *guard = entries;
    }
}

/// Find a live connected session for the given host selector.
///
/// Selectors of the form `{connection_id}@{session_id}` route to one exact
/// terminal tab: with jumpserver/bastion hosts, several tabs of the same
/// connection can sit on *different* target nodes, so a session-qualified
/// selector must never fall back to a sibling tab — that is precisely how
/// commands end up on the wrong node. If the exact tab is gone, we return
/// `None`; the caller decides between the fresh-connection slow path
/// (direct hosts) and an explicit error (bastion hosts, see
/// [`live_session_required`]).
///
/// Plain selectors keep the legacy behaviour: first entry matching the
/// connection id, then a fallback by connection name — but only across
/// sessions whose login script completed (or that have none). A tab where
/// the operator navigated the bastion menu manually is only reachable via
/// its exact session-qualified selector.
fn find_live_session(host_selector: &str) -> Option<LiveSessionEntry> {
    let guard = session_registry().read().ok()?;
    let keys: Vec<(String, String, bool)> = guard
        .iter()
        .map(|entry| {
            (
                entry.connection_id.clone(),
                entry.session.session_id().to_string(),
                entry.login_script_completed,
            )
        })
        .collect();
    let index = find_live_session_index(host_selector, &keys, &read_connections())?;
    guard.get(index).cloned()
}

/// Testable core of [`find_live_session`]: resolves a selector to an index
/// into `entries`, given as `(connection_id, session_id,
/// login_script_completed)` triples (a plain projection of the registry, so
/// tests don't need to construct a real [`rusterm_ssh::SshSession`]).
fn find_live_session_index(
    host_selector: &str,
    entries: &[(String, String, bool)],
    connections: &[ConnectionConfig],
) -> Option<usize> {
    let (base, session_id) = split_host_selector(host_selector);
    if let Some(session_id) = session_id {
        // Exact-tab routing: the caller chose this specific PTY, so login
        // script state is irrelevant — a manually navigated tab is exactly
        // what the operator wants to reuse.
        return entries
            .iter()
            .position(|(conn_id, sess_id, _)| sess_id == session_id && conn_id == base);
    }

    entries
        .iter()
        .position(|(conn_id, _, plain_ok)| *plain_ok && conn_id == host_selector)
        .or_else(|| {
            // Fall back to matching by connection name.
            let target_name = connections
                .iter()
                .find(|c| c.id == host_selector)
                .map(|c| c.name.clone())?;
            entries.iter().position(|(conn_id, _, plain_ok)| {
                *plain_ok
                    && connections
                        .iter()
                        .any(|c| &c.id == conn_id && c.name == target_name)
            })
        })
}

/// Whether a host selector may only execute inside its exact live TTY.
///
/// True for session-qualified selectors (`{connection_id}@{tab_id}`) whose
/// base connection has a login script — i.e. bastion/jumpserver hosts. The
/// "currently on node X" state lives solely inside that tab's PTY channel:
/// a fresh SSH connection (or any other tab) lands back on the bastion
/// entry host and returns output from the wrong machine, so the caller must
/// fail loudly instead of falling back. Direct hosts keep the
/// fresh-connection fallback — a new connection reaches the same machine.
fn live_session_required(host_selector: &str, connections: &[ConnectionConfig]) -> bool {
    let (base, session_id) = split_host_selector(host_selector);
    if session_id.is_none() {
        return false;
    }
    connections.iter().any(|c| {
        (c.id == base || c.name == base)
            && c.login_script
                .as_deref()
                .is_some_and(|script| !script.trim().is_empty())
    })
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

/// Lowercase substrings in sudo's diagnostics that indicate the failure was
/// about authorization (missing/rejected password, no TTY, not in sudoers)
/// rather than the wrapped command itself exiting non-zero.
const SUDO_FAILURE_NEEDLES: &[&str] = &[
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
];

fn output_matches_sudo_failure(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    SUDO_FAILURE_NEEDLES
        .iter()
        .any(|needle| lower.contains(needle))
}

fn sudo_authorization_failed(result: &ExecResult) -> bool {
    if result.exit_code == Some(0) {
        return false;
    }
    output_matches_sudo_failure(&result.stderr_string())
}

/// Detect a `sudo -n` authorization failure from a live-PTY run. Unlike the
/// dedicated exec channel, a PTY merges stderr into the main output stream,
/// so sudo's diagnostics arrive in the outcome's stdout and
/// [`sudo_authorization_failed`] (which only inspects stderr) would miss
/// them.
fn live_sudo_failed(exit_code: Option<u32>, merged_output: &str) -> bool {
    if exit_code == Some(0) {
        return false;
    }
    output_matches_sudo_failure(merged_output)
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

/// Runs validated relay commands against saved SSH hosts. Tries to reuse
/// an already-connected UI session first (which has already navigated the
/// bastion menu); falls back to a fresh SSH connection if no live session
/// is available.
#[derive(Debug, Default)]
pub struct AppRelayExecutor;

#[derive(Debug)]
enum LiveExecError {
    /// No bytes were queued to the SSH writer; a fresh connection is safe.
    BeforeSend(String),
    /// The command was queued, so retrying could execute it twice.
    AfterSend(String),
}

impl std::fmt::Display for LiveExecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeSend(message) | Self::AfterSend(message) => formatter.write_str(message),
        }
    }
}

const MAX_LIVE_EXEC_OUTPUT: usize = 1024 * 1024;

/// Append one PTY chunk to the bounded response body while independently
/// retaining enough tail bytes to detect a sentinel that arrives after the
/// body cap.
fn capture_live_exec_output(
    raw: &mut Vec<u8>,
    marker_scan: &mut Vec<u8>,
    data: &[u8],
    rc_tag: &str,
) -> (Option<u32>, bool) {
    let remaining_capacity = MAX_LIVE_EXEC_OUTPUT.saturating_sub(raw.len());
    let truncated = data.len() > remaining_capacity;
    raw.extend_from_slice(&data[..data.len().min(remaining_capacity)]);

    marker_scan.extend_from_slice(data);
    if let Some((_, code)) = rusterm_ssh::direct::find_complete_rc_marker(marker_scan, rc_tag) {
        return (Some(code), truncated);
    }

    let scan_tail = rc_tag.len() + 32;
    if marker_scan.len() > scan_tail {
        marker_scan.drain(..marker_scan.len() - scan_tail);
    }
    (None, truncated)
}

async fn exec_via_live_session(
    entry: &LiveSessionEntry,
    command: &str,
    timeout: Duration,
) -> Result<ExecOutcome, LiveExecError> {
    use rusterm_ssh::direct::{find_complete_rc_marker, random_sentinel_hex, strip_echoed_wrapper};

    let started = Instant::now();
    let mut tap = tokio::time::timeout(timeout, entry.session.begin_output_tap())
        .await
        .map_err(|_| {
            LiveExecError::BeforeSend(
                "timed out waiting for another live-session command to finish".to_string(),
            )
        })?
        .map_err(|error| LiveExecError::BeforeSend(error.to_string()))?;
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(LiveExecError::BeforeSend(
            "request deadline elapsed before the command was sent".to_string(),
        ));
    }

    let sentinel = format!("RUSTERM_PTY_{}", random_sentinel_hex());
    let rc_tag = format!("{sentinel}_RC_");
    let wrapped = format!(
        "{{ {command} ; }} ; __rc=$? ; printf '\\n{tag}%d\\n' \"$__rc\"\r",
        command = command,
        tag = rc_tag,
    );

    tracing::info!(
        "[relay] reusing live session {} for exec",
        entry.session.session_id()
    );
    entry.input_tx.send(wrapped.into_bytes()).map_err(|error| {
        LiveExecError::BeforeSend(format!("failed to queue command for live session: {error}"))
    })?;

    let deadline = tokio::time::sleep(remaining);
    tokio::pin!(deadline);
    let mut raw = Vec::new();
    let mut marker_scan = Vec::new();
    let mut marker_code = None;
    let mut timed_out = false;
    let mut truncated = false;

    loop {
        tokio::select! {
            biased;
            data = tap.recv() => {
                let Some(data) = data else {
                    entry.session.mark_relay_exec_unusable();
                    return Err(LiveExecError::AfterSend(
                        "live session output closed after the command was queued; command status is unknown"
                            .to_string(),
                    ));
                };
                // Keep scanning after the response body reaches its cap so a
                // large-output command can still complete instead of timing
                // out merely because its sentinel arrived after 1 MiB.
                let (code, chunk_truncated) =
                    capture_live_exec_output(&mut raw, &mut marker_scan, &data, &rc_tag);
                truncated |= chunk_truncated;
                if code.is_some() {
                    marker_code = code;
                    break;
                }
            }
            _ = &mut deadline => {
                timed_out = true;
                entry.session.mark_relay_exec_unusable();
                break;
            }
        }
    }

    let marker = find_complete_rc_marker(&raw, &rc_tag);
    let exit_code = marker.map(|(_, code)| code).or(marker_code);
    let stdout = if let Some((tag_idx, _)) = marker {
        let before = String::from_utf8_lossy(&raw[..tag_idx]);
        let before = before.trim_end_matches(['\r', '\n', ' ']);
        strip_echoed_wrapper(before)
    } else {
        strip_echoed_wrapper(&String::from_utf8_lossy(&raw))
    };

    Ok(ExecOutcome {
        exit_code,
        stdout,
        stderr: String::new(),
        timed_out,
        truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

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
        // ── Fast path: reuse a live UI session if one exists. ──
        // Non-elevated commands run verbatim inside the live PTY. Elevated
        // commands on a *bastion* session-qualified selector run as
        // `sudo -n` inside the same PTY — a fresh connection would land on
        // the bastion entry host, not the node the tab navigated to. No
        // password is ever injected into the live PTY (its echo would leak
        // the secret); if `sudo -n` is refused the caller gets a clear
        // `elevation_required` explaining how to refresh the timestamp.
        // Elevated execution on *direct* hosts keeps the dedicated
        // connection path, where `sudo -S` reads the cached credential from
        // stdin without echo.
        let bastion_live_only = live_session_required(host_id, &read_connections());
        if let Some(entry) = find_live_session(host_id) {
            if !elevated {
                match exec_via_live_session(&entry, command, timeout).await {
                    Ok(outcome) => return Ok(outcome),
                    Err(LiveExecError::BeforeSend(error)) => {
                        if bastion_live_only {
                            tracing::error!(
                                "[relay] live session required for bastion selector {host_id} but unavailable before send: {error}; refusing bastion fallback"
                            );
                            return Err(ExecutorError::Exec(format!(
                                "[live-session-required] {}: {error}",
                                crate::i18n::t("relay.live_session_required"),
                            )));
                        }
                        tracing::warn!(
                            "[relay] live session unavailable before send for {host_id}: {error}; using a fresh connection"
                        );
                    }
                    Err(LiveExecError::AfterSend(error)) => {
                        tracing::error!(
                            "[relay] live-session command status unknown for {host_id}: {error}; refusing automatic retry"
                        );
                        return Err(ExecutorError::Exec(format!(
                            "{error}; the command was not retried"
                        )));
                    }
                }
            } else if bastion_live_only {
                let non_interactive_command = sudo_command(command, false);
                match exec_via_live_session(&entry, &non_interactive_command, timeout).await {
                    Ok(outcome) => {
                        // A PTY merges stderr into stdout, so sudo's
                        // diagnostics land in `outcome.stdout`.
                        if live_sudo_failed(outcome.exit_code, &outcome.stdout) {
                            tracing::warn!(
                                "[relay] non-interactive sudo refused inside live session {host_id}; not injecting a password into a live PTY"
                            );
                            return Err(ExecutorError::ElevationRequired(crate::i18n::t(
                                "relay.live_sudo_unavailable",
                            )));
                        }
                        return Ok(outcome);
                    }
                    Err(LiveExecError::BeforeSend(error)) => {
                        tracing::error!(
                            "[relay] live session required for bastion selector {host_id} but unavailable before send: {error}; refusing bastion fallback"
                        );
                        return Err(ExecutorError::Exec(format!(
                            "[live-session-required] {}: {error}",
                            crate::i18n::t("relay.live_session_required"),
                        )));
                    }
                    Err(LiveExecError::AfterSend(error)) => {
                        tracing::error!(
                            "[relay] live-session command status unknown for {host_id}: {error}; refusing automatic retry"
                        );
                        return Err(ExecutorError::Exec(format!(
                            "{error}; the command was not retried"
                        )));
                    }
                }
            }
            // Elevated + direct host: fall through to the dedicated
            // connection below, which can answer sudo's password prompt
            // over a no-echo exec channel.
        } else if bastion_live_only {
            // A session-qualified selector on a bastion host with no
            // matching live tab: the tab was closed (or its selector is
            // stale). A fresh connection would land on the bastion entry
            // host — the exact wrong-node bug this guard exists for.
            tracing::error!(
                "[relay] live session required for bastion selector {host_id} but no live tab matched; refusing bastion fallback"
            );
            return Err(ExecutorError::Exec(format!(
                "[live-session-required] {}",
                crate::i18n::t("relay.live_session_required"),
            )));
        }

        // ── Slow path: fresh SSH connection (existing behaviour). ──
        // Reached by plain selectors, elevated execution on direct hosts,
        // and session-qualified selectors on *direct* hosts whose tab has
        // closed (strip the suffix and connect to the base host — a fresh
        // connection reaches the same machine). Bastion selectors never
        // reach this point: with a live tab they are served inside that
        // PTY above (elevated runs use `sudo -n`), and without one the
        // guard above rejects them, because a fresh connection would land
        // on the bastion entry host instead of the node the tab had
        // navigated to.
        let (base_host_id, _) = split_host_selector(host_id);
        let (credential_key, ssh, login_script) = read_connections()
            .into_iter()
            .find_map(|conn| {
                if conn.id == base_host_id || conn.name == base_host_id {
                    let key = sudo_credential_key(&conn)?;
                    let script = conn.login_script.clone();
                    match conn.kind {
                        ConnectionKind::Ssh(ssh) => Some((key, ssh, script)),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| ExecutorError::UnknownHost(host_id.to_string()))?;

        // Parse the per-connection login script DSL (if any). Bastion hosts
        // like QiZhi (齐治交互终端) present an interactive asset-selection
        // menu after SSH login; the login script automates navigating that
        // menu (expect category list → send number → …) before the actual
        // command runs on the target node behind the bastion.
        let login_steps = match login_script.as_deref() {
            Some(text) if !text.trim().is_empty() => match rusterm_core::parse_login_script(text) {
                Ok(steps) if !steps.is_empty() => {
                    tracing::info!(
                        "[relay] host {host_id} has login script ({} steps)",
                        steps.len()
                    );
                    steps
                }
                Ok(_) => Vec::new(),
                Err(e) => {
                    tracing::warn!(
                        "[relay] host {host_id} login script parse error: {e}; ignoring"
                    );
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };

        let started = Instant::now();
        let handle = connect_direct(&ssh, DirectConnectOptions::default())
            .await
            .map_err(|e| ExecutorError::Connect(format!("{e:#}")))?;

        // All exec calls go through `exec_with_fallback`: a normal SSH host
        // answers the exec channel and the fallback is a no-op. Bastion
        // hosts (JumpServer etc.) reject exec requests with messages like
        // "exec request failed, try username/server/account as login name";
        // `exec_with_fallback` detects that and transparently retries the
        // command through a PTY + shell channel with sentinel markers — the
        // same interactive path a human operator would use.
        let result = if elevated {
            let non_interactive_command = sudo_command(command, false);
            let first = handle
                .exec_with_fallback_and_login(&non_interactive_command, None, timeout, &login_steps)
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
                    .exec_with_fallback_and_login(
                        &password_command,
                        Some(stdin.as_bytes()),
                        timeout,
                        &login_steps,
                    )
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
                .exec_with_fallback_and_login(command, None, timeout, &login_steps)
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

    fn conn(id: &str, name: &str) -> ConnectionConfig {
        ConnectionConfig {
            id: id.to_string(),
            name: name.to_string(),
            kind: ConnectionKind::Shell(rusterm_core::config::ShellConfig {
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                working_dir: None,
            }),
            group: None,
            tags: Vec::new(),
            onekey: false,
            login_script: None,
        }
    }

    fn live_key(conn_id: &str, sess_id: &str) -> (String, String, bool) {
        (conn_id.to_string(), sess_id.to_string(), true)
    }

    /// Registry entry for a tab whose login script failed/aborted or that
    /// the operator navigated manually — reachable only via its exact
    /// session-qualified selector.
    fn manual_live_key(conn_id: &str, sess_id: &str) -> (String, String, bool) {
        (conn_id.to_string(), sess_id.to_string(), false)
    }

    fn bastion_conn(id: &str, name: &str) -> ConnectionConfig {
        ConnectionConfig {
            login_script: Some("expect \"Opt>\"\nsend \"1\"".to_string()),
            ..conn(id, name)
        }
    }

    /// A session-qualified selector must resolve to the exact tab — never a
    /// sibling tab of the same connection. Two jumpserver tabs of one
    /// connection can sit on different target nodes; returning the "first"
    /// tab is how commands land on the wrong node.
    #[test]
    fn session_qualified_selector_matches_exact_tab_only() {
        let entries = vec![live_key("conn-j", "tab-a"), live_key("conn-j", "tab-b")];
        let connections = vec![conn("conn-j", "jumpserver")];

        assert_eq!(
            find_live_session_index("conn-j@tab-b", &entries, &connections),
            Some(1)
        );
        assert_eq!(
            find_live_session_index("conn-j@tab-a", &entries, &connections),
            Some(0)
        );
        // Tab closed → no silent fallback to the surviving sibling tab; the
        // caller then either takes the fresh-connection slow path (direct
        // hosts) or fails loudly (bastion hosts, `live_session_required`).
        assert_eq!(
            find_live_session_index("conn-j@tab-gone", &entries, &connections),
            None
        );
        // Session id belonging to a different connection must not match.
        assert_eq!(
            find_live_session_index("conn-other@tab-a", &entries, &connections),
            None
        );
    }

    /// Plain (un-suffixed) selectors keep the legacy first-match and
    /// name-fallback behaviour.
    #[test]
    fn plain_selector_keeps_legacy_connection_matching() {
        let entries = vec![live_key("conn-a", "tab-1"), live_key("conn-b", "tab-2")];
        let connections = vec![conn("conn-a", "web"), conn("conn-a2", "web")];

        assert_eq!(
            find_live_session_index("conn-b", &entries, &connections),
            Some(1)
        );
        // Name fallback: conn-a2 shares the name "web" with conn-a, whose
        // session is live.
        assert_eq!(
            find_live_session_index("conn-a2", &entries, &connections),
            Some(0)
        );
        assert_eq!(
            find_live_session_index("conn-z", &entries, &connections),
            None
        );
    }

    /// A tab whose login script failed (or that was navigated manually)
    /// must stay reachable via its exact session-qualified selector — that
    /// PTY is the only place holding the "currently on node X" state — but
    /// plain selectors must skip it: its node is unknown to the executor.
    #[test]
    fn manually_navigated_tab_reachable_only_via_exact_selector() {
        let entries = vec![
            manual_live_key("conn-j", "tab-manual"),
            live_key("conn-j", "tab-scripted"),
        ];
        let connections = vec![bastion_conn("conn-j", "jumpserver")];

        // Exact selector hits the manually navigated tab.
        assert_eq!(
            find_live_session_index("conn-j@tab-manual", &entries, &connections),
            Some(0)
        );
        // Plain selector skips it and lands on the script-completed tab.
        assert_eq!(
            find_live_session_index("conn-j", &entries, &connections),
            Some(1)
        );

        // With only the manual tab live, a plain selector matches nothing
        // (including via the name fallback).
        let manual_only = vec![manual_live_key("conn-j", "tab-manual")];
        assert_eq!(
            find_live_session_index("conn-j", &manual_only, &connections),
            None
        );
        assert_eq!(
            find_live_session_index("conn-j@tab-manual", &manual_only, &connections),
            Some(0)
        );
    }

    /// Bastion selectors must never fall back to a fresh SSH connection:
    /// only the session-qualified selector of a login-script host demands a
    /// live tab. Plain selectors and direct hosts keep the slow-path
    /// fallback (a fresh connection reaches the same machine there).
    #[test]
    fn live_session_required_only_for_session_qualified_bastion_selectors() {
        let connections = vec![
            bastion_conn("conn-j", "jumpserver"),
            conn("conn-d", "direct"),
        ];

        // Bastion + session suffix → live tab required.
        assert!(live_session_required("conn-j@tab-1", &connections));
        // Name-based selector with suffix also counts.
        assert!(live_session_required("jumpserver@tab-1", &connections));
        // Plain bastion selector → slow path may replay the login script.
        assert!(!live_session_required("conn-j", &connections));
        // Direct host → fallback always allowed.
        assert!(!live_session_required("conn-d@tab-2", &connections));
        assert!(!live_session_required("conn-d", &connections));
        // Unknown host → the slow path reports UnknownHost as before.
        assert!(!live_session_required("conn-x@tab-3", &connections));

        // A blank login script does not make a host a bastion.
        let blank = vec![ConnectionConfig {
            login_script: Some("   ".to_string()),
            ..conn("conn-b", "blankscript")
        }];
        assert!(!live_session_required("conn-b@tab-4", &blank));
    }

    #[test]
    fn live_exec_detects_completion_marker_after_output_cap() {
        let rc_tag = "RUSTERM_TEST_RC_";
        let mut raw = Vec::new();
        let mut marker_scan = Vec::new();
        let oversized = vec![b'x'; MAX_LIVE_EXEC_OUTPUT + 128];

        let (code, truncated) =
            capture_live_exec_output(&mut raw, &mut marker_scan, &oversized, rc_tag);
        assert_eq!(code, None);
        assert!(truncated);
        assert_eq!(raw.len(), MAX_LIVE_EXEC_OUTPUT);

        let (code, truncated_again) = capture_live_exec_output(
            &mut raw,
            &mut marker_scan,
            format!("{rc_tag}17\r\n").as_bytes(),
            rc_tag,
        );
        assert_eq!(code, Some(17));
        assert!(truncated_again);
        assert_eq!(raw.len(), MAX_LIVE_EXEC_OUTPUT);
    }

    #[test]
    fn sudo_command_quotes_untrusted_command_without_embedding_a_password() {
        let command = "printf '%s' \"$HOME\"";
        let wrapped = sudo_command(command, true);
        assert_eq!(
            wrapped,
            "sudo -S -p '' -- sh -lc 'printf '\"'\"'%s'\"'\"' \"$HOME\"'"
        );
        assert!(!wrapped.contains("secret"));

        // The live-PTY elevated path relies on the non-interactive form:
        // `sudo -n` must fail fast instead of prompting inside the PTY.
        let non_interactive = sudo_command(command, false);
        assert_eq!(
            non_interactive,
            "sudo -n -- sh -lc 'printf '\"'\"'%s'\"'\"' \"$HOME\"'"
        );
    }

    #[test]
    fn sudo_failure_detection_does_not_treat_command_exit_one_as_auth_failure() {
        let command_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"application error".to_vec(),
            timed_out: false,
            truncated: false,
        };
        assert!(!sudo_authorization_failed(&command_failure));

        let auth_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"sudo: a password is required".to_vec(),
            timed_out: false,
            truncated: false,
        };
        assert!(sudo_authorization_failed(&auth_failure));

        let interactive_auth_failure = ExecResult {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"sudo: interactive authentication is required\n".to_vec(),
            timed_out: false,
            truncated: false,
        };
        assert!(sudo_authorization_failed(&interactive_auth_failure));
    }

    /// A live PTY merges stderr into stdout, so `live_sudo_failed` must
    /// find sudo's diagnostics in the merged output stream — and must not
    /// misread an ordinary non-zero exit (or diagnostics echoed by a
    /// successful command) as an authorization failure.
    #[test]
    fn live_sudo_failure_detection_reads_merged_output_and_respects_exit_code() {
        // Success: never an auth failure, even if the output happens to
        // mention a sudo-like phrase (e.g. grepping logs for it).
        assert!(!live_sudo_failed(Some(0), "sudo: a password is required"));
        // Plain command failure with unrelated output.
        assert!(!live_sudo_failed(Some(1), "application error"));
        // sudo -n refused: diagnostics arrive on the merged PTY stream.
        assert!(live_sudo_failed(Some(1), "sudo: a password is required\n"));
        assert!(live_sudo_failed(
            Some(1),
            "sudo: sorry, you must have a tty to run sudo\n"
        ));
        // Not in sudoers is also an authorization problem.
        assert!(live_sudo_failed(
            Some(1),
            "ops is not in the sudoers file.  This incident will be reported.\n"
        ));
        // Unknown exit code (marker lost) with sudo diagnostics still
        // counts as a failure — better a clear elevation error than
        // returning sudo noise as command output.
        assert!(live_sudo_failed(None, "sudo: no password was provided\n"));
        assert!(!live_sudo_failed(None, "partial output only"));
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
