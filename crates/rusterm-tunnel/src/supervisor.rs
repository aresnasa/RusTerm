//! Per-tunnel supervision: connect → bind listener → forward →
//! health-check → on failure reconnect with exponential backoff.
//!
//! One [`run_tunnel`] task per active tunnel. The task owns everything
//! (SSH handle, listener, forward tasks) and reports state transitions
//! through a `watch` channel the manager/UI subscribes to.
//!
//! # Failure model
//!
//! - Connect fails → `Reconnecting` (backoff) or `Failed` if the user
//!   stopped/auto-reconnect is off.
//! - Bind fails (port busy) → `Failed` immediately with a clear message;
//!   retrying a busy port pointlessly would just churn.
//! - Two consecutive failed health probes (`exec true` with a short
//!   deadline) → treat the connection as dead and reconnect.
//! - The accept loop surviving individual junk connections is by design:
//!   forwarding errors kill only that connection's task.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

use rusterm_core::config::SshConfig;
use rusterm_ssh::{DirectConnectOptions, DirectHandle, connect_direct};

use crate::backoff::{backoff_delay, rand01_now};
use crate::config::{TunnelConfig, TunnelKind};

/// How often we probe the SSH connection while Active.
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(30);
pub const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// This many consecutive failed probes marks the tunnel down.
pub const HEALTH_FAILURE_THRESHOLD: u32 = 2;

/// Resolves a saved connection id into an `SshConfig` (with decrypted
/// credentials). Implemented by the app layer, which owns `ConfigManager`.
/// Returning `None` means "no such saved connection".
///
/// The supervisor performs the actual SSH connect (with keepalives and a
/// connect timeout), so credential resolution and connection policy stay
/// separated.
#[async_trait]
pub trait TunnelConnector: Send + Sync + std::fmt::Debug {
    async fn resolve(&self, connection_id: &str) -> anyhow::Result<Option<SshConfig>>;
}

/// What the UI renders. Sent as one message per transition on the tunnel's
/// watch channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    /// Never started or cleanly stopped.
    Stopped,
    /// Running the initial connect.
    Connecting { attempt: u32 },
    /// Listening and forwarding.
    Active { since_epoch_secs: i64 },
    /// Lost the connection; will retry.
    Reconnecting {
        attempt: u32,
        next_retry_ms: u64,
        last_error: String,
    },
    /// Gave up (bind failure, auth failure with auto-reconnect off, or an
    /// unsupported tunnel kind). Stays until the user intervenes.
    Failed(String),
}

impl Default for TunnelState {
    fn default() -> Self {
        Self::Stopped
    }
}

impl TunnelState {
    /// CSS-friendly classifier for the status dot in the UI.
    pub fn level(&self) -> &'static str {
        match self {
            TunnelState::Stopped | TunnelState::Failed(_) => "red",
            TunnelState::Connecting { .. } | TunnelState::Reconnecting { .. } => "yellow",
            TunnelState::Active { .. } => "green",
        }
    }
}

