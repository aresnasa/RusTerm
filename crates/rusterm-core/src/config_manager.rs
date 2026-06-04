use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use argon2::password_hash::{PasswordHasher, SaltString};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rand::RngCore;

use crate::config::{
    ConnectionConfig, ConnectionKind, EncryptedValue, PersistedConfig, PersistedConnection,
    PersistedConnectionKind, PersistedSshAuth, PersistedSshConfig, SshAuth, SshConfig,
};
use rusterm_crypto::{decrypt_data, encrypt_data, KeyringStore};

const CONFIG_FILE_NAME: &str = "settings.json";
const CONFIG_VERSION: u32 = 1;
const KEY_DERIVATION_SALT: &[u8] = b"rusterm-master-key-salt-v1";

#[derive(Clone)]
pub struct ConfigManager {
    config_path: PathBuf,
    master_key: [u8; 32],
    master_password_hash: Option<String>,
}

impl std::fmt::Debug for ConfigManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigManager")
            .field("config_path", &self.config_path)
            .field("master_key", &"[redacted]")
            .finish()
    }
}

impl ConfigManager {
    /// Create ConfigManager with a user-provided master password.
    /// On first run (no settings.json), creates a new hash.
    /// On subsequent runs, verifies the password against the stored hash.
    pub fn with_master_password(password: &str) -> Result<Self> {
        Self::migrate_legacy_config();
        let config_path = Self::resolve_config_path()?;

        let stored_hash = Self::read_master_password_hash(&config_path)?;

        let master_key = rusterm_crypto::derive_key(password, KEY_DERIVATION_SALT)?;

        if let Some(hash) = &stored_hash {
            if !rusterm_crypto::verify_password(password, hash)? {
                anyhow::bail!("Invalid master password");
            }
        }

        let master_password_hash = if stored_hash.is_some() {
            stored_hash
        } else {
            Some(Self::hash_password(password)?)
        };

        Ok(Self {
            config_path,
            master_key,
            master_password_hash,
        })
    }

    /// Check if settings.json exists (no key needed).
    pub fn check_config_exists() -> bool {
        Self::resolve_config_path()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// Legacy constructor using OS keyring / machine ID (for backward compat).
    pub fn new() -> Result<Self> {
        Self::migrate_legacy_config();
        let config_path = Self::resolve_config_path()?;
        let master_key = Self::resolve_master_key()?;
        Ok(Self {
            config_path,
            master_key,
            master_password_hash: None,
        })
    }

    fn resolve_config_path() -> Result<PathBuf> {
        // 1. Override via environment variable
        if let Ok(dir) = std::env::var("RUSTERM_CONFIG_DIR") {
            let path = PathBuf::from(dir);
            fs::create_dir_all(&path)
                .context("Failed to create config dir from RUSTERM_CONFIG_DIR")?;
            return Ok(path.join(CONFIG_FILE_NAME));
        }

        // 2. Next to the binary (primary location)
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return Ok(parent.join(CONFIG_FILE_NAME));
            }
        }

