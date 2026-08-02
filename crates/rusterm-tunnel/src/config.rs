//! Tunnel definitions, persisted as `tunnels.json` in the app config dir
//! (standalone file — same rationale as `relay.json`: `PersistedConfig` is
//! expensive to extend).
//!
//! A tunnel references a **saved SSH connection by id**. It never carries
//! credentials itself, keeping this file safe to read/copy.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use rusterm_core::paths::resolve_config_file_path;

pub const TUNNELS_CONFIG_FILE: &str = "tunnels.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelKind {
    /// `ssh -L listen_port:remote_host:remote_port` — forward one local
    /// port to a fixed remote destination, through the SSH server.
    LocalForward {
        remote_host: String,
        remote_port: u16,
    },
    /// `ssh -D listen_port` — a SOCKS5 proxy on the local side; each
    /// request is forwarded to its own destination through the SSH server.
    DynamicSocks,
    /// `ssh -R` — not implemented yet.
    Remote {
        remote_host: String,
        remote_port: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelConfig {
    /// Stable id (uuid) — referenced by the UI and status events.
    pub id: String,
    pub name: String,
    /// Id of a saved SSH connection in the app's config. Resolution to an
    /// `SshConfig` (including secret decryption) is the app layer's job,
    /// via [`crate::TunnelConnector`].
    pub connection_id: String,
    pub listen_addr: IpAddr,
    pub listen_port: u16,
    pub kind: TunnelKind,
    /// Start when the app starts.
    pub auto_start: bool,
    /// Reconnect with exponential backoff when the SSH connection drops.
    pub auto_reconnect: bool,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            connection_id: String::new(),
            listen_addr: std::net::Ipv4Addr::LOCALHOST.into(),
            listen_port: 0,
            kind: TunnelKind::DynamicSocks,
            auto_start: false,
            auto_reconnect: true,
        }
    }
}

impl TunnelConfig {
    pub fn new(id: String, name: String, connection_id: String, kind: TunnelKind) -> Self {
        Self {
            id,
            name,
            connection_id,
            kind,
            ..Default::default()
        }
    }

    /// Whether a Remote-forward tunnel can actually run in this version.
    pub fn is_supported(&self) -> bool {
        !matches!(self.kind, TunnelKind::Remote { .. })
    }
}

/// The on-disk document: version wrapper around a list of tunnels.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TunnelsDocument {
    pub version: u32,
    pub tunnels: Vec<TunnelConfig>,
}

impl TunnelsDocument {
    pub fn load() -> anyhow::Result<Self> {
        let path = resolve_config_file_path(TUNNELS_CONFIG_FILE)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                version: 1,
                ..Default::default()
            }),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = resolve_config_file_path(TUNNELS_CONFIG_FILE)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_json() {
        let doc = TunnelsDocument {
            version: 1,
            tunnels: vec![
                TunnelConfig {
                    id: "t1".into(),
                    name: "db-forward".into(),
                    connection_id: "conn-9".into(),
                    listen_port: 15432,
                    kind: TunnelKind::LocalForward {
                        remote_host: "127.0.0.1".into(),
                        remote_port: 5432,
                    },
                    auto_start: true,
                    ..Default::default()
                },
                TunnelConfig {
                    id: "t2".into(),
                    name: "socks".into(),
                    connection_id: "conn-9".into(),
                    listen_port: 1080,
                    kind: TunnelKind::DynamicSocks,
                    ..Default::default()
                },
            ],
        };
        let json = serde_json::to_string_pretty(&doc).unwrap();
        let back: TunnelsDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tunnels.len(), 2);
        assert_eq!(back.tunnels[0].listen_port, 15432);
        assert_eq!(
            back.tunnels[0].kind,
            TunnelKind::LocalForward {
                remote_host: "127.0.0.1".into(),
                remote_port: 5432
            }
        );
        assert!(back.tunnels[1].is_supported());
    }

    #[test]
    fn remote_forward_marks_unsupported() {
        let cfg = TunnelConfig {
            kind: TunnelKind::Remote {
                remote_host: "x".into(),
                remote_port: 1,
            },
            ..Default::default()
        };
        assert!(!cfg.is_supported());
    }
}
