//! Encrypted session-state snapshot — restores the user's previous sessions
//! after the app is closed and reopened.
//!
//! # What this is (and isn't)
//!
//! `SessionState` captures the bare minimum needed to **restore the user's
//! working context** on next launch: which sessions were open, what kind they
//! were (SSH/Shell/Serial/...), and the last working directory (`cwd`) of each.
//!
//! It deliberately does NOT persist:
//! - scrollback buffers (too large, slow to encrypt, can't be truly restored),
//! - environment variables (would leak secrets like `AWS_SECRET_ACCESS_KEY`),
//! - the foreground PTY process state (impossible to restore across restart),
//! - the contents of the input box (too transient),
//! - and crucially, **it never re-executes any past command or script** —
//!   the user explicitly asked us not to, because doing so could cause
//!   destructive side effects on the next launch.
//!
//! On restore we send a single `cd '<last_cwd>'` to each session's shell after
//! the shell is ready. For sessions where shell integration didn't take (raw
//! telnet, serial, broken shells) the `cwd` is `None` and the `cd` is silently
//! skipped.
//!
//! # File format
//!
//! ```text
//! magic:    b"RUSS"  (4 bytes)
//! version:  u8 = 1
//! reserved: [u8; 3] = [0, 0, 0]
//! nonce:    [u8; 12]   (AES-256-GCM nonce, generated fresh on every save)
//! payload:  AES-256-GCM(nonce, master_key, bincode(SessionState))
//! ```
//!
//! The whole payload (bincode-encoded `SessionState`) is sealed in a single
//! AEAD call, so tampering with any byte invalidates the entire file — this
//! is the right granularity because the file is small (a few KiB at most) and
//! we want the user to know immediately if their saved state is corrupt rather
//! than loading a half-correct snapshot.
//!
//! Saves are atomic (temp file + rename), copying the pattern from
//! [`crate::window_state::WindowState::save_to`].
//!
//! # Threat model
//!
//! The master key comes from `ConfigManager::master_key()` — derived from the
//! user's master password via Argon2id. Without the master password the file is
//! opaque. The file is stored locally only, never sent anywhere (the sync
//! crate's threat model explicitly excludes per-session transient state).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use rusterm_crypto::{decrypt_data, encrypt_data};

use crate::session::SessionType;

const MAGIC: &[u8; 4] = b"RUSS";
const VERSION: u8 = 1;
const FILE_NAME: &str = "session_state.enc";

/// Top-level persisted snapshot. One per app exit; one per app launch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    /// Schema version — bump when the on-disk layout changes. Older versions
    /// are rejected on load rather than migrated, because the data is
    /// ephemeral (the user just loses their last session's restore, which is
    /// acceptable on a format change).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// When this snapshot was taken (UTC). Mostly informational — the UI
    /// shows it as "上次会话：2025-07-18 14:32" so the user can decide whether
    /// the snapshot is stale.
    pub saved_at: chrono::DateTime<chrono::Utc>,

    /// ID of the session that was active when the snapshot was taken, if any.
    /// On restore the UI selects this tab last so the user lands where they
    /// left off.
    #[serde(default)]
    pub active_session: Option<String>,

    /// Per-session restore data. Order is preserved (first tab → first session).
    #[serde(default)]
    pub sessions: Vec<PersistedSession>,

    /// Last-selected theme name ("Dark" | "Light" | ...). Restored so the
    /// user doesn't see a theme flicker on launch.
    #[serde(default)]
    pub theme: Option<String>,
}

fn default_schema_version() -> u32 {
    1
}

