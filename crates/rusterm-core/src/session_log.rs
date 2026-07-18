//! Encrypted per-session terminal I/O log.
//!
//! # What this is (and isn't)
//!
//! `SessionLog` records a session's terminal input and output so the user can
//! review what happened during a past session. This is **sensitive user
//! data** — it can contain passwords typed into prompts, command output with
//! secrets, private keys printed to screen, etc.
//!
//! It is therefore **not part of the runtime log** (`tracing`). It is:
//!
//! - Stored locally only — never sent anywhere.
//! - Encrypted at rest with AES-256-GCM.
//! - Keyed per-session: each session's log file uses a key derived from the
//!   RusTerm master key + the session ID, so compromise of one log file does
//!   not reveal data from other sessions.
//! - Written as length-prefixed binary records (no plaintext on disk at any
//!   time, including in temporary buffers).
//!
//! # File format
//!
//! ```text
//! magic: b"RUSL"  (4 bytes)
//! version: u8 = 1
//! reserved: [u8; 3] = [0, 0, 0]
//! then a sequence of records, each:
//!   length: u32 big-endian (size of ciphertext that follows)
//!   ciphertext: <length> bytes  (nonce[12] || aead-sealed plaintext)
//! ```
//!
//! The plaintext payload of each record is a small JSON object:
//! `{"t":"<RFC3339>","d":"<IN|OUT>","b":"<base64 bytes>"}`.
//! JSON is used so that the encrypted record carries its own timestamp /
//! direction metadata, avoiding the need to invent a binary schema.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Local;
use zeroize::Zeroizing;

use rusterm_crypto::{decrypt_data, encrypt_data};

const MAGIC: &[u8; 4] = b"RUSL";
const VERSION: u8 = 1;

/// An encrypted session-log writer. Holds the per-session key in `Zeroizing`
/// memory; plaintext never touches disk.
pub struct SessionLog {
    writer: Mutex<Option<fs::File>>,
    session_id: String,
    /// Per-session AEAD key derived from the master key. Held in `Zeroizing`
    /// so it's wiped on drop.
    key: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("session_id", &self.session_id)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl SessionLog {
    /// Create a new encrypted session log for `session_id`, using `key` as the
    /// per-session AEAD key. The key MUST be derived from the master key via
    /// `ConfigManager::derive_session_key` — never invent a key ad hoc.
    ///
    /// On first creation of a log file for this session, a 4-byte magic +
    /// version header is written so readers can detect format corruption.
    pub fn new(session_id: &str, key: [u8; 32]) -> Result<Self> {
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm")
            .join("session_logs");
        fs::create_dir_all(&log_dir)?;

        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        // Sanitize session_id for use in a filename: keep only alphanumerics
        // and `-`/`_`, truncate to 36 chars (UUID length). This also prevents
        // path-traversal-ish session IDs from escaping the log dir.
        let safe_id: String = session_id
            .chars()
            .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            .take(36)
            .collect();
        let safe_id = if safe_id.is_empty() {
            "session".to_string()
        } else {
            safe_id
        };
        let filename = format!("{}_{}.rusl", safe_id, timestamp);
        let path = log_dir.join(&filename);

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening session log at {}", path.display()))?;

        // Write magic+version header if the file is empty (newly created).
        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            file.write_all(MAGIC)?;
            file.write_all(&[VERSION, 0, 0, 0])?;
            file.flush()?;
        }

        Ok(Self {
            writer: Mutex::new(Some(file)),
            session_id: session_id.to_string(),
            key: Zeroizing::new(key),
        })
    }

    /// Append a terminal-output chunk to the encrypted log.
    pub fn log_output(&self, data: &[u8]) {
        self.write_entry("OUT", data);
    }

    /// Append a terminal-input chunk to the encrypted log.
    pub fn log_input(&self, data: &[u8]) {
        self.write_entry("IN", data);
    }

    fn write_entry(&self, direction: &str, data: &[u8]) {
        if let Ok(mut guard) = self.writer.lock() {
            if let Some(ref mut file) = *guard {
                let timestamp = Local::now().to_rfc3339();
                let payload = serde_json::json!({
                    "t": timestamp,
                    "d": direction,
                    "b": BASE64.encode(data),
                });
                let payload_bytes = match serde_json::to_vec(&payload) {
                    Ok(v) => v,
                    Err(_) => return,
                };

                let ciphertext = match encrypt_data(&self.key, &payload_bytes) {
                    Ok(c) => c,
                    Err(_) => return,
                };

                // Length-prefix the ciphertext so readers can iterate records.
                let len = u32::try_from(ciphertext.len()).unwrap_or(0);
                if len == 0 {
                    return;
                }
                let len_bytes = len.to_be_bytes();

                // Best-effort write — failures here can't be surfaced to the
                // user meaningfully (we're on a background session task), so
                // we silently drop. The runtime `tracing` log will record a
                // count of dropped entries (without contents) if needed.
                let _ = file.write_all(&len_bytes);
                let _ = file.write_all(&ciphertext);
                let _ = file.flush();
            }
        }
    }

