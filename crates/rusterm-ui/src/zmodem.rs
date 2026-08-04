//! ZMODEM (lrzsz rz/sz) integration for RusTerm terminal sessions.
//!
//! This module bridges the pure-protocol [`rusterm_zmodem`] crate to the
//! live PTY/SSH event loop. Each terminal session may have an active
//! [`ZmodemSession`] installed in [`ZmodemSessions`]; when present, the
//! `SessionEvent::Output` handler routes output bytes through
//! [`process_output`] before they reach the terminal renderer. ZMODEM
//! protocol frames are consumed by the session and translated into file
//! transfers (rfd save/open dialogs + on-disk writes), while non-ZMODEM
//! bytes are returned to the caller for normal rendering.
//!
//! ## Roles
//!
//!  * **Receive** (`sz` on the remote): the remote sends a file. We open a
//!    save dialog, write incoming data blocks to the chosen path, and ACK.
//!  * **Send** (`rz` on the remote): the remote is ready to receive. We open
//!    a file-open dialog, read the file, and stream ZDATA blocks.
//!
//! ## Threading
//!
//! `process_output` runs on the synchronous event loop and only updates the
//! in-memory session state + drains `to_pty` bytes (which the caller injects
//! via `input_senders`). File I/O and the rfd dialogs happen in `spawn`ed
//! tasks that report back through the [`ZmodemSessions`] handle, so the
//! event loop is never blocked on disk or a native dialog.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use rusterm_zmodem::{Direction, SessionEvent, ZmodemSession};

/// Per-session ZMODEM state holder. Installed lazily when the first ZMODEM
/// frame is detected and removed when the session finishes (Done/Cancelled).
#[derive(Default, Debug)]
pub struct ZmodemSessions {
    sessions: HashMap<String, ZmodemSession>,
    /// Open file writers for receive-mode sessions, keyed by session id.
    /// Written to from `spawn`ed tasks (after the rfd dialog resolves).
    writers: HashMap<String, Arc<Mutex<Option<std::fs::File>>>>,
    /// Pending send payloads (file read by the user) awaiting install.
    pending_send: HashMap<String, Vec<u8>>,
}

impl ZmodemSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if `session_id` has an active (non-finished) ZMODEM session.
    pub fn is_active(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|s| s.is_active() && !s.is_finished())
    }

    /// Remove a session's ZMODEM state (e.g. on disconnect).
    pub fn remove(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.writers.remove(session_id);
        self.pending_send.remove(session_id);
    }

    /// Install a chosen save path for a receive-mode session. Called by the
    /// rfd save-dialog task once the user confirms.
    pub fn install_receive_path(&mut self, session_id: &str, path: PathBuf) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.set_save_path(path.clone());
        }
        // Open the writer file handle for the event loop to drain into.
        match std::fs::File::create(&path) {
            Ok(file) => {
                self.writers
                    .insert(session_id.to_string(), Arc::new(Mutex::new(Some(file))));
            }
            Err(err) => {
                tracing::warn!("[ZMODEM] failed to create receive file {:?}: {err}", path);
            }
        }
    }

    /// Install a chosen send payload for a send-mode session. Called by the
    /// rfd open-dialog task once the user picks a file.
    pub fn install_send_payload(&mut self, session_id: &str, name: String, payload: Vec<u8>) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.begin_send(name, payload);
        }
    }

    /// Get-or-insert a ZMODEM session for `session_id`.
    fn session_for(&mut self, session_id: &str) -> &mut ZmodemSession {
        self.sessions.entry(session_id.to_string()).or_default()
    }

    /// Drain any `to_pty` bytes the session has produced (ZMODEM responses
    /// that must be written back to the remote).
    pub fn take_pty_output(&mut self, session_id: &str) -> Vec<u8> {
        self.sessions
            .get_mut(session_id)
            .map(|s| s.take_pty_output())
            .unwrap_or_default()
    }

    /// Drain UI events for a session.
    pub fn take_events(&mut self, session_id: &str) -> Vec<SessionEvent> {
        self.sessions
            .get_mut(session_id)
            .map(|s| s.take_events())
            .unwrap_or_default()
    }
}

