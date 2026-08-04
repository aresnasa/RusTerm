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
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::Local;
use zeroize::Zeroizing;

use rusterm_crypto::{decrypt_data, encrypt_data};

const MAGIC: &[u8; 4] = b"RUSL";
const VERSION: u8 = 1;

/// Commands sent from the hot-path caller to the background writer thread.
/// Keeping the data owned (`Vec<u8>`) lets the caller's `&[u8]` borrow be
/// released immediately — the thread does not share any lifetime with the
/// terminal output loop.
enum LogCommand {
    Entry {
        direction: &'static str,
        data: Vec<u8>,
    },
}

/// An encrypted session-log writer.
///
/// The expensive work — JSON serialization, base64 encoding, AES-256-GCM
/// encryption, and the synchronous disk write + `flush()` — runs on a
/// dedicated background OS thread. The terminal output loop (which handles
/// every PTY data chunk) only does a cheap `mpsc::send` of the raw bytes,
/// so logging never blocks rendering or user interaction.
///
/// Holds the per-session key in `Zeroizing` memory; plaintext never touches
/// disk. The key is moved into the background thread, which wipes it on exit.
pub struct SessionLog {
    /// Sender held by the caller; dropping it (or sending `Close`) signals
    /// the background thread to drain, flush, and exit.
    tx: Mutex<Option<Sender<LogCommand>>>,
    /// Background writer thread handle. Joined in `Drop` so pending entries
    /// are flushed before the `SessionLog` is torn down.
    handle: Mutex<Option<JoinHandle<()>>>,
    session_id: String,
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
    ///
    /// The file handle and the per-session key are moved into a dedicated
    /// background thread, so the caller never blocks on disk I/O or crypto.
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

        // Spawn the background writer thread. It owns the file + key so the
        // caller never blocks on disk I/O or AES-GCM. An unbounded channel
        // keeps `send` non-blocking; if the writer falls behind the entries
        // are buffered in memory (each is small). The thread wipes the key
        // via `Zeroizing`'s Drop when it exits.
        let (tx, rx): (Sender<LogCommand>, Receiver<LogCommand>) = mpsc::channel();
        let key = Zeroizing::new(key);
        let handle = thread::Builder::new()
            .name(format!(
                "session-log-{}",
                &session_id[..session_id.len().min(16)]
            ))
            .spawn(move || {
                background_writer(file, key, rx);
            })
            .context("spawning session-log background thread")?;

