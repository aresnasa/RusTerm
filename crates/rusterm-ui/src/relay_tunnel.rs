//! Bridges between the REST relay / SSH tunnel subsystems and the app.
//!
//! Both subsystems are deliberately UI-agnostic (see their crates): the
//! relay needs a way to enumerate saved hosts and run commands on them, the
//! tunnel manager needs a way to resolve `connection_id` → `SshConfig`.
//! This module implements those two traits over the live [`AppState`]
//! signal, and owns the process-level runtimes (relay handle, tunnel
//! manager) the UI talks to.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;

use rusterm_core::config::{ConnectionConfig, ConnectionKind, SshConfig};
use rusterm_relay::{
    ExecOutcome, ExecutorError, HostInfo, RelayExecutor, RelayHandle, run as run_relay,
};
use rusterm_ssh::{DirectConnectOptions, connect_direct};
use rusterm_tunnel::{TunnelConnector, TunnelManager};

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
        timeout: Duration,
    ) -> Result<ExecOutcome, ExecutorError> {
        let ssh = read_connections()
            .into_iter()
            .find_map(|conn| {
                if conn.id == host_id || conn.name == host_id {
                    match conn.kind {
                        ConnectionKind::Ssh(ssh) => Some(ssh),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .ok_or_else(|| ExecutorError::UnknownHost(host_id.to_string()))?;

        let started = std::time::Instant::now();
        let handle = connect_direct(&ssh, DirectConnectOptions::default())
            .await
            .map_err(|e| ExecutorError::Connect(format!("{e:#}")))?;

        let exec_future = handle.exec(command, timeout);
        let result = match tokio::time::timeout(timeout, exec_future).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let _ = handle.disconnect().await;
                return Err(ExecutorError::Exec(format!("{e:#}")));
            }
            Err(_) => {
                let _ = handle.disconnect().await;
                return Ok(ExecOutcome {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: format!("local timeout after {timeout:?}"),
                    timed_out: true,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
            }
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