/// Outcome of [`process_output`]: the bytes to render, plus any PTY response
/// bytes the caller must write back to the remote (ZMODEM acks/headers).
#[derive(Debug, Default)]
pub struct ProcessedOutput {
    /// Bytes that should be rendered to the terminal (non-ZMODEM passthrough).
    pub passthrough: Vec<u8>,
    /// Bytes to write back to the PTY (ZMODEM protocol responses).
    pub to_pty: Vec<u8>,
    /// True if a ZMODEM session just became active (first frame detected) —
    /// lets the caller suppress terminal rendering bookkeeping.
    pub zmodem_active: bool,
    /// True if the ZMODEM session just finished (Done/Cancelled) and should
    /// be cleaned up.
    pub zmodem_finished: bool,
    /// UI events the caller should dispatch (file dialogs, progress, etc.).
    pub events: Vec<SessionEvent>,
}

/// Route a chunk of session output through the ZMODEM detector.
///
/// If no ZMODEM session is active and the bytes don't begin a ZMODEM frame,
/// this returns the bytes unchanged in `passthrough` (a no-op). Once a frame
/// is detected, subsequent bytes are fed to the session and only non-protocol
/// bytes are returned as passthrough.
pub fn process_output(
    state: &mut ZmodemSessions,
    session_id: &str,
    data: &[u8],
) -> ProcessedOutput {
    let mut out = ProcessedOutput::default();
    let session = state.session_for(session_id);
    let passthrough = session.feed(data);
    let to_pty = session.take_pty_output();
    out.zmodem_active = session.is_active();
    out.zmodem_finished = session.is_finished();
    out.events = session.take_events();
    out.passthrough = passthrough;
    out.to_pty = to_pty;
    out
}

/// Spawn the UI-side handling for a single ZMODEM session event.
///
/// Called by the event loop for each event returned by [`process_output`].
/// File I/O and the rfd dialogs happen in `spawn`ed tasks that write back
/// to `state_handle`. The `input_sender` injects any `to_pty` bytes the
/// session produces after a dialog resolves (e.g. ZFILE after the user
/// picks a file to send).
///
/// Returns `true` if the ZMODEM session has finished (Done/Cancelled) and
/// the caller should stop feeding further output to it.
pub fn dispatch_event(
    session_id: &str,
    event: SessionEvent,
    state_handle: Arc<Mutex<ZmodemSessions>>,
    input_sender: mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    match event {
        SessionEvent::ReceiveOffered => {
            tracing::info!(
                "[ZMODEM] receive offered for {}",
                &session_id[..session_id.len().min(8)]
            );
        }
        SessionEvent::SendOffered { .. } => {
            tracing::info!(
                "[ZMODEM] send offered for {}",
                &session_id[..session_id.len().min(8)]
            );
            spawn_send_file_picker(session_id.to_string(), state_handle, input_sender);
        }
        SessionEvent::FileOffer { name, size, .. } => {
            tracing::info!(
                "[ZMODEM] file offer: name={:?} size={:?} for {}",
                name,
                size,
                &session_id[..session_id.len().min(8)]
            );
            spawn_save_dialog(session_id.to_string(), name, state_handle, input_sender);
        }
        SessionEvent::DataReceived {
            offset,
            len,
            total,
            data,
        } => {
            tracing::debug!(
                "[ZMODEM] data received offset={} len={} total={:?} for {}",
                offset,
                len,
                total,
                &session_id[..session_id.len().min(8)]
            );
            // Write the verified data block to the receive file. The writer
            // was installed by install_receive_path after the save dialog.
            let writer_arc = {
                let s = state_handle.lock();
                s.writers.get(session_id).cloned()
            };
            if let Some(writer_arc) = writer_arc {
                let mut guard = writer_arc.lock();
                if let Some(file) = guard.as_mut() {
                    use std::io::Write;
                    if let Err(err) = file.write_all(&data) {
                        tracing::warn!(
                            "[ZMODEM] failed to write {} bytes to receive file: {err}",
                            data.len()
                        );
                    }
                }
            }
        }
        SessionEvent::Done { direction, bytes } => {
            tracing::info!(
                "[ZMODEM] {} done: {} bytes for {}",
                match direction {
                    Direction::Receive => "receive",
                    Direction::Send => "send",
                },
                bytes,
                &session_id[..session_id.len().min(8)]
            );
            let mut s = state_handle.lock();
            if let Some(writer_arc) = s.writers.remove(session_id) {
                let mut guard = writer_arc.lock();
                if let Some(mut file) = guard.take() {
                    let _ = file.flush();
                }
            }
            s.remove(session_id);
            return true;
        }
        SessionEvent::Cancelled => {
            tracing::warn!(
                "[ZMODEM] cancelled for {}",
                &session_id[..session_id.len().min(8)]
            );
            let mut s = state_handle.lock();
            s.remove(session_id);
            return true;
        }
        SessionEvent::Skipped => {
            tracing::info!(
                "[ZMODEM] file skipped for {}",
                &session_id[..session_id.len().min(8)]
            );
        }
    }
    false
}