/// Per-session restore data. Everything needed to reconnect + `cd` back to
/// the last working directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSession {
    /// Session id. Preserved across restart so reconnect feels continuous
    /// (the same id is reused for the new session).
    pub id: String,

    /// Tab title (e.g. "user@host" for SSH, "Local" for shell).
    #[serde(default)]
    pub name: String,

    /// What kind of session to open. `Ssh` requires `connection_id` to be set
    /// so we can look up the host/auth; `Shell` opens a local PTY.
    pub kind: SessionType,

    /// For SSH/Telnet/TCP: hostname of the remote, for display before the
    /// connection is re-established. Informational only.
    #[serde(default)]
    pub hostname: Option<String>,

    /// For SSH/Telnet/TCP: index into `AppState::connections`, so we can
    /// look up the `ConnectionConfig` to reconnect. `None` for `Shell` and
    /// `Serial` sessions.
    #[serde(default)]
    pub connection_id: Option<String>,

    /// Last working directory of the session, as reported by the shell via
    /// OSC 7 (`ESC ] 7 ; file://<host><path> ST`). `None` if the shell never
    /// reported one (raw telnet/serial, or shell integration didn't take).
    /// On restore we send `cd '<cwd>'\n` once the shell is ready.
    #[serde(default)]
    pub cwd: Option<String>,

    /// Tail of the command history (last N entries, default 100). Used to
    /// re-seed the in-memory command-history suggestions on restore so the
    /// user sees their recent commands in the ghost-text suggester. These are
    /// NEVER re-executed — they're display-only.
    #[serde(default)]
    pub command_history_tail: Vec<String>,
}

impl SessionState {
    /// Resolve the path to the session-state file.
    ///
    /// Mirrors `ConfigManager::resolve_config_path`'s precedence:
    /// 1. `RUSTERM_CONFIG_DIR` env var (test/config override),
    /// 2. next to the binary (same dir as `settings.json`),
    /// 3. platform config dir fallback.
    ///
    /// This keeps the file co-located with `settings.json` so a portable
    /// install on a USB stick carries its saved session state with it.
    pub fn resolve_path() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("RUSTERM_CONFIG_DIR") {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path).ok();
            return Ok(path.join(FILE_NAME));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return Ok(parent.join(FILE_NAME));
            }
        }
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm");
        std::fs::create_dir_all(&config_dir).ok();
        Ok(config_dir.join(FILE_NAME))
    }

    /// Load and decrypt the saved session state. Returns `Ok(None)` if the
    /// file doesn't exist (first launch, or user deleted it). Any other error
    /// (corrupt, tampered, wrong key) is returned as `Err` — the caller
    /// decides whether to log+ignore or surface to the user.
    pub fn load(master_key: &[u8; 32]) -> Result<Option<Self>> {
        let path = Self::resolve_path()?;
        Self::load_from(&path, master_key)
    }

    /// Load from a specific path — used by tests to avoid env-var races.
    pub fn load_from(path: &Path, master_key: &[u8; 32]) -> Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading session_state.enc"),
        };

        if bytes.len() < 8 {
            anyhow::bail!("session_state.enc too short for header");
        }
        if &bytes[0..4] != MAGIC {
            anyhow::bail!("session_state.enc has wrong magic");
        }
        if bytes[4] != VERSION {
            anyhow::bail!(
                "session_state.enc has unsupported version {} (expected {})",
                bytes[4],
                VERSION
            );
        }

        // bytes[5..8] are reserved (currently zero).
        let ciphertext = &bytes[8..];
        let plaintext = decrypt_data(master_key, ciphertext).context("decrypting session state")?;
        let state: SessionState =
            bincode::deserialize(&plaintext).context("deserializing session state")?;

        // Reject future-schema snapshots rather than half-loading them.
        if state.schema_version != 1 {
            anyhow::bail!(
                "session_state.enc has unsupported schema_version {} (expected 1)",
                state.schema_version
            );
        }

        Ok(Some(state))
    }

    /// Encrypt and atomically persist the snapshot.
    ///
    /// Writes `magic | version | reserved | nonce || ciphertext+tag` to a temp
    /// file, then renames over the target. The temp file is in the same dir so
    /// the rename is atomic on POSIX (and best-effort on Windows).
    pub fn save(&self, master_key: &[u8; 32]) -> Result<()> {
        let path = Self::resolve_path()?;
        self.save_to(&path, master_key)
    }

    /// Save to a specific path — used by tests.
    pub fn save_to(&self, path: &Path, master_key: &[u8; 32]) -> Result<()> {
        let plaintext = bincode::serialize(self).context("serializing session state")?;
        // `encrypt_data` returns `nonce || ciphertext || tag` as a single Vec —
        // we store that whole blob after the header. On load we feed everything
        // after the header to `decrypt_data`, which expects exactly this shape.
        let ciphertext =
            encrypt_data(master_key, &plaintext).context("encrypting session state")?;

        let mut out = Vec::with_capacity(8 + ciphertext.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&[0u8; 3]); // reserved
        out.extend_from_slice(&ciphertext);

        let temp_path = path.with_extension("enc.tmp");
        std::fs::write(&temp_path, &out).context("writing temp session_state.enc")?;
        std::fs::rename(&temp_path, path).context("renaming session_state.enc")?;
        Ok(())
    }

    /// Delete the session-state file. Called when the user picks "不再询问"
    /// (don't ask again) on the restore dialog — we don't want to re-prompt
    /// on every launch, so we wipe the file and let future saves be skipped
    /// (the UI gates saves on a settings flag).
    pub fn delete() -> Result<()> {
        let path = Self::resolve_path()?;
        if path.exists() {
            std::fs::remove_file(&path).context("deleting session_state.enc")?;
        }
        Ok(())
    }

    /// Helper for callers that just want to know if a saved state exists
    /// without decrypting it (e.g. to decide whether to show the restore
    /// dialog after unlock — though we always load anyway to get the data).
    pub fn exists() -> bool {
        Self::resolve_path().map(|p| p.exists()).unwrap_or(false)
    }
}

