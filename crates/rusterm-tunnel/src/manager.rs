//! [`TunnelManager`] — the app-facing entry point. Owns the per-tunnel
//! supervisor tasks, persists `tunnels.json`, and exposes snapshots the UI
//! renders.
//!
//! Two independent channels matter here:
//!
//! - a `tokio::sync::watch` channel **per tunnel** carrying the latest
//!   [`TunnelState`] (cheap for the UI to poll);
//! - a `broadcast` channel for persistence-triggered changes (add/remove)
//!   so a settings screen can refresh even while no state transition fires.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::{oneshot, watch};

use crate::config::{TunnelConfig, TunnelsDocument};
use crate::ports::{check_port_available, suggest_listen_ports};
use crate::supervisor::{TunnelConnector, TunnelState, run_tunnel};

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("no tunnel with id {0}")]
    NotFound(String),
    #[error("tunnel {0} is already running")]
    AlreadyRunning(String),
    #[error("IO: {0}")]
    Io(String),
}

/// What the UI draws in one row of the tunnel list.
#[derive(Debug, Clone)]
pub struct TunnelSnapshot {
    pub config: TunnelConfig,
    pub state: TunnelState,
}

struct ManagedTunnel {
    config: TunnelConfig,
    state_rx: watch::Receiver<TunnelState>,
    state_tx: watch::Sender<TunnelState>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for ManagedTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedTunnel")
            .field("id", &self.config.id)
            .field("state", &*self.state_rx.borrow())
            .finish_non_exhaustive()
    }
}

impl ManagedTunnel {
    fn new(config: TunnelConfig) -> Self {
        let (state_tx, state_rx) = watch::channel(TunnelState::Stopped);
        Self {
            config,
            state_rx,
            state_tx,
            stop_tx: None,
            task: None,
        }
    }

    fn is_running(&self) -> bool {
        self.stop_tx.is_some()
    }

    fn snapshot(&self) -> TunnelSnapshot {
        TunnelSnapshot {
            config: self.config.clone(),
            state: self.state_rx.borrow().clone(),
        }
    }
}

/// Thread-safe facade. Each method takes `&self`; coordination is through
/// an interior `RwLock`.
pub struct TunnelManager {
    tunnels: RwLock<HashMap<String, ManagedTunnel>>,
    connector: Arc<dyn TunnelConnector>,
    /// The Tokio runtime supervisor tasks are spawned onto. Captured at
    /// construction so `start()` can be called from any thread (including
    /// UI event handlers with no enter-guard).
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for TunnelManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelManager")
            .field("count", &self.tunnels.read().len())
            .finish_non_exhaustive()
    }
}

impl TunnelManager {
    /// Construct using the *current* Tokio runtime. Panics if there isn't
    /// one — prefer [`with_runtime`] for explicit control.
    pub fn new(connector: Arc<dyn TunnelConnector>) -> Arc<Self> {
        Self::with_runtime(connector, tokio::runtime::Handle::current())
    }