fn spawn_send_file_picker(
    session_id: String,
    state_handle: Arc<Mutex<ZmodemSessions>>,
    input_sender: mpsc::UnboundedSender<Vec<u8>>,
) {
    tokio::spawn(async move {
        let file = rfd::AsyncFileDialog::new()
            .set_title("选择要上传的文件 / Select file to send")
            .pick_file()
            .await;
        if let Some(file) = file {
            let path = file.path().to_path_buf();
            match tokio::fs::read(&path).await {
                Ok(payload) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "upload.bin".to_string());
                    {
                        let mut s = state_handle.lock();
                        s.install_send_payload(&session_id, name, payload);
                        if let Some(bytes) =
                            s.sessions.get_mut(&session_id).map(|s| s.take_pty_output())
                            && !bytes.is_empty()
                        {
                            let _ = input_sender.send(bytes);
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!("[ZMODEM] failed to read {:?}: {err}", path);
                }
            }
        } else {
            tracing::info!("[ZMODEM] send cancelled by user");
        }
    });
}

fn spawn_save_dialog(
    session_id: String,
    default_name: String,
    state_handle: Arc<Mutex<ZmodemSessions>>,
    input_sender: mpsc::UnboundedSender<Vec<u8>>,
) {
    tracing::info!(
        "[ZMODEM] spawning save dialog for {} (name={:?})",
        &session_id[..session_id.len().min(8)],
        default_name
    );
    tokio::spawn(async move {
        let mut dialog =
            rfd::AsyncFileDialog::new().set_title("保存接收的文件 / Save received file");
        if !default_name.is_empty() {
            dialog = dialog.set_file_name(&default_name);
        }
        let file = dialog.save_file().await;
        tracing::info!(
            "[ZMODEM] save dialog result for {}: {}",
            &session_id[..session_id.len().min(8)],
            if file.is_some() {
                "picked"
            } else {
                "cancelled/failed"
            }
        );
        if let Some(file) = file {
            let path = file.path().to_path_buf();
            tracing::info!(
                "[ZMODEM] save path chosen for {}: {:?}",
                &session_id[..session_id.len().min(8)],
                path
            );
            {
                let mut s = state_handle.lock();
                s.install_receive_path(&session_id, path);
                if let Some(bytes) = s.sessions.get_mut(&session_id).map(|s| s.take_pty_output())
                    && !bytes.is_empty()
                {
                    tracing::info!(
                        "[ZMODEM] sending ZRPOS ({} bytes) to PTY for {}",
                        bytes.len(),
                        &session_id[..session_id.len().min(8)]
                    );
                    let _ = input_sender.send(bytes);
                }
            }
        } else {
            tracing::info!("[ZMODEM] receive cancelled by user");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusterm_zmodem::frame::{CrcMode, FrameType, HeaderFrame, encode_hex_header};

    fn zrqinit_frame() -> Vec<u8> {
        encode_hex_header(&HeaderFrame {
            frame_type: FrameType::ZRQInit,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        })
    }

    #[test]
    fn non_zmodem_bytes_pass_through_unchanged() {
        let mut s = ZmodemSessions::new();
        let out = process_output(&mut s, "s1", b"hello\n");
        assert_eq!(out.passthrough, b"hello\n");
        assert!(out.to_pty.is_empty());
        assert!(!out.zmodem_active);
        assert!(out.events.is_empty());
    }

    #[test]
    fn zrqinit_activates_session_and_produces_zrinit_response() {
        let mut s = ZmodemSessions::new();
        let out = process_output(&mut s, "s1", &zrqinit_frame());
        // Hex frame consumed → no passthrough.
        assert_eq!(out.passthrough, b"");
        assert!(out.zmodem_active);
        // The session emits a ZRINIT back to the PTY.
        assert!(!out.to_pty.is_empty());
        // And a ReceiveOffered event.
        assert!(
            out.events
                .iter()
                .any(|e| matches!(e, SessionEvent::ReceiveOffered))
        );
        assert!(s.is_active("s1"));
    }

    #[test]
    fn bytes_before_frame_pass_through_then_frame_consumed() {
        let mut s = ZmodemSessions::new();
        let mut input = b"prompt$ ".to_vec();
        input.extend_from_slice(&zrqinit_frame());
        let out = process_output(&mut s, "s1", &input);
        assert_eq!(&out.passthrough, b"prompt$ ");
        assert!(out.zmodem_active);
    }

    #[test]
    fn remove_clears_all_session_state() {
        let mut s = ZmodemSessions::new();
        process_output(&mut s, "s1", &zrqinit_frame());
        assert!(s.is_active("s1"));
        s.remove("s1");
        assert!(!s.is_active("s1"));
    }

    #[test]
    fn take_pty_output_drains_after_processing() {
        let mut s = ZmodemSessions::new();
        process_output(&mut s, "s1", &zrqinit_frame());
        // After process_output drained to_pty, a second take returns empty.
        assert!(s.take_pty_output("s1").is_empty());
    }

    #[test]
    fn install_receive_path_creates_writer_and_opens_file() {
        let mut s = ZmodemSessions::new();
        process_output(&mut s, "s1", &zrqinit_frame());
        // Use a unique filename directly in the system temp dir (which
        // always exists) — no subdirectory creation needed.
        let path = std::env::temp_dir().join(format!(
            "rusterm-zmodem-recv-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        s.install_receive_path("s1", path.clone());
        // A writer handle should now be installed.
        assert!(s.writers.contains_key("s1"));
        // The file should exist on disk.
        assert!(path.exists());
        // Cleanup.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn install_send_payload_emits_zfile_header_to_pty() {
        let mut s = ZmodemSessions::new();
        // Seed a send-mode session by feeding a ZRINIT (what `rz` emits).
        let zrinit = encode_hex_header(&HeaderFrame {
            frame_type: FrameType::ZRInit,
            data: [0x00, 0x00, 0x21, 0x00],
            crc_mode: CrcMode::Crc16,
        });
        process_output(&mut s, "s1", &zrinit);
        // Simulate the user picking a file.
        s.install_send_payload("s1", "upload.txt".to_string(), b"payload".to_vec());
        // The session should have produced ZFILE header bytes to send.
        let pty = s.take_pty_output("s1");
        assert!(!pty.is_empty());
        // ZFILE hex header starts with ZPAD ZPAD ZDLE ZHEX ('B').
        assert!(pty.windows(4).any(|w| w == [b'*', b'*', 0x18, b'B']));
    }
}
