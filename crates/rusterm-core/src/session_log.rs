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
    /// Path to the encrypted `.rusl` file on disk. Stored so the UI can
    /// locate the file later for "copy full session" — without this, the
    /// path is lost because `new()` builds it internally and only hands the
    /// file handle to the background thread. The path is computed from the
    /// session id + a timestamp at creation time, so it is stable for the
    /// lifetime of the log.
    path: PathBuf,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("session_id", &self.session_id)
            .field("key", &"<redacted>")
            .field("path", &self.path.display().to_string())
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
            path,
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

    /// The on-disk path of the encrypted `.rusl` file. Exposed so the UI can
    /// locate the file for "copy full interactive session" — the path is
    /// otherwise unreachable because `new()` builds it internally and hands
    /// the file handle to the background thread.
    ///
    /// Note: the file is appended to for the lifetime of the `SessionLog`;
    /// the path is stable and never changes.
    pub fn path(&self) -> &std::path::Path {
        &self.path
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

/// Convert a decrypted session-log record stream into a single human-readable
/// text transcript suitable for clipboard copy. This is what powers the
/// "copy full interactive session" feature — it preserves the chronological
/// flow of prompts and user input that the rendered-terminal `render_all()`
/// path loses (password prompts, vim buffers, anything scrolled off-screen).
///
/// The transform applied to each record's raw PTY bytes:
///
/// 1. **CSI / escape sequences stripped**: `\x1b[...X` (CSI), `\x1b]...\x07`
///   (OSC), `\x1bP...\x1b\\` (DCS), single-char `\x1bX` controls. These are
///   rendering instructions; keeping them would produce gibberish.
/// 2. **Control characters normalized**: `\r` is dropped (carriage return
///   before `\n` is redundant on Unix PTYs), `\n` is kept as the line
///   separator, `\t` is preserved, other C0 controls below 0x20 are dropped.
/// 3. **Backspace (`\x08`) handling**: applies a destructive backspace on
///   the in-progress line buffer so the transcript reflects the final visible
///   text rather than every keystroke of a corrected word.
/// 4. **Direction markers**: by default `IN` records are prefixed with a faint
///   `[in] ` marker so the user can see what they typed (e.g. at a password
///   prompt) vs. what the host printed. `OUT` records are emitted verbatim
///   (they already contain their own newlines from the remote side). Pass
///   `DirectionMarker::None` to drop the markers for a clean transcript.
///
/// Bytes that are not valid UTF-8 are replaced with `U+FFFD` per the standard
/// `String::from_utf8_lossy` semantics.
///
/// # Security
///
/// This function does NOT attempt to mask password input. The raw `IN` bytes
/// are exactly what the user typed. If the caller is presenting this to the
/// user via the clipboard and the user typed a password at an interactive
/// prompt, that password will appear in the transcript. This matches user
/// expectation for a "copy what I did" feature, but callers must NOT log the
/// returned text or send it anywhere off-device.
pub fn records_to_transcript(records: &[(String, String, Vec<u8>)]) -> String {
    let mut out = String::with_capacity(records.len() * 64);
    for (_ts, direction, data) in records {
        let cleaned = strip_pty_control(data);
        if cleaned.is_empty() {
            continue;
        }
        if direction == "IN" {
            // User input. Prefix with a marker so the reader can tell prompt
            // output from typed input. Keep the trailing newline if any —
            // shells echo the user's keystrokes back to OUT, so an `IN` entry
            // is typically just the typed bytes without a newline (the
            // terminal driver adds `\r` on Enter, which we strip).
            out.push_str("[in] ");
            out.push_str(&cleaned);
            // If the user's input did not end with a newline, the following
            // OUT record will continue on the same line — which is exactly
            // what the user saw (the prompt output appeared after their
            // input on the same line). Don't force a newline here.
            if !cleaned.ends_with('\n') {
                out.push('\n');
            }
        } else {
            // Terminal output — emit verbatim. The remote side controls its
            // own newlines.
            out.push_str(&cleaned);
        }
    }
    out
}

/// Strip ANSI/VT100 escape sequences and normalize control characters from a
/// raw PTY byte stream, returning a readable `String`.
///
/// Handles:
/// - CSI sequences: `ESC [ ... <final byte 0x40-0x7E>`
/// - OSC sequences: `ESC ] ... (BEL | ESC \\)`
/// - DCS sequences: `ESC P ... ESC \\`
/// - Other escape sequences: `ESC <single char>`
/// - Backspace (`\b`, 0x08): destructive on the in-progress line
/// - Carriage return (`\r`, 0x0D): dropped (redundant before `\n` on Unix)
/// - Other C0 controls (< 0x20): dropped, except `\n` (0x0A) and `\t` (0x09)
///
/// Bytes that don't form valid UTF-8 are handled lossily.
pub fn strip_pty_control(data: &[u8]) -> String {
    // We process byte-by-byte for the control-sequence state machine, then
    // collect the surviving bytes into a UTF-8 string at the end. This avoids
    // needing to track UTF-8 continuation boundaries inside escape sequences
    // (which can't appear there anyway).
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == 0x1b {
            // Escape. Determine the kind of sequence and skip it.
            i += 1;
            if i >= data.len() {
                break;
            }
            match data[i] {
                // CSI: ESC [ ... <0x40-0x7E>
                b'[' => {
                    i += 1;
                    let mut final_byte = 0u8;
                    while i < data.len() {
                        let c = data[i];
                        i += 1;
                        // Final byte is in range 0x40..=0x7E (params/intermediates are 0x20..=0x3F).
                        if (0x40..=0x7E).contains(&c) {
                            final_byte = c;
                            break;
                        }
                    }
                    // Full-screen TUIs (koko/jumpserver menus, htop, vim,
                    // ...) redraw rows via absolute/relative cursor motion
                    // plus screen clears, without emitting newlines. If we
                    // silently drop those sequences, every positioned row
                    // gets concatenated onto its neighbors and the menu
                    // turns into one long unreadable blob. Treat vertical
                    // cursor motion and display clears as line boundaries so
                    // each redrawn row lands on its own transcript line.
                    //
                    //   H/f : CUP/HVP  — absolute row;col positioning
                    //   A/B : CUU/CUD  — up/down relative motion
                    //   E/F : CNL/CPL  — next/previous line
                    //   J   : ED       — erase display (frame boundary)
                    //
                    // Horizontal-only motion (C/D/G/`) and line-local erase
                    // (K) keep the current line intact. Collapse runs of
                    // boundaries to a single '\n' (e.g. the common
                    // `ESC[2J` `ESC[H` clear+home combo) and never emit a
                    // leading newline for a clear/home at the very start.
                    if matches!(final_byte, b'H' | b'f' | b'A' | b'B' | b'E' | b'F' | b'J')
                        && out.last().is_some_and(|&c| c != b'\n')
                    {
                        out.push(b'\n');
                    }
                }
                // OSC: ESC ] ... (BEL \x07 | ST = ESC \\)
                b']' => {
                    i += 1;
                    while i < data.len() {
                        if data[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                // DCS: ESC P ... ST (ESC \\)
                b'P' => {
                    i += 1;
                    while i < data.len() {
                        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                // All other ESC sequences: ESC <single char>. This covers
                // ESC = / ESC > (keypad mode), ESC c (reset), ESC 7/8 (save
                // cursor), ESC D/M/E (index/RI/NEL), etc.
                _ => {
                    i += 1;
                }
            }
        } else if b == 0x08 {
            // Backspace: destructive. Pop the last byte from the output if it
            // isn't a newline (newlines are structural — don't eat them).
            if out.last().is_some_and(|&c| c != b'\n') {
                out.pop();
            }
            i += 1;
        } else if b == 0x0d {
            // Carriage return: drop. On Unix PTYs, `\r\n` is the usual
            // sequence and the `\r` is redundant. A lone `\r` (cursor to
            // column 0 without newline) is rare in interactive sessions and
            // would only matter for progress spinners, which we don't want
            // in a transcript anyway.
            i += 1;
        } else if b == 0x0a || b == 0x09 || b >= 0x20 {
            // Newline, tab, or printable byte — keep.
            out.push(b);
            i += 1;
        } else {
            // Other C0 control character (< 0x20, not `\n`/`\t`/`\b`/`\r`):
            // drop. This covers `\x00` (NUL), `\x07` (BEL), `\x0b` (VT),
            // `\x0c` (FF), `\x0e`/`\x0f` (SO/SI), `\x1c`-`\x1f` (FS/GS/RS/US).
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    // ── transcript / strip_pty_control tests ──────────────────────────────

    #[test]
    fn strip_pty_control_strips_csi_sequences() {
        // SGR "set color to red"
        let input = b"\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_pty_control(input), "hello world");
    }

    #[test]
    fn strip_pty_control_strips_csi_cursor_moves() {
        // CUU (cursor up 1) + CUF (cursor forward 3)
        let input = b"line1\n\x1b[1A\x1b[3Coverwrite";
        assert_eq!(strip_pty_control(input), "line1\noverwrite");
    }

    #[test]
    fn strip_pty_control_strips_osc_sequences_bel_terminated() {
        // OSC 0 ; title BEL
        let input = b"\x1b]0;my window title\x07prompt$ ";
        assert_eq!(strip_pty_control(input), "prompt$ ");
    }

    #[test]
    fn strip_pty_control_strips_osc_sequences_st_terminated() {
        // OSC 2 ; title ST (ESC \\)
        let input = b"\x1b]2;my title\x1b\\prompt$ ";
        assert_eq!(strip_pty_control(input), "prompt$ ");
    }

    #[test]
    fn strip_pty_control_strips_dcs_sequences() {
        // DCS sixel-ish payload ST-terminated
        let input = b"\x1bPq...binary...\x1b\\done";
        assert_eq!(strip_pty_control(input), "done");
    }

    #[test]
    fn strip_pty_control_strips_single_char_escapes() {
        // ESC = (keypad application mode), ESC > (keypad numeric mode),
        // ESC c (reset), ESC 7 (save cursor), ESC 8 (restore cursor)
        let input = b"\x1b=\x1b>\x1bcstart\x1b7mid\x1b8end";
        assert_eq!(strip_pty_control(input), "startmidend");
    }

    #[test]
    fn strip_pty_control_handles_backspace_destructively() {
        // User typed "helo" then BS twice then "llo" → "hello"
        let input = b"helo\x08\x08llo";
        assert_eq!(strip_pty_control(input), "hello");
    }

    #[test]
    fn strip_pty_control_backspace_does_not_eat_newline() {
        // BS at a line boundary must not consume the newline — it's structural.
        let input = b"done\n\x08more";
        assert_eq!(strip_pty_control(input), "done\nmore");
    }

    #[test]
    fn strip_pty_control_drops_carriage_return() {
        // The classic `\r\n` line ending on Unix PTYs
        let input = b"line1\r\nline2\r\n";
        assert_eq!(strip_pty_control(input), "line1\nline2\n");
    }

    #[test]
    fn strip_pty_control_drops_other_c0_controls() {
        // NUL, BEL, VT, FF, SO, SI between printable letters. The control
        // bytes must all be dropped, the printable letters preserved.
        let input = b"\x00a\x07b\x0bc\x0cd\x0ee\x0ff";
        assert_eq!(strip_pty_control(input), "abcdef");
    }

    #[test]
    fn strip_pty_control_preserves_tab_and_newline() {
        let input = b"\tcol1\tcol2\n";
        assert_eq!(strip_pty_control(input), "\tcol1\tcol2\n");
    }

    #[test]
    fn strip_pty_control_preserves_utf8() {
        let input = "héllo wörld 中文 🦀".as_bytes();
        assert_eq!(strip_pty_control(input), "héllo wörld 中文 🦀");
    }

    #[test]
    fn strip_pty_control_replaces_invalid_utf8() {
        // 0xff is not valid UTF-8 in any position
        let input = &[b'a', 0xff, b'b'];
        let out = strip_pty_control(input);
        assert_eq!(out, "a\u{FFFD}b");
    }

    #[test]
    fn strip_pty_control_positioned_rows_each_on_own_line() {
        // koko/jumpserver-style menu frame: rows painted via absolute cursor
        // positioning (CUP) with no \r\n between them. Each row must land on
        // its own transcript line instead of being concatenated into a blob.
        let input = b"\x1b[1;36m  assets \x1b[0m\
                      \x1b[3;1H26: k8s-node-a\
                      \x1b[4;1H27: storage-b\
                      \x1b[5;1H28: jump-c";
        let out = strip_pty_control(input);
        assert_eq!(out, "  assets \n26: k8s-node-a\n27: storage-b\n28: jump-c");
    }

    #[test]
    fn strip_pty_control_clear_and_home_collapse_to_one_boundary() {
        // The classic full-redraw prefix `ESC[2J` `ESC[H` must produce a
        // single line boundary, not two. Also: a clear at the very start of
        // the buffer must not produce a leading empty line.
        let input = b"\x1b[2J\x1b[Hmenu row";
        assert_eq!(strip_pty_control(input), "menu row");
        // Between content chunks, the pair collapses to exactly one \n.
        let input = b"prev\x1b[2J\x1b[Hnext";
        assert_eq!(strip_pty_control(input), "prev\nnext");
    }

    #[test]
    fn strip_pty_control_color_and_inline_sequences_add_no_boundaries() {
        // SGR colors (final 'm'), horizontal cursor moves (C/D), erase-in-
        // line (K) and mode sets (h/l) must NOT introduce line boundaries.
        let input = b"\x1b[1;31mred \x1b[0mplain\x1b[2Cgap\x1b[Kend\x1b[?25l";
        assert_eq!(strip_pty_control(input), "red plaingapend");
    }

    #[test]
    fn records_to_transcript_interactive_tui_login_flow_is_readable() {
        // Synthetic jumpserver session: ssh banner + login prompt, then the
        // koko asset menu (TUI frame via cursor positioning), then the user
        // types a selection, koko prints an error row and redraws. Every
        // menu row and interaction must appear on its own line, in order.
        let records = vec![
            (
                "t1".into(),
                "OUT".into(),
                b"Welcome to jumpserver\r\nlogin: ".to_vec(),
            ),
            ("t2".into(), "IN".into(), b"ops\r".to_vec()),
            (
                "t3".into(),
                "OUT".into(),
                b"ops\r\n\x1b[2J\x1b[H\x1b[1;36m  \xe8\xb5\x84\xe4\xba\xa7\xe5\x88\x86\xe7\xb1\xbb\xe5\x88\x97\xe8\xa1\xa8\x1b[0m\
                  \x1b[3;1H26: engine-k8s-node\
                  \x1b[4;1H27: storage-node"
                    .to_vec(),
            ),
            ("t4".into(), "IN".into(), b"33\r".to_vec()),
            (
                "t5".into(),
                "OUT".into(),
                b"\x1b[2J\x1b[H\x1b[3;1Hinvalid input, choose asset class\
                  \x1b[4;1H26: engine-k8s-node"
                    .to_vec(),
            ),
        ];
        let transcript = records_to_transcript(&records);
        let lines: Vec<&str> = transcript.lines().collect();
        // Login phase in order.
        assert_eq!(lines[0], "Welcome to jumpserver");
        assert_eq!(lines[1], "login: [in] ops");
        assert_eq!(lines[2], "ops");
        // Menu rows each on their own line.
        assert!(transcript.contains("  资产分类列表"));
        assert!(transcript.contains("\n26: engine-k8s-node"));
        assert!(transcript.contains("\n27: storage-node"));
        // The user's selection is visible as input.
        assert!(transcript.contains("[in] 33"));
        // The follow-up frame is separated, not pasted onto the old rows.
        assert!(transcript.contains("\ninvalid input, choose asset class"));
    }

    #[test]
    fn records_to_transcript_emits_in_and_out_in_order() {
        let records = vec![
            ("t1".into(), "OUT".into(), b"login: ".to_vec()),
            ("t2".into(), "IN".into(), b"root\n".to_vec()),
            ("t3".into(), "OUT".into(), b"Password: ".to_vec()),
            ("t4".into(), "IN".into(), b"secret".to_vec()),
            ("t5".into(), "OUT".into(), b"\r\n$ ".to_vec()),
            ("t6".into(), "IN".into(), b"ls\n".to_vec()),
            ("t7".into(), "OUT".into(), b"file1 file2\r\n$ ".to_vec()),
        ];
        let transcript = records_to_transcript(&records);
        // Note the `\n\n` after `secret`: the IN record added a synthetic
        // newline (input had none), then the OUT record opened with `\r\n`
        // (the remote echoing back the Enter keystroke) whose `\r` is
        // stripped, leaving a second `\n`. This is the authentic PTY flow —
        // the user typed `secret<Enter>` and the host echoed the Enter.
        assert_eq!(
            transcript,
            "login: [in] root\nPassword: [in] secret\n\n$ [in] ls\nfile1 file2\n$ "
        );
    }

    #[test]
    fn records_to_transcript_strips_ansi_from_output() {
        // A colored prompt with SGR codes
        let records = vec![
            (
                "t1".into(),
                "OUT".into(),
                b"\x1b[32mroot@host\x1b[0m:~$ \x1b[36m".to_vec(),
            ),
            ("t2".into(), "IN".into(), b"ls\n".to_vec()),
            ("t3".into(), "OUT".into(), b"\x1b[0mfile1\n".to_vec()),
        ];
        let transcript = records_to_transcript(&records);
        assert_eq!(transcript, "root@host:~$ [in] ls\nfile1\n");
    }

    #[test]
    fn records_to_transcript_skips_empty_records() {
        let records = vec![
            ("t1".into(), "OUT".into(), b"".to_vec()),
            ("t2".into(), "OUT".into(), b"\x1b[2J\x1b[H".to_vec()), // clear screen
            ("t3".into(), "OUT".into(), b"hello\n".to_vec()),
        ];
        let transcript = records_to_transcript(&records);
        assert_eq!(transcript, "hello\n");
    }

    /// The full "copy interactive session" path: write records via the
    /// SessionLog background writer, close (flush), decrypt, and convert to
    /// a transcript. Proves the entire pipeline works end-to-end.
    #[test]
    fn full_session_log_to_transcript_round_trip() {
        let key = test_key();
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("rusterm_test_transcript_{}_{}", std::process::id(), nonce);

        // Clean up stale files with this prefix
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm")
            .join("session_logs");
        if let Ok(entries) = fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("rusterm_test_transcript_") && n.ends_with(".rusl")
                }) {
                    let _ = fs::remove_file(&p);
                }
            }
        }

        let log = SessionLog::new(&session_id, key).unwrap();
        // Simulate an interactive login: prompt, user input, output, command, output.
        log.log_output(b"\x1b[?2004hlogin: ");
        log.log_input(b"admin\n");
        log.log_output(b"\r\nPassword: ");
        log.log_input(b"hunter2");
        log.log_output(b"\r\n$ ");
        log.log_input(b"ls -la\n");
        log.log_output(b"\x1b[0mtotal 0\r\ndrwxr-xr-x 2 admin admin 64 Jan 1 00:00 .\r\n$ ");
        // The path() accessor is what powers the UI's ability to find the file.
        let path = log.path().to_path_buf();
        log.close();

        let records = SessionLog::decrypt_file(&path, &key).expect("decrypt should succeed");
        assert_eq!(records.len(), 7, "all 7 records must be present");

        let transcript = records_to_transcript(&records);
        // Verify the interactive flow is preserved: prompt → input → prompt → input → output.
        assert!(
            transcript.contains("login: [in] admin"),
            "login prompt + input must appear"
        );
        assert!(
            transcript.contains("Password: [in] hunter2"),
            "password prompt + input must appear"
        );
        assert!(transcript.contains("total 0"), "ls output must appear");
        assert!(
            transcript.contains("drwxr-xr-x"),
            "directory listing must appear"
        );
        // The SGR escape sequences must be stripped.
        assert!(
            !transcript.contains('\x1b'),
            "no escape characters should survive"
        );
        // Bracketed paste mode sequence (ESC [ ? 2004 h) must be stripped.
        assert!(
            !transcript.contains("2004"),
            "bracketed paste mode CSI must be stripped"
        );

        let _ = fs::remove_file(&path);
    }

    /// `path()` must return the on-disk location of the `.rusl` file so the
    /// UI can find it for decryption without having to reconstruct the path
    /// from session_id + timestamp (which would race the background writer).
    #[test]
    fn session_log_path_accessor_returns_actual_file() {
        let key = test_key();
        let session_id = format!("rusterm_test_path_{}", std::process::id());
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm")
            .join("session_logs");
        // Clean any stale file
        if let Ok(entries) = fs::read_dir(&log_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rusterm_test_path_") && n.ends_with(".rusl"))
                {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        let log = SessionLog::new(&session_id, key).unwrap();
        let path = log.path().to_path_buf();
        log.close();
        assert!(path.exists(), "path() must point at a real file");
        assert!(
            path.to_string_lossy().ends_with(".rusl"),
            "must be a .rusl file"
        );
        assert!(
            path.to_string_lossy().contains(&session_id),
            "filename must contain the session_id"
        );
        let _ = fs::remove_file(&path);
    }
}