/// Convenience wrapper that holds the master key in `Zeroizing` memory so it's
/// wiped on drop. Lets callers pass `&MasterKey` around without worrying about
/// exposing the raw bytes.
#[derive(Debug)]
pub struct MasterKey(Zeroizing<[u8; 32]>);

impl MasterKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(Zeroizing::new(key))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<Zeroizing<[u8; 32]>> for MasterKey {
    fn from(key: Zeroizing<[u8; 32]>) -> Self {
        Self(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_key() -> [u8; 32] {
        // Deterministic test key. NOT used anywhere outside tests.
        [0x42u8; 32]
    }

    fn sample_state() -> SessionState {
        SessionState {
            schema_version: 1,
            saved_at: chrono::DateTime::parse_from_rfc3339("2025-07-18T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            active_session: Some("sess-1".to_string()),
            sessions: vec![PersistedSession {
                id: "sess-1".to_string(),
                name: "user@host".to_string(),
                kind: SessionType::Ssh,
                hostname: Some("example.com".to_string()),
                connection_id: Some("conn-1".to_string()),
                cwd: Some("/home/user/projects".to_string()),
                command_history_tail: vec!["ls -la".to_string(), "cd src".to_string()],
            }],
            theme: Some("Dark".to_string()),
        }
    }

    #[test]
    fn roundtrip_preserves_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();
        let state = sample_state();

        state.save_to(&path, &key).unwrap();
        let loaded = SessionState::load_from(&path, &key).unwrap().unwrap();

        assert_eq!(state, loaded);
    }

    #[test]
    fn load_returns_none_on_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        assert!(
            SessionState::load_from(&path, &test_key())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn load_rejects_wrong_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();
        let wrong_key = [0x99u8; 32];

        sample_state().save_to(&path, &key).unwrap();
        let result = SessionState::load_from(&path, &wrong_key);
        assert!(result.is_err(), "decryption with wrong key must fail");
    }

    #[test]
    fn load_rejects_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        std::fs::write(&path, b"XXXX\x01\x00\x00\x00garbage").unwrap();
        assert!(SessionState::load_from(&path, &test_key()).is_err());
    }

    #[test]
    fn load_rejects_bad_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        std::fs::write(&path, b"RUSS\x02\x00\x00\x00garbage").unwrap();
        assert!(SessionState::load_from(&path, &test_key()).is_err());
    }

    #[test]
    fn load_rejects_tampered_ciphertext() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();

        sample_state().save_to(&path, &key).unwrap();

