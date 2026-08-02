//! Relay configuration: HTTP bind settings and the accounts allowed to
//! authenticate. Stored as standalone JSON in the app config dir
//! (`relay.json`) — deliberately NOT in `PersistedConfig`, whose
//! read-modify-write pattern makes adding fields expensive.
//!
//! Passwords are never stored in plaintext: each account carries an Argon2id
//! PHC string produced by [`hash_password`].

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use rusterm_core::paths::resolve_config_file_path;

pub const RELAY_CONFIG_FILE: &str = "relay.json";
pub const DEFAULT_PORT: u16 = 8877;
pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Master switch — the HTTP server only runs when `true`.
    pub enabled: bool,
    /// Bind address. Loopback by default; `0.0.0.0` requires explicit user
    /// confirmation in the UI because the API executes remote commands.
    pub bind_addr: IpAddr,
    pub port: u16,
    pub accounts: Vec<RelayAccount>,
    /// Request body size cap (defence against memory DoS).
    pub max_body_bytes: usize,
    /// Wall-clock limit applied to each `/exec` request end-to-end unless a
    /// smaller per-request timeout is supplied.
    pub request_timeout_ms: u64,
    /// Max SSH command executions per minute, per account.
    pub per_account_rate_limit: u32,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_addr: std::net::Ipv4Addr::LOCALHOST.into(),
            port: DEFAULT_PORT,
            accounts: Vec::new(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            per_account_rate_limit: 60,
        }
    }
}

/// One BasicAuth account. The username is the lookup key; the password is
/// stored as an Argon2id PHC string (`$argon2id$v=19$...`).
///
/// `allowed_hosts` and `allowed_commands` implement the per-account
/// authorization matrix the user asked for: "在 json 中定义要在哪个终端执行哪个
/// 命令". An empty `allowed_hosts` means "all saved hosts"; an empty
/// `allowed_commands` means "all commands that pass the safety validator".
/// Non-empty values are regexes matched against the full command line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayAccount {
    pub username: String,
    pub password_hash: String,
    /// Saved-host ids (or names) this account may target. Empty = all.
    pub allowed_hosts: Vec<String>,
    /// Regex allowlist; empty = all commands that pass validation.
    pub allowed_commands: Vec<String>,
    /// Read-only accounts may only run commands classified as non-mutating
    /// (see `validator::is_readonly_command`).
    pub readonly: bool,
}

impl Default for RelayAccount {
    fn default() -> Self {
        Self {
            username: String::new(),
            password_hash: String::new(),
            allowed_hosts: Vec::new(),
            allowed_commands: Vec::new(),
            readonly: false,
        }
    }
}

impl RelayConfig {
    pub fn load() -> anyhow::Result<Self> {
        let path = resolve_config_file_path(RELAY_CONFIG_FILE)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = resolve_config_file_path(RELAY_CONFIG_FILE)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find_account(&self, username: &str) -> Option<&RelayAccount> {
        self.accounts.iter().find(|a| a.username == username)
    }

    /// Whether the configured bind address exposes the API beyond this
    /// machine. The UI must confirm this transition.
    pub fn binds_publicly(&self) -> bool {
        !self.bind_addr.is_loopback()
    }
}

// ── Password hashing ─────────────────────────────────────────────────────

/// Hash a plaintext password with Argon2id. Returns the PHC string to store
/// in `RelayAccount::password_hash`.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::default(),
    );
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2 hashing failed: {e}"))?;
    Ok(hash.to_string())
}

/// Verify a candidate password against a stored PHC string. Returns `false`
/// (never errors) on any mismatch or malformed hash so callers cannot
/// distinguish "user unknown" from "bad password".
pub fn verify_password(password_hash: &str, candidate: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let parsed = match PasswordHash::new(password_hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    argon2::Argon2::default()
        .verify_password(candidate.as_bytes(), &parsed)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_loopback_and_disabled() {
        let cfg = RelayConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.bind_addr.is_loopback());
        assert!(!cfg.binds_publicly());
        assert_eq!(cfg.port, DEFAULT_PORT);
    }

    #[test]
    fn public_bind_detection() {
        let mut cfg = RelayConfig::default();
        cfg.bind_addr = "0.0.0.0".parse().unwrap();
        assert!(cfg.binds_publicly());
    }

    #[test]
    fn password_hash_roundtrip() {
        let hash = hash_password("s3cret!").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "s3cret!"));
        assert!(!verify_password(&hash, "wrong"));
    }

    #[test]
    fn verify_rejects_malformed_hash() {
        assert!(!verify_password("not-a-phc-string", "anything"));
    }

    #[test]
    fn config_json_roundtrip() {
        let mut cfg = RelayConfig::default();
        cfg.accounts.push(RelayAccount {
            username: "ops".into(),
            password_hash: hash_password("pw").unwrap(),
            allowed_hosts: vec!["prod-web-1".into()],
            allowed_commands: vec![r"^docker ps".into()],
            readonly: true,
        });
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RelayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.accounts.len(), 1);
        assert_eq!(back.accounts[0].username, "ops");
        assert!(back.accounts[0].readonly);
        assert!(back.accounts[0].allowed_hosts[0] == "prod-web-1");
    }
}