    pub fn with_runtime(
        connector: Arc<dyn TunnelConnector>,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            tunnels: RwLock::new(HashMap::new()),
            connector,
            runtime,
        })
    }

    /// Load persisted tunnel definitions. Supervisor tasks are NOT started;
    /// call [`autostart`] after the app is ready.
    pub fn load_from_disk(&self) -> anyhow::Result<usize> {
        let doc = TunnelsDocument::load()?;
        let count = doc.tunnels.len();
        let mut guard = self.tunnels.write();
        for config in doc.tunnels {
            guard
                .entry(config.id.clone())
                .or_insert_with(|| ManagedTunnel::new(config));
        }
        Ok(count)
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        let doc = TunnelsDocument {
            version: 1,
            tunnels: self
                .tunnels
                .read()
                .values()
                .map(|t| t.config.clone())
                .collect(),
        };
        doc.save()
    }

    // ── CRUD ────────────────────────────────────────────────────────────

    pub fn upsert(&self, config: TunnelConfig) {
        self.persist_registry_change(|guard| {
            match guard.get_mut(&config.id) {
                Some(existing) if !existing.is_running() => {
                    existing.config = config;
                }
                Some(existing) => {
                    // Running tunnel: only metadata edits are allowed
                    // through; runtime fields (ports, kind, connection)
                    // require stop → edit → start, enforced by the UI.
                    existing.config.name = config.name;
                    existing.config.auto_start = config.auto_start;
                    existing.config.auto_reconnect = config.auto_reconnect;
                }
                None => {
                    guard.insert(config.id.clone(), ManagedTunnel::new(config));
                }
            }
        });
    }

    pub fn remove(&self, id: &str) -> Result<(), ManagerError> {
        self.stop(id)?;
        self.persist_registry_change(|guard| {
            guard.remove(id);
        });
        Ok(())
    }

    fn persist_registry_change(&self, change: impl FnOnce(&mut HashMap<String, ManagedTunnel>)) {
        {
            let mut guard = self.tunnels.write();
            change(&mut guard);
        }
        if let Err(e) = self.persist() {
            tracing::warn!("[tunnel] failed to persist tunnels.json: {e:#}");
        }
    }

    // ── lifecycle ───────────────────────────────────────────────────────

    pub fn list(&self) -> Vec<TunnelSnapshot> {
        self.tunnels
            .read()
            .values()
            .map(ManagedTunnel::snapshot)
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<TunnelSnapshot> {
        self.tunnels.read().get(id).map(ManagedTunnel::snapshot)
    }

    /// Subscribe to future states of one tunnel (for a live UI row).
    pub fn subscribe(&self, id: &str) -> Option<watch::Receiver<TunnelState>> {
        self.tunnels.read().get(id).map(|t| t.state_rx.clone())
    }

    pub fn start(&self, id: &str) -> Result<(), ManagerError> {
        let mut guard = self.tunnels.write();
        let tunnel = guard
            .get_mut(id)
            .ok_or_else(|| ManagerError::NotFound(id.into()))?;
        if tunnel.is_running() {
            return Err(ManagerError::AlreadyRunning(id.into()));
        }
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = self.runtime.spawn(run_tunnel(
            tunnel.config.clone(),
            self.connector.clone(),
            tunnel.state_tx.clone(),
            stop_rx,
        ));
        tunnel.stop_tx = Some(stop_tx);
        tunnel.task = Some(task);
        Ok(())
    }

    pub fn stop(&self, id: &str) -> Result<(), ManagerError> {
        let mut guard = self.tunnels.write();
        let tunnel = guard
            .get_mut(id)
            .ok_or_else(|| ManagerError::NotFound(id.into()))?;
        if let Some(tx) = tunnel.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = tunnel.task.take() {
            self.runtime.spawn(async move {
                let _ = task.await;
            });
        }
        Ok(())
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.tunnels.read().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    pub fn autostart(&self) {
        let ids: Vec<String> = self
            .tunnels
            .read()
            .values()
            .filter(|t| t.config.auto_start)
            .map(|t| t.config.id.clone())
            .collect();
        for id in ids {
            if let Err(e) = self.start(&id) {
                tracing::warn!("[tunnel] autostart of {id} failed: {e}");
            }
        }
    }

    // ── ports ───────────────────────────────────────────────────────────

    /// Probe whether the configured port of one tunnel is free.
    pub fn check_listen_port(&self, id: &str) -> Option<bool> {
        let guard = self.tunnels.read();
        guard.get(id).map(|t| {
            let cfg = &t.config;
            check_port_available(cfg.listen_addr, cfg.listen_port)
        })
    }

    /// Suggest up to `count` alternative ports near `desired`.
    pub fn suggest_ports(&self, desired: u16, count: usize) -> Vec<u16> {
        suggest_listen_ports(std::net::Ipv4Addr::LOCALHOST.into(), desired, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rusterm_core::config::SshConfig;

    #[derive(Debug)]
    struct StubConnector;

    #[async_trait]
    impl TunnelConnector for StubConnector {
        async fn resolve(&self, _id: &str) -> anyhow::Result<Option<SshConfig>> {
            anyhow::bail!("no ssh server in tests")
        }
    }

    fn manager() -> Arc<TunnelManager> {
        // Leaked on purpose: sync `#[test]` fns have no ambient runtime and
        // this stub manager outlives the builder. Harmless in tests.
        let runtime = Box::leak(Box::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        ));
        TunnelManager::with_runtime(Arc::new(StubConnector), runtime.handle().clone())
    }

    fn sample(id: &str) -> TunnelConfig {
        TunnelConfig {
            id: id.into(),
            name: format!("tunnel-{id}"),
            connection_id: "conn".into(),
            ..Default::default()
        }
    }

    #[test]
    fn add_list_get_remove() {
        let m = manager();
        m.tunnels
            .write()
            .insert("a".into(), ManagedTunnel::new(sample("a")));
        assert_eq!(m.list().len(), 1);
        assert_eq!(m.get("a").unwrap().config.name, "tunnel-a");
        assert!(m.get("missing").is_none());
        let _ = m.remove("a");
        assert_eq!(m.list().len(), 0);
    }

    #[tokio::test]
    async fn start_twice_errors() {
        let m = manager();
        let id = "t1";
        m.tunnels
            .write()
            .insert(id.into(), ManagedTunnel::new(sample(id)));
        m.start(id).unwrap();
        let err = m.start(id).unwrap_err();
        assert!(matches!(err, ManagerError::AlreadyRunning(_)));
        m.stop_all().await;
    }

    #[tokio::test]
    async fn stop_unknown_errors() {
        let m = manager();
        let err = m.stop("ghost").unwrap_err();
        assert!(matches!(err, ManagerError::NotFound(_)));
    }

    #[test]
    fn suggest_ports_skips_busy() {
        let m = manager();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let busy = listener.local_addr().unwrap().port();
        let suggestions = m.suggest_ports(busy, 3);
        assert!(!suggestions.contains(&busy));
    }

    #[test]
    fn edits_to_running_tunnel_only_touch_metadata() {
        let m = manager();
        let id = "t2";
        {
            m.tunnels
                .write()
                .insert(id.into(), ManagedTunnel::new(sample(id)));
        }
        // Fake "running" state by injecting a stop channel.
        {
            let mut guard = m.tunnels.write();
            let (tx, _rx) = oneshot::channel::<()>();
            guard.get_mut(id).unwrap().stop_tx = Some(tx);
        }
        let mut updated = sample(id);
        updated.name = "renamed".into();
        updated.listen_port = 9999; // should be ignored while running
        m.upsert(updated);
        let snap = m.get(id).unwrap();
        assert_eq!(snap.config.name, "renamed");
        assert_eq!(snap.config.listen_port, 0);
    }
}