        // Flip a byte in the ciphertext region (after the 8-byte header).
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[20] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        assert!(
            SessionState::load_from(&path, &key).is_err(),
            "tampered ciphertext must fail AEAD verification"
        );
    }

    #[test]
    fn plaintext_never_appears_on_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();

        let state = SessionState {
            schema_version: 1,
            saved_at: chrono::Utc::now(),
            active_session: Some("sess-secret-id".to_string()),
            sessions: vec![PersistedSession {
                id: "sess-secret-id".to_string(),
                name: "user@secret-host".to_string(),
                kind: SessionType::Ssh,
                hostname: Some("secret-host.example.com".to_string()),
                connection_id: Some("conn-secret".to_string()),
                cwd: Some("/home/user/secret-project".to_string()),
                command_history_tail: vec!["ssh production-db".to_string()],
            }],
            theme: Some("Dark".to_string()),
        };
        state.save_to(&path, &key).unwrap();

        let raw = std::fs::read(&path).unwrap();
        // None of these sensitive strings should appear in the on-disk bytes.
        for needle in [
            "sess-secret-id",
            "user@secret-host",
            "secret-host.example.com",
            "conn-secret",
            "/home/user/secret-project",
            "ssh production-db",
        ] {
            assert!(
                !raw.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "plaintext fragment {:?} must not appear in session_state.enc",
                needle
            );
        }
    }

    #[test]
    fn save_is_atomic_temp_file_gone() {
        // After a successful save the temp file should be gone (renamed over).
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let temp_path = path.with_extension("enc.tmp");

        sample_state().save_to(&path, &test_key()).unwrap();

        assert!(!temp_path.exists(), "temp file should have been renamed");
        assert!(path.exists(), "final file should exist");
    }

    #[test]
    fn delete_removes_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        // Write a file directly so we don't depend on resolve_path env vars.
        std::fs::write(&path, b"RUSS\x01\x00\x00\x00garbage").unwrap();
        assert!(path.exists());
        std::fs::remove_file(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn empty_sessions_roundtrips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();

        let state = SessionState {
            schema_version: 1,
            saved_at: chrono::Utc::now(),
            active_session: None,
            sessions: vec![],
            theme: None,
        };
        state.save_to(&path, &key).unwrap();
        let loaded = SessionState::load_from(&path, &key).unwrap().unwrap();
        assert_eq!(state, loaded);
        assert!(loaded.sessions.is_empty());
    }

    #[test]
    fn multiple_sessions_preserve_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = test_key();

        let state = SessionState {
            schema_version: 1,
            saved_at: chrono::Utc::now(),
            active_session: Some("sess-2".to_string()),
            sessions: vec![
                PersistedSession {
                    id: "sess-1".to_string(),
                    name: "Local".to_string(),
                    kind: SessionType::Shell,
                    hostname: None,
                    connection_id: None,
                    cwd: Some("/tmp".to_string()),
                    command_history_tail: vec![],
                },
                PersistedSession {
                    id: "sess-2".to_string(),
                    name: "prod".to_string(),
                    kind: SessionType::Ssh,
                    hostname: Some("prod.example.com".to_string()),
                    connection_id: Some("conn-prod".to_string()),
                    cwd: Some("/var/log".to_string()),
                    command_history_tail: vec!["tail -f syslog".to_string()],
                },
                PersistedSession {
                    id: "sess-3".to_string(),
                    name: "serial".to_string(),
                    kind: SessionType::Serial,
                    hostname: None,
                    connection_id: None,
                    cwd: None, // serial sessions don't report cwd
                    command_history_tail: vec![],
                },
            ],
            theme: Some("Light".to_string()),
        };
        state.save_to(&path, &key).unwrap();
        let loaded = SessionState::load_from(&path, &key).unwrap().unwrap();

        assert_eq!(loaded.sessions.len(), 3);
        assert_eq!(loaded.sessions[0].id, "sess-1");
        assert_eq!(loaded.sessions[1].id, "sess-2");
        assert_eq!(loaded.sessions[2].id, "sess-3");
        assert_eq!(loaded.active_session, Some("sess-2".to_string()));
        // Serial session should have cwd=None preserved.
        assert!(loaded.sessions[2].cwd.is_none());
    }
}