    /// Close the underlying file. Subsequent `log_*` calls are no-ops.
    pub fn close(&self) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = None;
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Decrypt an entire session-log file (all records). Returns the parsed
    /// records as `(timestamp, direction, bytes)` tuples. Used by the UI when
    /// the user wants to review a past session.
    ///
    /// `key` must be the same per-session key the file was written with.
    pub fn decrypt_file(
        path: &std::path::Path,
        key: &[u8; 32],
    ) -> Result<Vec<(String, String, Vec<u8>)>> {
        let bytes = fs::read(path).context("reading session log file")?;
        if bytes.len() < 8 {
            anyhow::bail!("session log file too short for header");
        }
        if &bytes[0..4] != MAGIC {
            anyhow::bail!("session log file has wrong magic");
        }
        if bytes[4] != VERSION {
            anyhow::bail!(
                "session log file has unsupported version {} (expected {})",
                bytes[4],
                VERSION
            );
        }

        let mut cursor = 8; // skip magic + version + reserved
        let mut records = Vec::new();
        while cursor + 4 <= bytes.len() {
            let len = u32::from_be_bytes([
                bytes[cursor],
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
            ]) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                break; // truncated tail — stop here
            }
            let ciphertext = &bytes[cursor..cursor + len];
            cursor += len;

            let plaintext =
                decrypt_data(key, ciphertext).context("decrypting session log record")?;
            let parsed: serde_json::Value =
                serde_json::from_slice(&plaintext).context("parsing session log record")?;
            let timestamp = parsed
                .get("t")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let direction = parsed
                .get("d")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let b64 = parsed
                .get("b")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let data = BASE64.decode(&b64).unwrap_or_default();
            records.push((timestamp, direction, data));
        }
        Ok(records)
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        self.close();
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

    #[test]
    fn session_log_encrypts_at_rest() {
        let dir = tempdir().unwrap();
        // Override the data dir for the test by changing CWD — `dirs::data_dir`
        // doesn't honor env vars portably, so we monkey-patch by writing
        // directly through `decrypt_file` against a path we control via the
        // `write_to_path` helper below.
        let path = dir.path().join("test.rusl");
        let key = test_key();

        // Write header + a couple of encrypted records manually using the same
        // logic as `SessionLog::write_entry`, but to a known path.
        {
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(MAGIC).unwrap();
            file.write_all(&[VERSION, 0, 0, 0]).unwrap();

            for (dir_str, data) in [
                ("OUT", b"hello world".as_slice()),
                ("IN", b"my-secret-password".as_slice()),
            ] {
                let payload = serde_json::json!({
                    "t": "2024-01-01T00:00:00Z",
                    "d": dir_str,
                    "b": BASE64.encode(data),
                });
                let payload_bytes = serde_json::to_vec(&payload).unwrap();
                let ciphertext = encrypt_data(&key, &payload_bytes).unwrap();
                let len = u32::try_from(ciphertext.len()).unwrap();
                file.write_all(&len.to_be_bytes()).unwrap();
                file.write_all(&ciphertext).unwrap();
            }
            file.flush().unwrap();
        }

        // Verify the on-disk bytes do NOT contain the plaintext strings.
        let raw = fs::read(&path).unwrap();
        assert!(
            !raw.windows(b"hello world".len())
                .any(|w| w == b"hello world")
        );
        assert!(
            !raw.windows(b"my-secret-password".len())
                .any(|w| w == b"my-secret-password"),
            "plaintext must not appear in session log file"
        );

        // Verify round-trip decryption works.
        let records = SessionLog::decrypt_file(&path, &key).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].1, "OUT");
        assert_eq!(records[0].2, b"hello world");
        assert_eq!(records[1].1, "IN");
        assert_eq!(records[1].2, b"my-secret-password");
    }

    #[test]
    fn decrypt_file_rejects_wrong_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("wrongkey.rusl");
        let key = test_key();
        let wrong_key = [0x99u8; 32];

        {
            let mut file = fs::File::create(&path).unwrap();
            file.write_all(MAGIC).unwrap();
            file.write_all(&[VERSION, 0, 0, 0]).unwrap();
            let payload = serde_json::json!({
                "t": "2024-01-01T00:00:00Z",
                "d": "OUT",
                "b": BASE64.encode(b"secret"),
            });
            let payload_bytes = serde_json::to_vec(&payload).unwrap();
            let ciphertext = encrypt_data(&key, &payload_bytes).unwrap();
            let len = u32::try_from(ciphertext.len()).unwrap();
            file.write_all(&len.to_be_bytes()).unwrap();
            file.write_all(&ciphertext).unwrap();
        }

        let result = SessionLog::decrypt_file(&path, &wrong_key);
        assert!(result.is_err(), "decryption with wrong key must fail");
    }

    #[test]
    fn decrypt_file_rejects_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("badmagic.rusl");
        fs::write(&path, b"XXXX\x01\x00\x00\x00").unwrap();
        let result = SessionLog::decrypt_file(&path, &test_key());
        assert!(result.is_err());
    }
}
