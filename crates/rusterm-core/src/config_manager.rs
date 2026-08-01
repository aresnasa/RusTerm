use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rand::RngCore;

use crate::config::{
    ConnectionConfig, ConnectionKind, DEFAULT_ONEKEY_PASSWORD_EXPECT, EncryptedValue,
    FocusedTabAppearance, OneKey, OneKeyStep, PersistedConfig, PersistedConnection,
    PersistedConnectionKind, PersistedOneKey, PersistedOneKeyStep, PersistedSshAuth,
    PersistedSshConfig, SidebarPreferences, SshAuth, SshConfig,
};
use rusterm_crypto::{KeyringStore, decrypt_data, encrypt_data};

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
        // Delegate to the centralised path resolver. See
        // `rusterm_core::paths` for the full resolution order and the
        // rationale (short version: platform config dir is now the
        // primary location so `cargo clean` doesn't wipe the user's
        // saved connections during development).
        crate::paths::resolve_config_file_path(CONFIG_FILE_NAME)
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
                    tracing::warn!(
                        "OS keyring unavailable, deriving master key from machine ID: {e}"
                    );
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
        let content = fs::read_to_string(config_path).context("Failed to read settings.json")?;
        let persisted: serde_json::Value =
            serde_json::from_str(&content).context("Failed to parse settings.json")?;
        Ok(persisted
            .get("master_password_hash")
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

    /// Read the `restore_disabled` flag from settings.json. Used on unlock to
    /// decide whether to even attempt loading `session_state.enc` (no point
    /// decrypting a file we'll never restore from).
    pub fn load_restore_disabled(&self) -> bool {
        self.read_persisted().restore_disabled
    }

    /// Load the focused-pane tab outline, falling back to a safe light outline
    /// for older or manually edited settings files.
    pub fn load_focused_tab_appearance(&self) -> FocusedTabAppearance {
        self.read_persisted().focused_tab_appearance.normalized()
    }

    /// Persist only the focused-pane tab appearance while preserving encrypted
    /// connections, OneKeys, and all other settings.
    pub fn save_focused_tab_appearance(&self, appearance: FocusedTabAppearance) -> Result<()> {
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: appearance.normalized(),
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    /// Persist the `restore_disabled` flag to settings.json. Used when the user
    /// picks "不再询问" on the restore dialog — we set the flag so we never
    /// prompt again (and stop saving session state entirely).
    /// Preserves existing connections + OneKeys (read-modify-write).
    pub fn save_restore_disabled(&self, restore_disabled: bool) -> Result<()> {
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    /// Read the `confirm_close_on_exit` flag from settings.json. Used on unlock
    /// to decide whether to show the "是否确实要关闭本软件？" dialog when the
    /// user closes the last window. Defaults to true (safe default — always
    /// ask) for older settings files that predate the field.
    pub fn load_confirm_close_on_exit(&self) -> bool {
        self.read_persisted().confirm_close_on_exit
    }

    /// Persist the `confirm_close_on_exit` flag to settings.json. Used when the
    /// user toggles the "下次关闭时不再询问" checkbox on the close-confirmation
    /// dialog. Preserves existing connections + OneKeys (read-modify-write).
    pub fn save_confirm_close_on_exit(&self, confirm_close_on_exit: bool) -> Result<()> {
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    /// Read the suggestion-popup settings from settings.json. Returns
    /// `(enabled, count)` where `enabled` controls whether suggestions are
    /// shown at all, and `count` is the max number of items (3/5/10).
    pub fn load_suggestion_settings(&self) -> (bool, u8) {
        let cfg = self.read_persisted();
        (cfg.suggestion_enabled, cfg.suggestion_count)
    }

    /// Persist the suggestion-popup settings to settings.json. Preserves all
    /// other fields (read-modify-write). `count` is clamped to {3, 5, 10} with
    /// a fallback to 3 for invalid values.
    pub fn save_suggestion_settings(&self, enabled: bool, count: u8) -> Result<()> {
        let count = match count {
            5 => 5,
            10 => 10,
            _ => 3, // 3 is the default/compact value
        };
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: enabled,
            suggestion_count: count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    /// Load normalized connection-sidebar preferences from settings.json.
    pub fn load_sidebar_preferences(&self) -> SidebarPreferences {
        self.read_persisted().sidebar.normalized()
    }

    /// Persist connection-sidebar preferences while preserving encrypted
    /// connections, OneKeys, and every unrelated setting.
    pub fn save_sidebar_preferences(&self, sidebar: &SidebarPreferences) -> Result<()> {
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: sidebar.clone().normalized(),
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;
        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;
        Ok(())
    }

    /// Expose the master key for use by other components that need to encrypt
    /// sensitive user data at rest (e.g. `SessionLog`). The key itself is
    /// never written to logs — see `Debug for ConfigManager` above, which
    /// redacts it.
    ///
    /// Returns a `Zeroizing` wrapper so callers don't accidentally leave the
    /// key material in unzeroed memory.
    pub fn master_key(&self) -> zeroize::Zeroizing<[u8; 32]> {
        zeroize::Zeroizing::new(self.master_key)
    }

    /// Expose the resolved path to `settings.json` so other components
    /// (notably `rusterm-sync`, which needs to read/write the file to push
    /// it to a cloud backend) can locate it without duplicating the
    /// resolution logic in [`Self::resolve_config_path`].
    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    /// Derive a per-session subkey from the master key + session ID. This is
    /// used by `SessionLog` to encrypt that session's I/O with a key that's
    /// scoped to the session — compromising one session's log file does not
    /// reveal data from other sessions.
    ///
    /// Derivation is Argon2id with the session ID as salt, which is sufficient
    /// because the master key is already high-entropy.
    pub fn derive_session_key(&self, session_id: &str) -> Result<[u8; 32]> {
        let salt = session_id.as_bytes();
        // Pad salt to Argon2's minimum 8 bytes if the session ID is unusually
        // short (UUIDs are 36 chars, so this is defensive only).
        let salt_padded: Vec<u8> = if salt.len() < 8 {
            let mut v = salt.to_vec();
            v.resize(8, 0);
            v
        } else {
            salt.to_vec()
        };
        // Convert master key to a hex string for use as the Argon2 "password"
        // input. (Argon2 takes bytes; we just need a deterministic high-entropy
        // preimage.)
        let master_hex = base64::engine::general_purpose::STANDARD.encode(self.master_key);
        rusterm_crypto::derive_key(&master_hex, &salt_padded)
    }

    pub fn save_connections(&self, connections: &[ConnectionConfig]) -> Result<()> {
        // Preserve existing OneKeys (read-modify-write) so saving connections
        // doesn't clobber the OneKey library.
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: connections
                .iter()
                .map(|c| self.encrypt_connection(c))
                .collect::<Result<Vec<_>>>()?,
            onekeys: existing.onekeys,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    /// Read the on-disk PersistedConfig (or an empty default if missing/unparseable).
    fn read_persisted(&self) -> PersistedConfig {
        if !self.config_path.exists() {
            return PersistedConfig {
                version: CONFIG_VERSION,
                connections: vec![],
                onekeys: vec![],
                master_password_hash: None,
                restore_disabled: false,
                confirm_close_on_exit: true,
                focused_tab_appearance: FocusedTabAppearance::default(),
                suggestion_enabled: true,
                suggestion_count: 3,
                sidebar: SidebarPreferences::default(),
            };
        }
        fs::read_to_string(&self.config_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or(PersistedConfig {
                version: CONFIG_VERSION,
                connections: vec![],
                onekeys: vec![],
                master_password_hash: None,
                restore_disabled: false,
                confirm_close_on_exit: true,
                focused_tab_appearance: FocusedTabAppearance::default(),
                suggestion_enabled: true,
                suggestion_count: 3,
                sidebar: SidebarPreferences::default(),
            })
    }

    /// Save the OneKey library. Preserves existing connections (read-modify-write).
    pub fn save_onekeys(&self, onekeys: &[OneKey]) -> Result<()> {
        let existing = self.read_persisted();
        let persisted = PersistedConfig {
            version: CONFIG_VERSION,
            connections: existing.connections,
            onekeys: onekeys
                .iter()
                .map(|ok| self.encrypt_onekey(ok))
                .collect::<Result<Vec<_>>>()?,
            master_password_hash: self.master_password_hash.clone(),
            restore_disabled: existing.restore_disabled,
            confirm_close_on_exit: existing.confirm_close_on_exit,
            focused_tab_appearance: existing.focused_tab_appearance,
            suggestion_enabled: existing.suggestion_enabled,
            suggestion_count: existing.suggestion_count,
            sidebar: existing.sidebar,
        };

        let json =
            serde_json::to_string_pretty(&persisted).context("Failed to serialize config")?;

        let temp_path = self.config_path.with_extension("json.tmp");
        fs::write(&temp_path, &json).context("Failed to write config file")?;
        fs::rename(&temp_path, &self.config_path).context("Failed to rename temp config file")?;

        Ok(())
    }

    pub fn load_onekeys(&self) -> Result<Vec<OneKey>> {
        self.read_persisted()
            .onekeys
            .into_iter()
            .map(|pok| self.decrypt_onekey(pok))
            .collect()
    }

    fn encrypt_onekey(&self, ok: &OneKey) -> Result<PersistedOneKey> {
        let steps = ok
            .steps
            .iter()
            .map(|s| self.encrypt_step(s))
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedOneKey {
            id: ok.id.clone(),
            name: ok.name.clone(),
            steps,
        })
    }

    fn encrypt_step(&self, s: &OneKeyStep) -> Result<PersistedOneKeyStep> {
        Ok(PersistedOneKeyStep {
            label: s.label.clone(),
            expect: s.expect.clone(),
            send: self.encrypt_string(&s.send)?,
        })
    }

    fn decrypt_onekey(&self, pok: PersistedOneKey) -> Result<OneKey> {
        let steps = pok
            .steps
            .into_iter()
            .map(|step| self.decrypt_step(step))
            .collect::<Result<Vec<_>>>()?;

        // Earlier releases used incomplete password defaults: bare `password:`,
        // Git-only `password for \S+:`, and a combined rule that still omitted
        // SSH key passphrases. Upgrade only those known defaults; leave every
        // other user-authored regex unchanged.
        let has_git_username_step = steps
            .iter()
            .any(|step| step.expect.starts_with("Username for"));
        let steps = steps
            .into_iter()
            .map(|mut step| {
                let expect = step.expect.trim();
                let is_legacy_default = matches!(
                    expect,
                    r"password(?: for \S+)?:" | r"password for \S+:"
                ) || (has_git_username_step && expect == "password:");
                if is_legacy_default {
                    tracing::info!(
                        "Migrated a legacy OneKey Password expect to the general password prompt pattern"
                    );
                    step.expect = DEFAULT_ONEKEY_PASSWORD_EXPECT.to_string();
                }
                step
            })
            .collect();

        Ok(OneKey {
            id: pok.id,
            name: pok.name,
            steps,
        })
    }

    fn decrypt_step(&self, s: PersistedOneKeyStep) -> Result<OneKeyStep> {
        Ok(OneKeyStep {
            label: s.label,
            expect: s.expect,
            send: self.decrypt_value(&s.send)?,
        })
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
                host_key_policy: ssh.host_key_policy.clone(),
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
                host_key_policy: ssh.host_key_policy,
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
        ConnectionGroup, ConnectionKind, OneKey, OneKeyStep, SerialConfig, SidebarPreferences,
        SshAuth, SshConfig, TcpConfig, TelnetConfig, default_host_key_policy,
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
    fn focused_tab_appearance_roundtrips_and_survives_other_saves() {
        let (cm, _dir) = test_config_manager();
        let appearance = FocusedTabAppearance {
            border_color: "#f0e0d0".to_string(),
            border_width: 3,
            border_radius: 8,
        };

        cm.save_focused_tab_appearance(appearance.clone()).unwrap();
        assert_eq!(cm.load_focused_tab_appearance(), appearance);

        cm.save_connections(&[]).unwrap();
        cm.save_onekeys(&[]).unwrap();
        cm.save_restore_disabled(true).unwrap();

        assert_eq!(cm.load_focused_tab_appearance(), appearance);
        assert!(cm.load_restore_disabled());
    }

    #[test]
    fn sidebar_preferences_roundtrip_and_survive_other_saves() {
        let (cm, _dir) = test_config_manager();
        let preferences = SidebarPreferences {
            width_px: 388,
            hidden_connection_ids: vec!["hidden".to_string()],
            groups: vec![ConnectionGroup {
                id: "ops".to_string(),
                name: "Operations".to_string(),
                collapsed: true,
            }],
        };

        cm.save_sidebar_preferences(&preferences).unwrap();
        assert_eq!(cm.load_sidebar_preferences(), preferences);

        cm.save_connections(&[]).unwrap();
        cm.save_onekeys(&[]).unwrap();
        cm.save_restore_disabled(true).unwrap();
        cm.save_suggestion_settings(false, 10).unwrap();

        assert_eq!(cm.load_sidebar_preferences(), preferences);
    }

    #[test]
    fn confirm_close_on_exit_defaults_to_true_and_roundtrips() {
        let (cm, _dir) = test_config_manager();
        // Default must be true (safe default — always ask) for a brand-new
        // settings file that has never set the field.
        assert!(
            cm.load_confirm_close_on_exit(),
            "confirm_close_on_exit must default to true"
        );

        // Saving other fields must NOT clobber confirm_close_on_exit.
        cm.save_confirm_close_on_exit(false).unwrap();
        assert!(!cm.load_confirm_close_on_exit());

        cm.save_connections(&[]).unwrap();
        cm.save_onekeys(&[]).unwrap();
        cm.save_restore_disabled(true).unwrap();
        cm.save_focused_tab_appearance(FocusedTabAppearance::default())
            .unwrap();
        assert!(
            !cm.load_confirm_close_on_exit(),
            "confirm_close_on_exit must survive other saves"
        );

        // And flipping it back to true also roundtrips.
        cm.save_confirm_close_on_exit(true).unwrap();
        assert!(cm.load_confirm_close_on_exit());
    }

    #[test]
    fn test_save_and_load_empty() {
        let (cm, _dir) = test_config_manager();
        cm.save_connections(&[]).unwrap();
        let loaded = cm.load_connections().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_onekey_save_load_roundtrip() {
        let (cm, _dir) = test_config_manager();
        let onekeys = vec![OneKey {
            id: "ok1".to_string(),
            name: "git-inesa".to_string(),
            steps: vec![
                OneKeyStep {
                    label: "Username".to_string(),
                    expect: r"Username for \S+:".to_string(),
                    send: "my-user".to_string(),
                },
                OneKeyStep {
                    label: "Password".to_string(),
                    // Use the current default so this remains a pure round-trip.
                    expect: DEFAULT_ONEKEY_PASSWORD_EXPECT.to_string(),
                    send: "secret-token-123".to_string(),
                },
            ],
        }];
        cm.save_onekeys(&onekeys).unwrap();
        let loaded = cm.load_onekeys().unwrap();
        assert_eq!(loaded, onekeys);

        // Each step's `send` must be encrypted at rest — not plaintext in the file.
        let raw = std::fs::read_to_string(&cm.config_path).unwrap();
        assert!(!raw.contains("secret-token-123"));
        assert!(!raw.contains("my-user"));
    }

    #[test]
    fn onekey_credentials_roundtrip_after_manager_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("test_settings.json");
        let master_password = "test-only master password";
        let saved = vec![OneKey {
            id: "restart-roundtrip".to_string(),
            name: "special credentials".to_string(),
            steps: [
                " leading and trailing spaces ",
                "quotes-'\"-and-symbols-!@#$%^&*()",
                "unicode-sëcret-🔒-密码",
                "trailing-line-endings\r\n",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, send)| OneKeyStep {
                label: format!("Credential {index}"),
                expect: format!(r"prompt-{index}:"),
                send: send.to_string(),
            })
            .collect(),
        }];

        let writer = ConfigManager {
            config_path: config_path.clone(),
            master_key: rusterm_crypto::derive_key(master_password, KEY_DERIVATION_SALT).unwrap(),
            master_password_hash: Some(ConfigManager::hash_password(master_password).unwrap()),
        };
        writer.save_onekeys(&saved).unwrap();
        drop(writer);

        let reader = ConfigManager {
            config_path,
            master_key: rusterm_crypto::derive_key(master_password, KEY_DERIVATION_SALT).unwrap(),
            master_password_hash: Some(ConfigManager::hash_password(master_password).unwrap()),
        };
        let loaded = reader.load_onekeys().unwrap();

        assert!(
            loaded == saved,
            "OneKey credential data changed after recreating ConfigManager"
        );
    }

    #[test]
    fn test_onekey_password_expect_migrated_for_git_https() {
        let (cm, _dir) = test_config_manager();
        let onekeys = vec![OneKey {
            id: "ok-migrate".to_string(),
            name: "gitlab".to_string(),
            steps: vec![
                OneKeyStep {
                    label: "Username".to_string(),
                    expect: r"Username for \S+:".to_string(),
                    send: "user".to_string(),
                },
                OneKeyStep {
                    label: "Password".to_string(),
                    expect: "password:".to_string(),
                    send: "pass".to_string(),
                },
            ],
        }];

        cm.save_onekeys(&onekeys).unwrap();
        let loaded = cm.load_onekeys().unwrap();
        let steps = &loaded[0].steps;

        assert_eq!(steps[0].expect, r"Username for \S+:");
        assert_eq!(steps[1].expect, DEFAULT_ONEKEY_PASSWORD_EXPECT);
        assert_eq!(steps[1].send, "pass");
    }

    #[test]
    fn test_legacy_git_password_default_is_migrated_to_general_pattern() {
        let (cm, _dir) = test_config_manager();
        let onekeys = vec![OneKey {
            id: "ok-legacy-default".to_string(),
            name: "account".to_string(),
            steps: vec![OneKeyStep {
                label: "Password".to_string(),
                expect: r"password for \S+:".to_string(),
                send: "pass".to_string(),
            }],
        }];

        cm.save_onekeys(&onekeys).unwrap();
        let loaded = cm.load_onekeys().unwrap();

        assert_eq!(loaded[0].steps[0].expect, DEFAULT_ONEKEY_PASSWORD_EXPECT);
        assert_eq!(loaded[0].steps[0].send, "pass");
    }

    #[test]
    fn test_previous_general_password_default_is_migrated_for_passphrases() {
        let (cm, _dir) = test_config_manager();
        let onekeys = vec![OneKey {
            id: "ok-previous-default".to_string(),
            name: "account".to_string(),
            steps: vec![OneKeyStep {
                label: "Password".to_string(),
                expect: r"password(?: for \S+)?:".to_string(),
                send: "pass".to_string(),
            }],
        }];

        cm.save_onekeys(&onekeys).unwrap();
        let loaded = cm.load_onekeys().unwrap();

        assert_eq!(loaded[0].steps[0].expect, DEFAULT_ONEKEY_PASSWORD_EXPECT);
        assert_eq!(loaded[0].steps[0].send, "pass");
    }

    #[test]
    fn test_onekey_password_expect_not_migrated_without_username_step() {
        // A bare `password:` expect is correct for SSH password prompts (which
        // are literally `password:`). The migration must NOT touch OneKeys that
        // don't have a `Username for \S+:` step, otherwise SSH password autofill
        // would break.
        let (cm, _dir) = test_config_manager();
        let onekeys = vec![OneKey {
            id: "ok-ssh".to_string(),
            name: "ssh-host".to_string(),
            steps: vec![OneKeyStep {
                label: "Password".to_string(),
                expect: r"password:".to_string(),
                send: "ssh-pass".to_string(),
            }],
        }];
        cm.save_onekeys(&onekeys).unwrap();
        let loaded = cm.load_onekeys().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].steps[0].expect, r"password:",
            "bare 'password:' expect must be preserved when no Username step is present (SSH password prompt)"
        );
    }

    #[test]
    fn test_onekeys_preserved_when_saving_connections() {
        let (cm, _dir) = test_config_manager();
        cm.save_onekeys(&[OneKey {
            id: "ok1".to_string(),
            name: "n".to_string(),
            steps: vec![OneKeyStep {
                label: "l".to_string(),
                expect: "e".to_string(),
                send: "s".to_string(),
            }],
        }])
        .unwrap();
        // Saving connections must not clobber the OneKey library.
        cm.save_connections(&[]).unwrap();
        let loaded = cm.load_onekeys().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].steps[0].send, "s");
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
                host_key_policy: default_host_key_policy(),
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
                host_key_policy: default_host_key_policy(),
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
                host_key_policy: default_host_key_policy(),
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
        assert!(
            parsed["connections"][0]["kind"]["Ssh"]["auth"]["Password"]["password"]["_encrypted"]
                .is_string()
        );
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
                host_key_policy: default_host_key_policy(),
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
            master_password_hash: parsed["master_password_hash"]
                .as_str()
                .map(|s| s.to_string()),
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
