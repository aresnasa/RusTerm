//! `rusterm-tunnel` — SSH tunnel supervisor.
//!
//! Implements `ssh -L` (local port → fixed remote destination) and
//! `ssh -D` (dynamic SOCKS5) as *app-level* tunnels running over the saved
//! SSH connections, with the priorities the UI asked for: keepalive +
//! health-check driven auto-reconnect (exponential backoff, jittered), and
//! listen-port conflict detection with concrete port suggestions.
//!
//! Wiring summary:
//!
//! - [`config::TunnelConfig`] — one tunnel; persisted in `tunnels.json`
//!   (connection referenced by id, never credentials).
//! - [`supervisor::run_tunnel`] — one task per running tunnel; publishes
//!   [`TunnelState`] transitions on a `watch` channel.
//! - [`manager::TunnelManager`] — owns all supervisors + persistence and is
//!   what the UI calls.
//! - [`supervisor::TunnelConnector`] — the app implements this to resolve
//!   `connection_id` → `SshConfig` (secrets stay in the app layer).
//!
//! `ssh -R` (remote forward) is intentionally a TODO; the enum value is
//! persisted so the UI can grey it out, and starting one fails cleanly.

pub mod backoff;
pub mod config;
pub mod manager;
pub mod ports;
pub mod socks5;
pub mod supervisor;

pub use backoff::{backoff_delay, rand01_now};
pub use config::{TunnelConfig, TunnelKind, TunnelsDocument};
pub use manager::{ManagerError, TunnelManager, TunnelSnapshot};
pub use ports::{check_port_available, suggest_listen_ports};
pub use supervisor::{TunnelConnector, TunnelState, run_tunnel};