        Ok(Self {
            tx: Mutex::new(Some(tx)),
            handle: Mutex::new(Some(handle)),
            session_id: session_id.to_string(),
        })
    }

    /// Append a terminal-output chunk to the encrypted log.
    ///
    /// Non-blocking: sends the raw bytes to the background writer thread,
    /// which does the JSON + base64 + AES-GCM + disk write off the hot path.
    /// If the background thread has exited (e.g. after `close`), the send
    /// fails silently — logging is best-effort and must never break the
    /// terminal output loop.
    pub fn log_output(&self, data: &[u8]) {
        self.send(LogCommand::Entry {
            direction: "OUT",
            data: data.to_vec(),
        });
    }

    /// Append a terminal-input chunk to the encrypted log. See [`log_output`].
    pub fn log_input(&self, data: &[u8]) {
        self.send(LogCommand::Entry {
            direction: "IN",
            data: data.to_vec(),
        });
    }

    /// Best-effort enqueue; silently drops if the channel is closed.
    fn send(&self, cmd: LogCommand) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(cmd);
            }
        }
    }

    /// Close the log: signal the background thread to drain pending entries,
    /// flush the file, and exit. Subsequent `log_*` calls are no-ops.
    ///
    /// This blocks until the background thread has flushed all pending
    /// entries to disk. The drain is bounded by how much output was enqueued
    /// before `close` (the channel is unbounded but only holds what the
    /// session actually produced), so the wait is short in practice. `close`
    /// is called on session teardown (not per-chunk), so a brief blocking wait
    /// is acceptable and ensures no log entries are lost.
    pub fn close(&self) {
        // Take the sender out of the mutex so no further sends can succeed.
        let tx_taken = self.tx.lock().ok().and_then(|mut g| g.take());
        if let Some(tx) = tx_taken {
            // Dropping `tx` closes the channel, which lets the background
            // thread's `rx.recv()` loop return `None` after it drains.
            drop(tx);
        }
        // Join the thread so pending entries are flushed to disk before we
        // return. The writer thread exits promptly once the channel closes.
        let handle_taken = self.handle.lock().ok().and_then(|mut g| g.take());
        if let Some(handle) = handle_taken {
            let _ = handle.join();
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

/// Background writer loop. Owns the file handle + the per-session AEAD key,
/// so the JSON + base64 + AES-GCM + disk I/O all happen off the terminal
/// output loop. The key is held in `Zeroizing` so it is wiped when this
/// function returns (thread exit).
///
/// The loop terminates when the sender is dropped (channel returns `None`),
/// which happens on `close()` / `Drop`.
fn background_writer(mut file: fs::File, key: Zeroizing<[u8; 32]>, rx: Receiver<LogCommand>) {
    let write_entry = |direction: &str, data: &[u8], file: &mut fs::File| {
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

        let ciphertext = match encrypt_data(&key, &payload_bytes) {
            Ok(c) => c,
            Err(_) => return,
        };

        // Length-prefix the ciphertext so readers can iterate records.
        let len = u32::try_from(ciphertext.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        let len_bytes = len.to_be_bytes();

        // Best-effort write — failures here can't be surfaced to the user
        // meaningfully (we're on a background thread), so we silently drop.
        let _ = file.write_all(&len_bytes);
        let _ = file.write_all(&ciphertext);
        let _ = file.flush();
    };

    // Drain all entries until the sender is dropped (channel returns `None`).
    // The sender is dropped by `close()` / `Drop`, after which the loop exits,
    // the final flush runs, and the key is wiped.
    while let Ok(LogCommand::Entry { direction, data }) = rx.recv() {
        write_entry(direction, &data, &mut file);
    }
    // Final flush (file is dropped on function return, but flush explicitly
    // so any OS-buffered data is pushed to disk).
    let _ = file.flush();
    // `key` (Zeroizing) is wiped here when it goes out of scope.
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

    /// Round-trip test for the background-writer path: `log_output` /
    /// `log_input` enqueue on the hot path; the background thread does the
    /// JSON + base64 + AES-GCM + disk write. `close()` flushes and joins.
    /// `decrypt_file` must then read back exactly what was logged, proving
    /// the on-disk format is unchanged and the background thread committed
    /// all entries before `close` returned.
    #[test]
    fn background_writer_round_trips_through_decrypt_file() {
        // `dirs::data_dir()` is used internally, so we can't easily redirect
        // the file path. Instead, we create the SessionLog, log a few
        // entries, close it, then find the file it created and decrypt it.
        let key = test_key();
        // Unique per run: PID + a monotonically increasing counter avoids
        // collisions with stale files from previous test runs.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("rusterm_test_bg_{}_{}", std::process::id(), nonce);

        // Clean up any stale files with the test prefix so the search below
        // can only find the file this run creates.
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm")
            .join("session_logs");
        if let Ok(entries) = fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rusterm_test_bg_") && n.ends_with(".rusl"))
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        let log = SessionLog::new(&session_id, key).unwrap();

        log.log_output(b"hello background");
        log.log_input(b"typed this");
        log.log_output(b"second output chunk");
        log.close();

        // The file lives under <data_dir>/rusterm/session_logs/. Find it by
        // session_id prefix + .rusl extension.
        let candidate = fs::read_dir(&log_dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&session_id) && n.ends_with(".rusl"))
            })
            .max();
        let path = candidate.expect("session log file should exist after close()");

        let records = SessionLog::decrypt_file(&path, &key).unwrap();
        assert_eq!(records.len(), 3, "all 3 entries must be flushed by close()");
        assert_eq!(records[0].1, "OUT");
        assert_eq!(records[0].2, b"hello background");
        assert_eq!(records[1].1, "IN");
        assert_eq!(records[1].2, b"typed this");
        assert_eq!(records[2].1, "OUT");
        assert_eq!(records[2].2, b"second output chunk");

        // Clean up the test file.
        let _ = fs::remove_file(&path);
    }

    /// `log_output` after `close()` must be a silent no-op (the channel is
    /// closed). It must NOT panic or block — the terminal output loop
    /// depends on this.
    #[test]
    fn log_after_close_is_silent_noop() {
        let key = test_key();
        let session_id = format!("rusterm_test_noop_{}", std::process::id());
        let log = SessionLog::new(&session_id, key).unwrap();
        log.close();
        // These must not panic / block.
        log.log_output(b"after close");
        log.log_input(b"after close");
    }
}