        // 3. Fallback: platform config dir
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm");
        fs::create_dir_all(&config_dir).context("Failed to create config dir")?;
        Ok(config_dir.join(CONFIG_FILE_NAME))
    }

    fn resolve_master_key() -> Result<[u8; 32]> {
        match KeyringStore::get_credential("rusterm-master-key") {
            Ok(encoded) => {
                let bytes = BASE64
                    .decode(&encoded)
                    .context("Failed to decode master key")?;
                if bytes.len() != 32 {
                    anyhow::bail!("Master key has wrong length");
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            Err(_) => {
                let mut key = [0u8; 32];
                rand::rng().fill_bytes(&mut key);
                if let Err(e) =
                    KeyringStore::save_credential("rusterm-master-key", &BASE64.encode(key))
                {
                    tracing::warn!("OS keyring unavailable, deriving master key from machine ID: {e}");
                    let machine_id = Self::get_machine_id();
                    key = rusterm_crypto::derive_key(&machine_id, KEY_DERIVATION_SALT)?;
                }
                Ok(key)
            }
        }
    }

    fn read_master_password_hash(config_path: &PathBuf) -> Result<Option<String>> {
        if !config_path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(config_path)
            .context("Failed to read settings.json")?;
        let persisted: serde_json::Value = serde_json::from_str(&content)
            .context("Failed to parse settings.json")?;
        Ok(persisted.get("master_password_hash")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }

    fn hash_password(password: &str) -> Result<String> {
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
        let salt = SaltString::encode_b64(KEY_DERIVATION_SALT)
            .map_err(|e| anyhow::anyhow!("Salt error: {}", e))?;
        argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("Hash error: {}", e))
            .map(|h| h.to_string())
    }

    fn migrate_legacy_config() {
        let legacy_name = "connections.json";
        let new_name = CONFIG_FILE_NAME;

        // Check next to binary
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let legacy = parent.join(legacy_name);
                let new_path = parent.join(new_name);
                if legacy.exists() && !new_path.exists() {
                    if let Err(e) = fs::rename(&legacy, &new_path) {
                        tracing::warn!("Failed to migrate {legacy_name}: {e}");
                    } else {
                        tracing::info!("Migrated {legacy_name} to {new_name}");
                    }
                }
            }
        }

        // Check platform config dir
        if let Some(config_dir) = dirs::config_dir() {
            let dir = config_dir.join("rusterm");
            let legacy = dir.join(legacy_name);
            let new_path = dir.join(new_name);
            if legacy.exists() && !new_path.exists() {
                if let Err(e) = fs::rename(&legacy, &new_path) {
                    tracing::warn!("Failed to migrate {legacy_name} in config dir: {e}");
                } else {
                    tracing::info!("Migrated {legacy_name} to {new_name} in config dir");
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn get_machine_id() -> String {
        std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| {
                s.lines()
                    .find(|l| l.contains("IOPlatformUUID"))
                    .map(|l| l.to_string())
            })
            .unwrap_or_else(|| "fallback-machine-id".to_string())
    }

    #[cfg(target_os = "linux")]
    fn get_machine_id() -> String {
        fs::read_to_string("/etc/machine-id").unwrap_or_else(|_| "fallback-machine-id".to_string())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn get_machine_id() -> String {
        "fallback-machine-id".to_string()
    }

    pub fn load_connections(&self) -> Result<Vec<ConnectionConfig>> {
        if !self.config_path.exists() {
            return Ok(Vec::new());
        }

        let content =
            fs::read_to_string(&self.config_path).context("Failed to read config file")?;

        let persisted: PersistedConfig =
            serde_json::from_str(&content).context("Failed to parse config file")?;

        persisted
            .connections
            .into_iter()
            .map(|pc| self.decrypt_connection(pc))
            .collect()
    }

    pub fn save_connections(&self, connections: &[ConnectionConfig]) -> Result<()> {
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: connections
                .iter()
                .map(|c| self.encrypt_connection(c))
                .collect::<Result<Vec<_>>>()?,
            master_password_hash: self.master_password_hash.clone(),
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    fn encrypt_connection(&self, conn: &ConnectionConfig) -> Result<PersistedConnection> {
        Ok(PersistedConnection {
            id: conn.id.clone(),
            name: conn.name.clone(),
            kind: self.encrypt_kind(&conn.kind)?,
            group: conn.group.clone(),
            tags: conn.tags.clone(),
            onekey: conn.onekey,
        })
    }

    fn encrypt_kind(&self, kind: &ConnectionKind) -> Result<PersistedConnectionKind> {
        Ok(match kind {
            ConnectionKind::Ssh(ssh) => PersistedConnectionKind::Ssh(PersistedSshConfig {
                host: ssh.host.clone(),
                port: ssh.port,
                username: ssh.username.clone(),
                auth: self.encrypt_auth(&ssh.auth)?,
                terminal_type: ssh.terminal_type.clone(),
                proxy_jump: ssh.proxy_jump.clone(),
                keepalive_interval: ssh.keepalive_interval,
            }),
            ConnectionKind::Serial(s) => PersistedConnectionKind::Serial(s.clone()),
            ConnectionKind::Telnet(t) => PersistedConnectionKind::Telnet(t.clone()),
            ConnectionKind::Shell(s) => PersistedConnectionKind::Shell(s.clone()),
            ConnectionKind::Tcp(t) => PersistedConnectionKind::Tcp(t.clone()),
        })
    }

    fn encrypt_auth(&self, auth: &SshAuth) -> Result<PersistedSshAuth> {
        Ok(match auth {
            SshAuth::Password { password } => PersistedSshAuth::Password {
                password: self.encrypt_string(password)?,
            },
            SshAuth::Key {
                private_key_path,
                passphrase,
            } => PersistedSshAuth::Key {
                private_key_path: private_key_path.clone(),
                passphrase: passphrase
                    .as_ref()
                    .map(|p| self.encrypt_string(p))
                    .transpose()?,
            },
            SshAuth::Agent => PersistedSshAuth::Agent,
        })
    }

    fn encrypt_string(&self, plaintext: &str) -> Result<EncryptedValue> {
        let ciphertext = encrypt_data(&self.master_key, plaintext.as_bytes())?;
        Ok(EncryptedValue {
            _encrypted: BASE64.encode(ciphertext),
        })
    }

    fn decrypt_connection(&self, pc: PersistedConnection) -> Result<ConnectionConfig> {
        Ok(ConnectionConfig {
            id: pc.id,
            name: pc.name,
            kind: self.decrypt_kind(pc.kind)?,
            group: pc.group,
            tags: pc.tags,
            onekey: pc.onekey,
        })
    }

    fn decrypt_kind(&self, kind: PersistedConnectionKind) -> Result<ConnectionKind> {
        Ok(match kind {
            PersistedConnectionKind::Ssh(ssh) => ConnectionKind::Ssh(SshConfig {
                host: ssh.host,
                port: ssh.port,
                username: ssh.username,
                auth: self.decrypt_auth(ssh.auth)?,
                terminal_type: ssh.terminal_type,
                proxy_jump: ssh.proxy_jump,
                keepalive_interval: ssh.keepalive_interval,
            }),
            PersistedConnectionKind::Serial(s) => ConnectionKind::Serial(s),
            PersistedConnectionKind::Telnet(t) => ConnectionKind::Telnet(t),
            PersistedConnectionKind::Shell(s) => ConnectionKind::Shell(s),
            PersistedConnectionKind::Tcp(t) => ConnectionKind::Tcp(t),
        })
    }

    fn decrypt_auth(&self, auth: PersistedSshAuth) -> Result<SshAuth> {
        Ok(match auth {
            PersistedSshAuth::Password { password } => SshAuth::Password {
                password: self.decrypt_value(&password)?,
            },
            PersistedSshAuth::Key {
                private_key_path,
                passphrase,
            } => SshAuth::Key {
                private_key_path,
                passphrase: passphrase.map(|p| self.decrypt_value(&p)).transpose()?,
            },
            PersistedSshAuth::Agent => SshAuth::Agent,
        })
    }

    fn decrypt_value(&self, ev: &EncryptedValue) -> Result<String> {
        let ciphertext = BASE64
            .decode(&ev._encrypted)
            .context("Failed to decode encrypted value")?;
        let plaintext = decrypt_data(&self.master_key, &ciphertext)?;
        String::from_utf8(plaintext.to_vec()).context("Decrypted value is not valid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ConnectionKind, SerialConfig, SshAuth, SshConfig, TcpConfig, TelnetConfig,
    };

    fn test_config_manager() -> (ConfigManager, tempfile::TempDir) {
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test_settings.json");
        let cm = ConfigManager {
            config_path,
            master_key: key,
            master_password_hash: None,
        };
        (cm, dir)
    }

    #[test]
    fn test_save_and_load_empty() {
        let (cm, _dir) = test_config_manager();
        cm.save_connections(&[]).unwrap();
        let loaded = cm.load_connections().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_save_and_load_ssh_password() {
        let (cm, _dir) = test_config_manager();
        let conn = ConnectionConfig {
            id: "test-1".to_string(),
            name: "Test Server".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "192.168.1.1".to_string(),
                port: 22,
                username: "root".to_string(),
                auth: SshAuth::Password {
                    password: "my-secret-password".to_string(),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: Some(30),
            }),
            group: Some("Production".to_string()),
            tags: vec!["linux".to_string()],
            onekey: true,
        };

        cm.save_connections(&[conn.clone()]).unwrap();
        let loaded = cm.load_connections().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "test-1");
        assert_eq!(loaded[0].name, "Test Server");

        if let ConnectionKind::Ssh(ssh) = &loaded[0].kind {
            if let SshAuth::Password { password } = &ssh.auth {
                assert_eq!(password, "my-secret-password");
            } else {
                panic!("Expected Password auth");
            }
        } else {
            panic!("Expected SSH connection");
        }

        let json_content = fs::read_to_string(&cm.config_path).unwrap();
        assert!(
            !json_content.contains("my-secret-password"),
            "Password should be encrypted, not plaintext in JSON"
        );
        assert!(json_content.contains("_encrypted"));
    }

    #[test]
    fn test_save_and_load_ssh_key_with_passphrase() {
        let (cm, _dir) = test_config_manager();
        let conn = ConnectionConfig {
            id: "test-2".to_string(),
            name: "Key Server".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "10.0.0.1".to_string(),
                port: 22,
                username: "admin".to_string(),
                auth: SshAuth::Key {
                    private_key_path: "~/.ssh/id_ed25519".to_string(),
                    passphrase: Some("key-passphrase".to_string()),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: None,
            }),
            group: None,
            tags: vec![],
            onekey: false,
        };

        cm.save_connections(&[conn.clone()]).unwrap();
        let loaded = cm.load_connections().unwrap();
        assert_eq!(loaded.len(), 1);

        if let ConnectionKind::Ssh(ssh) = &loaded[0].kind {
            if let SshAuth::Key {
                private_key_path,
                passphrase,
            } = &ssh.auth
            {
                assert_eq!(private_key_path, "~/.ssh/id_ed25519");
                assert_eq!(passphrase.as_deref(), Some("key-passphrase"));
            } else {
                panic!("Expected Key auth");
            }
        } else {
            panic!("Expected SSH connection");
        }
    }

    #[test]
    fn test_save_and_load_non_ssh() {
        let (cm, _dir) = test_config_manager();
        let conns = vec![
            ConnectionConfig {
                id: "serial-1".to_string(),
                name: "Router Console".to_string(),
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
            },
            ConnectionConfig {
                id: "tcp-1".to_string(),
                name: "Raw TCP".to_string(),
                kind: ConnectionKind::Tcp(TcpConfig {
                    host: "10.0.0.1".to_string(),
                    port: 8080,
                }),
                group: None,
                tags: vec![],
                onekey: false,
            },
            ConnectionConfig {
                id: "telnet-1".to_string(),
                name: "Legacy".to_string(),
                kind: ConnectionKind::Telnet(TelnetConfig {
                    host: "192.168.1.1".to_string(),
                    port: 23,
                }),
                group: None,
                tags: vec![],
                onekey: false,
            },
        ];

        cm.save_connections(&conns).unwrap();
        let loaded = cm.load_connections().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].id, "serial-1");
        assert_eq!(loaded[1].id, "tcp-1");
        assert_eq!(loaded[2].id, "telnet-1");
    }

    #[test]
    fn test_load_missing_file() {
        let (cm, _dir) = test_config_manager();
        let loaded = cm.load_connections().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_json_format_has_encrypted_marker() {
        let (cm, _dir) = test_config_manager();
        let conn = ConnectionConfig {
            id: "test-3".to_string(),
            name: "Check Format".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "host".to_string(),
                port: 22,
                username: "user".to_string(),
                auth: SshAuth::Password {
                    password: "secret".to_string(),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: None,
            }),
            group: None,
            tags: vec![],
            onekey: false,
        };

        cm.save_connections(&[conn]).unwrap();
        let json = fs::read_to_string(&cm.config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["version"], 1);
        assert!(parsed["connections"].is_array());
        assert_eq!(parsed["connections"][0]["name"], "Check Format");
        assert!(parsed["connections"][0]["kind"]["Ssh"]["auth"]["Password"]["password"]["_encrypted"].is_string());
    }

    #[test]
    fn test_master_password_flow() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test_settings.json");

        // First run: create with master password
        let key = rusterm_crypto::derive_key("mypassword", KEY_DERIVATION_SALT).unwrap();
        let hash = ConfigManager::hash_password("mypassword").unwrap();
        let cm = ConfigManager {
            config_path: config_path.clone(),
            master_key: key,
            master_password_hash: Some(hash),
        };

        let conn = ConnectionConfig {
            id: "test-mp".to_string(),
            name: "MP Test".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "host".to_string(),
                port: 22,
                username: "user".to_string(),
                auth: SshAuth::Password {
                    password: "secret123".to_string(),
                },
                terminal_type: "xterm-256color".to_string(),
                proxy_jump: None,
                keepalive_interval: None,
            }),
            group: None,
            tags: vec![],
            onekey: false,
        };

        cm.save_connections(&[conn]).unwrap();

        // Verify hash is stored
        let json = fs::read_to_string(&config_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["master_password_hash"].is_string());

        // Reload with same password
        let key2 = rusterm_crypto::derive_key("mypassword", KEY_DERIVATION_SALT).unwrap();
        let cm2 = ConfigManager {
            config_path: config_path.clone(),
            master_key: key2,
            master_password_hash: parsed["master_password_hash"].as_str().map(|s| s.to_string()),
        };
        let loaded = cm2.load_connections().unwrap();
        assert_eq!(loaded.len(), 1);

        if let ConnectionKind::Ssh(ssh) = &loaded[0].kind {
            if let SshAuth::Password { password } = &ssh.auth {
                assert_eq!(password, "secret123");
            }
        }
    }
}