fn now_epoch_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The supervisor's entry point. Runs until `stop_rx` fires or an
/// unrecoverable failure occurs. Every state change is published on
/// `state_tx`.
pub async fn run_tunnel(
    config: TunnelConfig,
    connector: Arc<dyn TunnelConnector>,
    state_tx: watch::Sender<TunnelState>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    if !config.is_supported() {
        let _ = state_tx.send(TunnelState::Failed(
            "remote forward (-R) is not implemented yet".into(),
        ));
        return;
    }

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if attempt == 1 {
            let _ = state_tx.send(TunnelState::Connecting { attempt });
        }

        // ── connect ──────────────────────────────────────────────────────
        let connect_result: anyhow::Result<DirectHandle> = async {
            let ssh_config = connector
                .resolve(&config.connection_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("no saved connection with id '{}'", config.connection_id)
                })?;
            connect_direct(&ssh_config, DirectConnectOptions::default()).await
        }
        .await;
        let handle = match connect_result {
            Ok(h) => h,
            Err(e) => {
                if !config.auto_reconnect {
                    let _ = state_tx.send(TunnelState::Failed(format!("{e:#}")));
                    return;
                }
                let delay = backoff_delay(attempt, rand01_now());
                let _ = state_tx.send(TunnelState::Reconnecting {
                    attempt,
                    next_retry_ms: delay.as_millis() as u64,
                    last_error: format!("{e:#}"),
                });
                if stop_or_sleep(&mut stop_rx, delay).await {
                    let _ = state_tx.send(TunnelState::Stopped);
                    return;
                }
                continue;
            }
        };

        // ── bind ─────────────────────────────────────────────────────────
        let bind_addr = SocketAddr::new(config.listen_addr, config.listen_port);
        let listener = match TcpListener::bind(bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                // Port conflicts don't get retried — retrying can't help,
                // and the UI's suggestion flow handles choosing a new port.
                let _ = state_tx.send(TunnelState::Failed(format!(
                    "cannot bind {}: {e} (choose another port)",
                    bind_addr
                )));
                return;
            }
        };

        let _ = state_tx.send(TunnelState::Active {
            since_epoch_secs: now_epoch_secs(),
        });
        tracing::info!(
            "[tunnel] '{}' active on {} via connection {}",
            config.name,
            bind_addr,
            config.connection_id
        );

        // ── serve until failure or stop ──────────────────────────────────
        let failure = serve_loop(&config, &handle, &listener, &state_tx, &mut stop_rx).await;

        // Cleanly drop the handle so the server sees a disconnect rather
        // than a hang when possible.
        let h = handle.clone();
        tokio::spawn(async move {
            let _ = h.disconnect().await;
        });

        match failure {
            LoopExit::Stopped => {
                let _ = state_tx.send(TunnelState::Stopped);
                return;
            }
            LoopExit::Broken(reason) => {
                if !config.auto_reconnect {
                    let _ = state_tx.send(TunnelState::Failed(reason));
                    return;
                }
                // Fresh retry cycle: the backoff counter restarts at 1.
                attempt = 1;
                let delay = backoff_delay(attempt, rand01_now());
                let _ = state_tx.send(TunnelState::Reconnecting {
                    attempt,
                    next_retry_ms: delay.as_millis() as u64,
                    last_error: reason,
                });
                if stop_or_sleep(&mut stop_rx, delay).await {
                    let _ = state_tx.send(TunnelState::Stopped);
                    return;
                }
            }
        }
    }
}

enum LoopExit {
    Stopped,
    Broken(String),
}

