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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SshAuth {
    Password { password: String },
    Key { private_key_path: String, passphrase: Option<String> },
    Agent,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedValue {
    pub _encrypted: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    pub version: u32,
    pub connections: Vec<PersistedConnection>,
    #[serde(default)]
    pub master_password_hash: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistedSshAuth {
    Password { password: EncryptedValue },
    Key { private_key_path: String, passphrase: Option<EncryptedValue> },
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
                auth: SshAuth::Password { password: "secret".to_string() },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: Some(30),
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
                host: "host".to_string(), port: 22, username: "user".to_string(),
                auth: SshAuth::Agent, terminal_type: "xterm-256color".to_string(),
                proxy_jump: None, keepalive_interval: None,
            }),
            ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyS0".to_string(), baud_rate: 9600, data_bits: 8,
                parity: "none".to_string(), stop_bits: 1, flow_control: "none".to_string(),
            }),
            ConnectionKind::Telnet(TelnetConfig { host: "host".to_string(), port: 23 }),
            ConnectionKind::Shell(ShellConfig {
                command: Some("/bin/bash".to_string()), args: vec![],
                env: vec![], working_dir: None,
            }),
            ConnectionKind::Tcp(TcpConfig { host: "host".to_string(), port: 8080 }),
        ];

        for kind in configs {
            let json = serde_json::to_string(&kind).unwrap();
            let de: ConnectionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, de);
        }
    }

    #[test]
    fn test_ssh_auth_variants() {
        let auths = vec![
            SshAuth::Password { password: "pass".to_string() },
            SshAuth::Key { private_key_path: "/path/to/key".to_string(), passphrase: Some("secret".to_string()) },
            SshAuth::Agent,
        ];

        for auth in auths {
            let json = serde_json::to_string(&auth).unwrap();
            let de: SshAuth = serde_json::from_str(&json).unwrap();
            assert_eq!(auth, de);
        }
    }
}
