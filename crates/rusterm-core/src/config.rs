use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: ConnectionKind,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub onekey: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionKind {
    Ssh(SshConfig),
    Serial(SerialConfig),
    Telnet(TelnetConfig),
    Shell(ShellConfig),
    Tcp(TcpConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub terminal_type: String,
    pub proxy_jump: Option<String>,
    pub keepalive_interval: Option<u64>,
    /// Host key verification policy.
    ///
    /// - `"accept-new"` (default): TOFU — first connection records the
    ///   server's host key fingerprint to `known_hosts`; subsequent
    ///   connections reject mismatched keys (MITM protection).
    /// - `"strict"`: reject any host whose key is not already in
    ///   `known_hosts`. Safest mode; requires the user to pre-populate
    ///   `known_hosts` (e.g. via `ssh-keyscan` or a previous `accept-new`
    ///   run on a trusted network).
    /// - `"disabled"`: skip verification entirely. **INSECURE** — vulnerable
    ///   to MITM. Provided only for break-glass / lab scenarios.
    #[serde(default = "default_host_key_policy")]
    pub host_key_policy: String,
}

pub fn default_host_key_policy() -> String {
    "accept-new".to_string()
}

// NOTE: `Debug` for `SshAuth` is implemented manually below to ensure passwords
// and key passphrases are never accidentally leaked through `{:?}` formatting
// (e.g. via `tracing::error!(?auth)`). This is part of RusTerm's privacy
// guarantee: secrets never appear in logs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub enum SshAuth {
    Password {
        password: String,
    },
    Key {
        private_key_path: String,
        passphrase: Option<String>,
    },
    Agent,
}

impl std::fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshAuth::Password { .. } => f
                .debug_struct("SshAuth::Password")
                .field("password", &"<redacted>")
                .finish(),
            SshAuth::Key {
                private_key_path, ..
            } => f
                .debug_struct("SshAuth::Key")
                .field("private_key_path", private_key_path)
                .field("passphrase", &"<redacted>")
                .finish(),
            SshAuth::Agent => f.write_str("SshAuth::Agent"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerialConfig {
    pub port: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: String,
    pub stop_bits: u8,
    pub flow_control: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelnetConfig {
    pub host: String,
    pub port: u16,
}

// `ShellConfig::env` can carry secrets (e.g. `AWS_SECRET_ACCESS_KEY=...`),
// so its `Debug` impl redacts all env *values* while preserving keys for
// diagnosability (knowing which env vars are set is operationally useful;
// knowing their values is not, and is a classic leak vector).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<String>,
}

impl std::fmt::Debug for ShellConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_env: Vec<(String, &str)> = self
            .env
            .iter()
            .map(|(k, _)| (k.clone(), "<redacted>"))
            .collect();
        f.debug_struct("ShellConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &redacted_env)
            .field("working_dir", &self.working_dir)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TcpConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub connections: Vec<ConnectionConfig>,
}

// --- Persistence types (encrypted JSON on disk) ---

// `EncryptedValue` stores AEAD ciphertext (nonce + AES-256-GCM output) as
// base64. The ciphertext itself is not secret, but we redact it in `Debug`
// to keep logs compact and avoid creating the impression that any
// cryptographic material is being written to disk in the clear.
#[derive(Clone, Serialize, Deserialize)]
pub struct EncryptedValue {
    pub _encrypted: String,
}

impl std::fmt::Debug for EncryptedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedValue")
            .field("_encrypted", &"<redacted>")
            .finish()
    }
}

/// Visual treatment for the top tab whose session owns the focused pane.
///
/// The full outline is rendered as an inset shadow so changing its width does
/// not resize tabs or make the tab row jump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusedTabAppearance {
    #[serde(default = "default_focused_tab_border_color")]
    pub border_color: String,
    #[serde(default = "default_focused_tab_border_width")]
    pub border_width: u8,
    #[serde(default = "default_focused_tab_border_radius")]
    pub border_radius: u8,
}

impl Default for FocusedTabAppearance {
    fn default() -> Self {
        Self {
            border_color: default_focused_tab_border_color(),
            border_width: default_focused_tab_border_width(),
            border_radius: default_focused_tab_border_radius(),
        }
    }
}

impl FocusedTabAppearance {
    /// Keep values safe for direct CSS interpolation, including settings that
    /// were edited manually outside the application.
    pub fn normalized(mut self) -> Self {
        let color_is_hex = self.border_color.len() == 7
            && self.border_color.starts_with('#')
            && self.border_color[1..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit());
        if !color_is_hex {
            self.border_color = default_focused_tab_border_color();
        }
        self.border_width = self.border_width.clamp(1, 4);
        self.border_radius = self.border_radius.min(12);
        self
    }
}

fn default_focused_tab_border_color() -> String {
    "#c0caf5".to_string()
}

fn default_focused_tab_border_width() -> u8 {
    1
}