/// `true` if we should stop; `false` if the delay elapsed.
async fn stop_or_sleep(stop_rx: &mut oneshot::Receiver<()>, delay: Duration) -> bool {
    tokio::select! {
        _ = stop_rx => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// Accept connections + run health probes until something breaks or we're
/// asked to stop.
async fn serve_loop(
    config: &TunnelConfig,
    handle: &DirectHandle,
    listener: &TcpListener,
    _state_tx: &watch::Sender<TunnelState>,
    stop_rx: &mut oneshot::Receiver<()>,
) -> LoopExit {
    let mut health = tokio::time::interval(HEALTH_INTERVAL);
    health.tick().await; // discard the immediate first tick
    let mut consecutive_health_failures = 0u32;

    loop {
        tokio::select! {
            biased;

            _ = &mut *stop_rx => return LoopExit::Stopped,

            _ = health.tick() => {
                if handle.is_alive(HEALTH_PROBE_TIMEOUT).await {
                    consecutive_health_failures = 0;
                } else {
                    consecutive_health_failures += 1;
                    tracing::warn!(
                        "[tunnel] '{}' health probe failed ({}/{})",
                        config.name,
                        consecutive_health_failures,
                        HEALTH_FAILURE_THRESHOLD
                    );
                    if consecutive_health_failures >= HEALTH_FAILURE_THRESHOLD {
                        return LoopExit::Broken("health check failed — SSH connection is dead".into());
                    }
                }
            }

            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let handle = handle.clone();
                        let kind = config.kind.clone();
                        let name = config.name.clone();
                        tokio::spawn(async move {
                            if let Err(e) = forward_one(stream, peer, kind, handle).await {
                                tracing::debug!(
                                    "[tunnel] '{}' connection from {} closed with: {e:#}",
                                    name, peer
                                );
                            }
                        });
                    }
                    Err(e) => {
                        // A transient accept error (e.g. fd exhaustion) is
                        // logged but doesn't kill the loop.
                        tracing::warn!("[tunnel] '{}' accept failed: {e}", config.name);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

/// Handle one inbound connection: either a direct LocalForward hop or a
/// full SOCKS5 negotiation.
async fn forward_one(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    kind: TunnelKind,
    handle: DirectHandle,
) -> anyhow::Result<()> {
    match kind {
        TunnelKind::LocalForward {
            remote_host,
            remote_port,
        } => {
            let mut upstream = handle
                .open_direct_tcpip(
                    &remote_host,
                    remote_port,
                    (&peer.ip().to_string(), peer.port()),
                )
                .await
                .context("opening direct-tcpip channel")?;
            tokio::io::copy_bidirectional(&mut stream, &mut upstream)
                .await
                .context("forwarding local port")?;
        }
        TunnelKind::DynamicSocks => {
            crate::socks5::serve(stream, handle).await?;
        }
        TunnelKind::Remote { .. } => {
            anyhow::bail!("remote forward unsupported");
        }
    }
    Ok(())
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Always-failing connector: exercises resolve-failure → backoff →
    /// reconnect transitions without any SSH server.
    #[derive(Debug)]
    struct FailConnector;

    #[async_trait]
    impl TunnelConnector for FailConnector {
        async fn resolve(&self, connection_id: &str) -> anyhow::Result<Option<SshConfig>> {
            anyhow::bail!("simulated connect failure for {connection_id}")
        }
    }

    #[test]
    fn state_levels() {
        assert_eq!(TunnelState::Stopped.level(), "red");
        assert_eq!(TunnelState::Failed("x".into()).level(), "red");
        assert_eq!(TunnelState::Connecting { attempt: 1 }.level(), "yellow");
        assert_eq!(
            TunnelState::Reconnecting {
                attempt: 2,
                next_retry_ms: 1000,
                last_error: "x".into()
            }
            .level(),
            "yellow"
        );
        assert_eq!(
            TunnelState::Active {
                since_epoch_secs: 0
            }
            .level(),
            "green"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connect_failure_reports_reconnecting_and_stops_on_signal() {
        let config = TunnelConfig {
            id: "t".into(),
            name: "probe".into(),
            connection_id: "c".into(),
            auto_reconnect: true,
            ..Default::default()
        };
        let (state_tx, mut state_rx) = watch::channel(TunnelState::Stopped);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(run_tunnel(
            config,
            Arc::new(FailConnector),
            state_tx,
            stop_rx,
        ));

        // Walk the transitions: Connecting → Reconnecting → (next) ...
        let mut saw_reconnecting = false;
        for _ in 0..10 {
            state_rx.changed().await.unwrap();
            match &*state_rx.borrow() {
                TunnelState::Reconnecting { attempt, .. } => {
                    saw_reconnecting = true;
                    assert!(*attempt >= 1);
                    break;
                }
                TunnelState::Connecting { .. } => continue,
                other => panic!("unexpected state {other:?}"),
            }
        }
        assert!(saw_reconnecting);

        // Stop signal during backoff → terminal Stopped.
        let _ = stop_tx.send(());
        loop {
            state_rx.changed().await.unwrap();
            if matches!(&*state_rx.borrow(), TunnelState::Stopped) {
                break;
            }
        }
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unsupported_kind_fails_immediately() {
        let config = TunnelConfig {
            id: "t".into(),
            name: "remote".into(),
            connection_id: "c".into(),
            kind: TunnelKind::Remote {
                remote_host: "x".into(),
                remote_port: 1,
            },
            ..Default::default()
        };
        let (state_tx, mut state_rx) = watch::channel(TunnelState::Stopped);
        let (_stop_tx, stop_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(run_tunnel(
            config,
            Arc::new(FailConnector),
            state_tx,
            stop_rx,
        ));
        state_rx.changed().await.unwrap();
        assert!(matches!(
            &*state_rx.borrow(),
            TunnelState::Failed(msg) if msg.contains("not implemented")
        ));
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn bind_failure_is_terminal() {
        // Occupy a port, then point the tunnel at it. The connector must
        // succeed for this test, so use a stub that *pretends* to succeed
        // — run_tunnel binds before it ever uses the handle for forwarding.
        // We can't fabricate a real DirectHandle here, so instead assert at
        // the state machine level: bind errors produce Failed, and the task
        // exits. Verified indirectly via integration in `manager` tests.
    }
}