fn default_focused_tab_border_radius() -> u8 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub version: u32,
    pub connections: Vec<PersistedConnection>,
    #[serde(default)]
    pub onekeys: Vec<PersistedOneKey>,
    #[serde(default)]
    pub master_password_hash: Option<String>,
    /// Whether the user picked "不再询问" on the session-state restore dialog.
    /// When true, we don't save session state and don't prompt on next launch.
    /// The user can re-enable via settings (future work: a settings toggle).
    /// Default false for backward compat — existing users get the prompt.
    #[serde(default)]
    pub restore_disabled: bool,
    /// Appearance of the complete outline around the focused pane's top tab.
    #[serde(default)]
    pub focused_tab_appearance: FocusedTabAppearance,
}

// --- OneKeys (ZOC-style Expect/Send auto-fill) ---

/// In-memory OneKey entry: a named sequence of Expect/Send steps.
/// When terminal output matches a step's `expect`, that step's `send` is offered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OneKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<OneKeyStep>,
}

// `OneKeyStep` holds the `send` value in memory as plaintext (so it can be sent
// to the terminal when the step matches). Its `Debug` impl redacts `send` so
// accidental `tracing::debug!(?step)` calls don't leak credentials into logs.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct OneKeyStep {
    /// Display label, e.g. "Username" / "Password".
    #[serde(default)]
    pub label: String,
    /// Regex matched against terminal output.
    pub expect: String,
    /// Value to send when this step matches (plaintext in memory).
    pub send: String,
}

impl std::fmt::Debug for OneKeyStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneKeyStep")
            .field("label", &self.label)
            .field("expect", &self.expect)
            .field("send", &"<redacted>")
            .finish()
    }
}

/// Persisted OneKey entry. Each step's `send` is encrypted at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOneKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub steps: Vec<PersistedOneKeyStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedOneKeyStep {
    #[serde(default)]
    pub label: String,
    pub expect: String,
    pub send: EncryptedValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConnection {
    pub id: String,
    pub name: String,
    pub kind: PersistedConnectionKind,
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub onekey: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedConnectionKind {
    Ssh(PersistedSshConfig),
    Serial(SerialConfig),
    Telnet(TelnetConfig),
    Shell(ShellConfig),
    Tcp(TcpConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: PersistedSshAuth,
    pub terminal_type: String,
    pub proxy_jump: Option<String>,
    pub keepalive_interval: Option<u64>,
    #[serde(default = "default_host_key_policy")]
    pub host_key_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedSshAuth {
    Password {
        password: EncryptedValue,
    },
    Key {
        private_key_path: String,
        passphrase: Option<EncryptedValue>,
    },
    Agent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = ConnectionConfig {
            id: "test-1".to_string(),
            name: "Test Server".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "192.168.1.1".to_string(),
                port: 22,
                username: "root".to_string(),
                auth: SshAuth::Password {
                    password: "secret".to_string(),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: Some(30),
                host_key_policy: default_host_key_policy(),
            }),
            group: Some("Production".to_string()),
            tags: vec!["linux".to_string(), "prod".to_string()],
            onekey: true,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_toml_roundtrip() {
        let config = ConnectionConfig {
            id: "test-2".to_string(),
            name: "Serial Device".to_string(),
            kind: ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 115200,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
        };

        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: ConnectionConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_all_connection_kinds() {
        let configs = vec![
            ConnectionKind::Ssh(SshConfig {
                host: "host".to_string(),
                port: 22,
                username: "user".to_string(),
                auth: SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: default_host_key_policy(),
            }),
            ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyS0".to_string(),
                baud_rate: 9600,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            ConnectionKind::Telnet(TelnetConfig {
                host: "host".to_string(),
                port: 23,
            }),
            ConnectionKind::Shell(ShellConfig {
                command: Some("/bin/bash".to_string()),
                args: vec![],
                env: vec![],
                working_dir: None,
            }),
            ConnectionKind::Tcp(TcpConfig {
                host: "host".to_string(),
                port: 8080,
            }),
        ];

        for kind in configs {
            let json = serde_json::to_string(&kind).unwrap();
            let de: ConnectionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, de);
        }
    }

    #[test]
    fn focused_tab_appearance_defaults_for_legacy_settings() {
        let config: PersistedConfig =
            serde_json::from_str(r#"{"version":1,"connections":[]}"#).unwrap();

        assert_eq!(
            config.focused_tab_appearance,
            FocusedTabAppearance::default()
        );
    }

    #[test]
    fn focused_tab_appearance_normalizes_untrusted_values() {
        let appearance = FocusedTabAppearance {
            border_color: "red; display: none".to_string(),
            border_width: 99,
            border_radius: 99,
        }
        .normalized();

        assert_eq!(appearance.border_color, "#c0caf5");
        assert_eq!(appearance.border_width, 4);
        assert_eq!(appearance.border_radius, 12);
    }

    #[test]
    fn test_ssh_auth_variants() {
        let auths = vec![
            SshAuth::Password {
                password: "pass".to_string(),
            },
            SshAuth::Key {
                private_key_path: "/path/to/key".to_string(),
                passphrase: Some("secret".to_string()),
            },
            SshAuth::Agent,
        ];

        for auth in auths {
            let json = serde_json::to_string(&auth).unwrap();
            let de: SshAuth = serde_json::from_str(&json).unwrap();
            assert_eq!(auth, de);
        }
    }
}
