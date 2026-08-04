//! High-level ZMODEM session state machine.
//!
//! The [`ZmodemSession`] drives a complete transfer (receive or send) on top
//! of the low-level [`crate::Detector`]. It is fed PTY output bytes and emits
//! [`SessionEvent`]s the UI layer can act on (file offer, data chunk, progress,
//! completion). Bytes that must be written back to the PTY (ZMODEM
//! acknowledgements, file headers, data blocks) are returned for the caller to
//! inject via the session's input channel.

use std::path::PathBuf;

use crate::frame::{CrcMode, DataEnd, FrameType, HeaderFrame, ZmodemFrame};
use crate::parser::{Detection, Detector};

/// Which side of the transfer this represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Remote is sending (`sz`); we are the receiver and write to a local file.
    Receive,
    /// Remote is receiving (`rz`); we are the sender and read from a local file.
    Send,
}

/// Events emitted by [`ZmodemSession`] for the UI layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// A ZRQINIT was seen — the remote (`sz`) wants to send us a file. The UI
    /// should prepare a save-location (the file name arrives in the
    /// subsequent `FileOffer`).
    ReceiveOffered,
    /// A ZRINIT was seen — the remote (`rz`) is ready to receive. The UI
    /// should prompt the user to pick a file to upload.
    SendOffered {
        /// Capability flags from the peer's ZRINIT (max block size, etc.).
        flags: [u8; 4],
    },
    /// A ZFILE header + filename/metadata arrived (receive path). The UI
    /// should confirm/adjust the save path.
    FileOffer {
        name: String,
        size: Option<u64>,
        mtime: Option<u64>,
        mode: Option<u32>,
    },
    /// A chunk of file data was received and CRC-verified. The `data`
    /// payload is owned so the UI layer can write it to disk.
    DataReceived {
        offset: u32,
        len: usize,
        total: Option<u64>,
        data: Vec<u8>,
    },
    /// The transfer completed (`ZEOF` received and acknowledged).
    Done { direction: Direction, bytes: u64 },
    /// The peer cancelled or a fatal error occurred.
    Cancelled,
    /// A non-fatal skip (ZSKIP).
    Skipped,
}

/// Phase of a session's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the first frame (ZRQINIT for receive, ZRINIT for send).
    Init,
    /// Receive: waiting for ZFILE after sending ZRINIT.
    AwaitFile,
    /// Receive: ZFILE received, FileOffer emitted, waiting for the UI to
    /// resolve a save path before sending ZRPOS (so the sender doesn't
    /// stream data before the writer is installed).
    AwaitSavePath,
    /// Receive: streaming data blocks.
    Receiving,
    /// Send: waiting for ZRPOS (resume point) after sending ZFILE.
    AwaitRpos,
    /// Send: streaming data blocks.
    Sending,
    /// Done — ZFIN/ZEOF exchanged.
    Done,
    /// Cancelled/errored.
    Cancelled,
}

/// A ZMODEM transfer session.
///
/// Owns the [`Detector`] and the transfer state. The caller feeds PTY output
/// via [`feed`] and writes any bytes returned in `to_pty` back to the remote.
pub struct ZmodemSession {
    direction: Direction,
    phase: Phase,
    detector: Detector,
    /// Pending bytes to write back to the PTY (ZMODEM responses).
    to_pty: Vec<u8>,
    /// Output events for the UI layer.
    events: Vec<SessionEvent>,
    /// Negotiated CRC mode (defaults to 16-bit; upgraded to 32-bit if the
    /// receiver's ZRINIT advertises it).
    crc_mode: CrcMode,
    /// File metadata gathered from ZFILE (receive path).
    file_name: Option<String>,
    file_size: Option<u64>,
    file_mtime: Option<u64>,
    file_mode: Option<u32>,
    /// Bytes received/sent so far.
    bytes_transferred: u64,
    /// Offset from the last ZDATA header (receive path). Data subframes
    /// after ZDATA carry file data; the offset is relative to this.
    data_offset: u32,
    /// Local path for receive (set by the UI after FileOffer).
    save_path: Option<PathBuf>,
    /// Local file bytes to send (set by the UI before driving the send loop).
    send_payload: Option<Vec<u8>>,
    /// Offset into `send_payload` for the next block.
    send_offset: u32,
    /// Block size (1024 by default; lrzsz uses 1024 below 4KB window).
    block_size: u32,
}

impl Default for ZmodemSession {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ZmodemSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZmodemSession")
            .field("direction", &self.direction)
            .field("phase", &self.phase)
            .field("crc_mode", &self.crc_mode)
            .field("file_name", &self.file_name)
            .field("file_size", &self.file_size)
            .field("bytes_transferred", &self.bytes_transferred)
            .field("send_offset", &self.send_offset)
            .field(
                "send_payload_len",
                &self.send_payload.as_ref().map(|p| p.len()),
            )
            .finish()
    }
}

impl ZmodemSession {
    pub fn new() -> Self {
        Self {
            direction: Direction::Receive, // resolved on first frame
            phase: Phase::Init,
            detector: Detector::new(),
            to_pty: Vec::new(),
            events: Vec::new(),
            crc_mode: CrcMode::Crc16,
            file_name: None,
            file_size: None,
            file_mtime: None,
            file_mode: None,
            bytes_transferred: 0,
            data_offset: 0,
            save_path: None,
            send_payload: None,
            send_offset: 0,
            block_size: 1024,
        }
    }

    /// True if the session is actively consuming a transfer (between the
    /// first frame and completion/cancel).
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Done | Phase::Cancelled) && self.detector.is_armed()
    }

    /// True if the session has finished (Done or Cancelled) and can be
    /// torn down.
    pub fn is_finished(&self) -> bool {
        matches!(self.phase, Phase::Done | Phase::Cancelled)
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn bytes_transferred(&self) -> u64 {
        self.bytes_transferred
    }

    /// Set the local save path (receive path) after the UI resolves the
    /// FileOffer. When the session is in [`Phase::AwaitSavePath`] (ZFILE
    /// received, waiting for the user to pick a save location), this sends
    /// ZRPOS(0) to tell the sender to start streaming and transitions to
    /// [`Phase::Receiving`]. This ordering guarantees the writer is open
    /// before any data blocks arrive.
    pub fn set_save_path(&mut self, path: PathBuf) {
        self.save_path = Some(path);
        match self.phase {
            // Pre-ZFILE: stash the path; ZRINIT was already sent on ZRQINIT.
            Phase::Init | Phase::AwaitFile => {}
            // Post-ZFILE: the writer is ready, tell the sender to start.
            Phase::AwaitSavePath => {
                self.send_zrpos(0);
                self.phase = Phase::Receiving;
            }
            _ => {}
        }
    }

    /// Provide the file payload to send (send path) after the UI picks a
    /// local file. Triggers the ZFILE header to the receiver.
    pub fn begin_send(&mut self, name: String, payload: Vec<u8>) {
        self.direction = Direction::Send;
        self.send_payload = Some(payload.clone());
        self.file_name = Some(name.clone());
        self.file_size = Some(payload.len() as u64);
        self.send_zfile(&name, payload.len() as u64);
        self.phase = Phase::AwaitRpos;
    }

    /// Drain bytes that must be written back to the PTY.
    pub fn take_pty_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.to_pty)
    }

    /// Drain UI events.
    pub fn take_events(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    /// Feed PTY output bytes. Returns passthrough bytes (non-ZMODEM traffic
    /// that should still be rendered to the terminal).
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let (passthrough, detections) = self.detector.feed(data);
        for detection in detections {
            self.handle_detection(detection);
        }
        passthrough
    }

    fn handle_detection(&mut self, detection: Detection) {
        match detection {
            Detection::Cancelled => {
                self.phase = Phase::Cancelled;
                self.events.push(SessionEvent::Cancelled);
            }
            Detection::Frame(frame) => match frame {
                ZmodemFrame::Header(h) => self.handle_header(h),
                ZmodemFrame::Data {
                    offset,
                    payload,
                    end,
                } => {
                    self.handle_data(offset, payload, end);
                }
            },
        }
    }

    fn handle_header(&mut self, h: HeaderFrame) {
        match (self.phase, h.frame_type) {
            // ---- Receive path: remote `sz` sends ZRQINIT ----
            (Phase::Init, FrameType::ZRQInit) => {
                self.direction = Direction::Receive;
                self.events.push(SessionEvent::ReceiveOffered);
                // Respond with our ZRINIT (receiver capabilities).
                self.send_zrinit();
                self.phase = Phase::AwaitFile;
            }
            // ---- Send path: remote `rz` sends ZRINIT ----
            (Phase::Init, FrameType::ZRInit) => {
                self.direction = Direction::Send;
                // Check for 32-bit CRC capability flag (bit 3 of data[3] is
                // actually CANFC32 in some impls; lrzsz uses data[1] bit 2).
                // For simplicity we keep 16-bit CRC which lrzsz always accepts.
                self.events
                    .push(SessionEvent::SendOffered { flags: h.data });
                // The UI will call begin_send() once the user picks a file.
            }
            // ---- Receive: ZFILE carries the filename + metadata ----
            (Phase::AwaitFile, FrameType::ZFile) => {
                // The filename + size/mtime/mode arrive in the following data
                // subframe (parsed by handle_data). Do NOT emit FileOffer yet
                // — wait for the metadata so the save dialog has a real name.
                // Do NOT send ZRPOS yet — wait for the UI to resolve a save
                // path so the writer is installed before data streams in.
                self.phase = Phase::AwaitSavePath;
            }
            // ---- Send: ZRPOS tells us where to resume ----
            (Phase::AwaitRpos, FrameType::ZRPos) => {
                let pos = u32::from_le_bytes(h.data);
                self.send_offset = pos;
                self.phase = Phase::Sending;
                self.pump_send_blocks();
            }
            // ---- Receive: ZDATA carries the file offset, followed by data
            // subframes with actual file content. ----
            (Phase::Receiving, FrameType::ZData) => {
                self.data_offset = u32::from_le_bytes(h.data);
            }
            // ---- Send: ZACK acknowledges a data block ----
            (Phase::Sending, FrameType::ZAck) => {
                self.pump_send_blocks();
            }
            // ---- Receive: ZEOF = end of file ----
            (Phase::Receiving, FrameType::ZEof) => {
                let bytes = self.bytes_transferred;
                self.send_zfin();
                self.phase = Phase::Done;
                self.events.push(SessionEvent::Done {
                    direction: Direction::Receive,
                    bytes,
                });
            }
            // ---- Bidirectional: ZFIN tears down the session ----
            (_, FrameType::ZFin) => {
                // Acknowledge with our own ZFIN (if we haven't already).
                if self.phase != Phase::Done {
                    self.send_zfin();
                }
                self.phase = Phase::Done;
                self.events.push(SessionEvent::Done {
                    direction: self.direction,
                    bytes: self.bytes_transferred,
                });
            }
            // ---- Receive: ZSKIP = skip this file ----
            (_, FrameType::ZSkip) => {
                self.events.push(SessionEvent::Skipped);
                self.phase = Phase::Init;
            }
            // ---- Send: ZRINIT after we started sending = re-negotiation ----
            (Phase::Sending, FrameType::ZRInit) => {
                // Ignore; keep sending.
            }
            _ => {
                // Unhandled header in this phase — ignore.
            }
        }
    }

    fn handle_data(&mut self, offset: u32, payload: Vec<u8>, end: DataEnd) {
        // Receive path: the first data after ZFILE carries the filename +
        // metadata as a NUL-separated string: "name\0size mtime mode\0".
        // This arrives while we're in AwaitSavePath (before ZRPOS is sent).
        if (self.phase == Phase::Receiving || self.phase == Phase::AwaitSavePath)
            && self.file_name.is_none()
            && self.bytes_transferred == 0
        {
            // This is the ZFILE metadata subframe (not actual file data).
            self.parse_file_metadata(&payload);
            // Emit the FileOffer with the now-known name. The UI opens a
            // save dialog; when it resolves, set_save_path() sends ZRPOS
            // and transitions to Receiving. We do NOT send ZRPOS here.
            self.events.push(SessionEvent::FileOffer {
                name: self.file_name.clone().unwrap_or_default(),
                size: self.file_size,
                mtime: self.file_mtime,
                mode: self.file_mode,
            });
            return;
        }
        // Actual file data. The offset from the ZDATA header is the base;
        // data subframes are relative to it.
        let abs_offset = self.data_offset as u64 + offset as u64;
        self.bytes_transferred = abs_offset + payload.len() as u64;
        self.events.push(SessionEvent::DataReceived {
            offset: abs_offset as u32,
            len: payload.len(),
            total: self.file_size,
            data: payload,
        });
        if end.expects_ack() {
            self.send_zack(abs_offset as u32);
        }
    }

    fn parse_file_metadata(&mut self, payload: &[u8]) {
        // Format: "filename\0size mtime mode\0" (lrzsz zsendfinfo).
        let mut parts = payload.splitn(2, |&b| b == 0);
        if let Some(name_bytes) = parts.next() {
            self.file_name = Some(String::from_utf8_lossy(name_bytes).into_owned());
        }
        if let Some(meta_bytes) = parts.next() {
            // Strip a trailing NUL terminator (lrzsz appends one) before
            // splitting on whitespace, since NUL is not a hex digit and
            // would poison the octal mode parse.
            let trimmed = meta_bytes.split(|&b| b == 0).next().unwrap_or(&[]);
            let meta_str = String::from_utf8_lossy(trimmed);
            let meta: Vec<&str> = meta_str.split_whitespace().collect();
            if let Some(s) = meta.first() {
                self.file_size = s.parse().ok();
            }
            if let Some(s) = meta.get(1) {
                self.file_mtime = s.parse().ok();
            }
            if let Some(s) = meta.get(2) {
                self.file_mode = u32::from_str_radix(s.trim_start_matches("0o"), 8).ok();
            }
        }
    }

    fn pump_send_blocks(&mut self) {
        // Clone the payload reference to avoid a borrow conflict with
        // send_zdata (which needs &mut self to push to to_pty).
        let payload = self.send_payload.clone();
        let Some(payload) = payload else {
            return;
        };
        // Send one block per ACK (simple stop-and-wait). A production impl
        // would window multiple blocks, but lrzsz tolerates stop-and-wait.
        let start = self.send_offset as usize;
        if start >= payload.len() {
            // All data sent — emit ZEOF and wait for ZFIN.
            self.send_zeof(payload.len() as u32);
            return;
        }
        // Send a ZDATA header with the current offset, followed by a data
        // subframe. This is what real lrzsz does: ZDATA header carries the
        // file offset, data subframes carry the actual bytes.
        self.send_zdata(start as u32);
        let end = (start + self.block_size as usize).min(payload.len());
        let chunk = &payload[start..end];
        let block_end = if end == payload.len() {
            DataEnd::AckEnd
        } else {
            DataEnd::AckContinue
        };
        let frame = crate::frame::encode_data_subframe(chunk, block_end, self.crc_mode);
        self.to_pty.extend_from_slice(&frame);
        self.send_offset = end as u32;
        self.bytes_transferred = end as u64;
    }

    // -----------------------------------------------------------------
    // Frame builders
    // -----------------------------------------------------------------

    fn send_zrinit(&mut self) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZRInit,
            // data[0..2] = max block (0 = use default), data[2] = flags
            // (CANFDX=0x01 | CANOVIO=0x02). We advertise full duplex +
            // overlap I/O. We deliberately do NOT set CANFC32 (0x20) so that
            // lrzsz uses ZBIN/CRC16 for binary headers and data subframes,
            // which is simpler and better tested.
            data: [0x00, 0x00, 0x03, 0x00],
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }

    fn send_zrpos(&mut self, pos: u32) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZRPos,
            data: pos.to_le_bytes(),
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }

    fn send_zack(&mut self, pos: u32) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZAck,
            data: pos.to_le_bytes(),
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }

    fn send_zfin(&mut self) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZFin,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }

    fn send_zfile(&mut self, name: &str, size: u64) {
        // ZFILE header + data subframe: "name\0size mtime mode\0" ZCRCW.
        let frame = HeaderFrame {
            frame_type: FrameType::ZFile,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
        let meta = format!("{} {} 0 0\0", size, 0);
        let mut payload = name.as_bytes().to_vec();
        payload.push(0);
        payload.extend_from_slice(meta.as_bytes());
        // Use encode_data_subframe (no ZDATA leader, no offset) — this is
        // what real lrzsz sends for the ZFILE metadata subframe.
        let block = crate::frame::encode_data_subframe(&payload, DataEnd::AckEnd, self.crc_mode);
        self.to_pty.extend_from_slice(&block);
    }

    fn send_zeof(&mut self, pos: u32) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZEof,
            data: pos.to_le_bytes(),
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }

    fn send_zdata(&mut self, offset: u32) {
        let frame = HeaderFrame {
            frame_type: FrameType::ZData,
            data: offset.to_le_bytes(),
            crc_mode: CrcMode::Crc16,
        };
        self.to_pty
            .extend_from_slice(&crate::frame::encode_hex_header(&frame));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ZDLE, ZHEX, ZPAD};

    #[test]
    fn new_session_starts_idle_and_inactive() {
        let s = ZmodemSession::new();
        assert!(!s.is_active());
        assert!(!s.is_finished());
        assert_eq!(s.bytes_transferred(), 0);
    }

    #[test]
    fn receive_path_zrqinit_triggers_receive_offered_and_zrinit_response() {
        let mut s = ZmodemSession::new();
        // Feed a ZRQINIT hex frame (what `sz` emits).
        let frame = HeaderFrame {
            frame_type: FrameType::ZRQInit,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let enc = crate::frame::encode_hex_header(&frame);
        s.feed(&enc);
        let events = s.take_events();
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::ReceiveOffered)));
        // The session should have emitted a ZRINIT back to the PTY.
        let pty = s.take_pty_output();
        assert!(pty.starts_with(&[ZPAD, ZPAD, ZDLE, ZHEX]));
        assert!(s.is_active());
        assert_eq!(s.direction(), Direction::Receive);
    }

    #[test]
    fn send_path_zrinit_triggers_send_offered() {
        let mut s = ZmodemSession::new();
        let frame = HeaderFrame {
            frame_type: FrameType::ZRInit,
            data: [0x00, 0x00, 0x21, 0x00],
            crc_mode: CrcMode::Crc16,
        };
        let enc = crate::frame::encode_hex_header(&frame);
        s.feed(&enc);
        let events = s.take_events();
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::SendOffered { .. })));
        assert_eq!(s.direction(), Direction::Send);
    }

    #[test]
    fn begin_send_emits_zfile_header_and_waits_for_rpos() {
        let mut s = ZmodemSession::new();
        s.direction = Direction::Send;
        s.begin_send("test.txt".to_string(), b"hello".to_vec());
        let pty = s.take_pty_output();
        // Should contain a ZFILE hex header.
        assert!(pty.windows(4).any(|w| w == [ZPAD, ZPAD, ZDLE, ZHEX]));
    }

    #[test]
    fn zfin_completes_the_session() {
        let mut s = ZmodemSession::new();
        s.direction = Direction::Receive;
        let frame = HeaderFrame {
            frame_type: FrameType::ZFin,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let enc = crate::frame::encode_hex_header(&frame);
        s.feed(&enc);
        let events = s.take_events();
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Done {
                direction: Direction::Receive,
                ..
            }
        )));
        assert!(s.is_finished());
    }

    #[test]
    fn cancel_event_marks_session_cancelled() {
        let mut s = ZmodemSession::new();
        s.feed(&[0x18; 8]); // 8× CAN
        let events = s.take_events();
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Cancelled)));
        assert!(s.is_finished());
    }

    #[test]
    fn parse_file_metadata_extracts_name_size_mtime_mode() {
        let mut s = ZmodemSession::new();
        // Use explicit NUL bytes — `\012` in a byte string is an octal
        // escape (LF), not a NUL. Split on real NUL (0x00).
        let mut payload = b"report.pdf".to_vec();
        payload.push(0x00);
        payload.extend_from_slice(b"12345 1700000000 0644");
        payload.push(0x00);
        s.parse_file_metadata(&payload);
        assert_eq!(s.file_name.as_deref(), Some("report.pdf"));
        assert_eq!(s.file_size, Some(12345));
        assert_eq!(s.file_mtime, Some(1700000000));
        assert_eq!(s.file_mode, Some(0o644));
    }

    #[test]
    fn parse_file_metadata_handles_missing_optional_fields() {
        let mut s = ZmodemSession::new();
        let mut payload = b"plain.txt".to_vec();
        payload.push(0x00);
        payload.push(0x00);
        s.parse_file_metadata(&payload);
        assert_eq!(s.file_name.as_deref(), Some("plain.txt"));
        assert_eq!(s.file_size, None);
        assert_eq!(s.file_mtime, None);
        assert_eq!(s.file_mode, None);
    }

    #[test]
    fn zskip_returns_session_to_init_and_emits_skipped() {
        let mut s = ZmodemSession::new();
        s.phase = Phase::AwaitFile;
        let frame = HeaderFrame {
            frame_type: FrameType::ZSkip,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let enc = crate::frame::encode_hex_header(&frame);
        s.feed(&enc);
        let events = s.take_events();
        assert!(events.iter().any(|e| matches!(e, SessionEvent::Skipped)));
    }

    // ---- Full receive-path integration test ----

    #[test]
    fn receive_path_zfile_with_metadata_triggers_file_offer() {
        // Simulate the full `sz` flow: ZRQINIT → ZRINIT response →
        // ZFILE header + data subframe (filename + metadata).
        let mut s = ZmodemSession::new();

        // 1. Feed ZRQINIT (what `sz` sends first).
        let zrqinit = HeaderFrame {
            frame_type: FrameType::ZRQInit,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        s.feed(&crate::frame::encode_hex_header(&zrqinit));
        let events = s.take_events();
        assert!(events
            .iter()
            .any(|e| matches!(e, SessionEvent::ReceiveOffered)));
        // Session should have emitted ZRINIT.
        assert!(!s.take_pty_output().is_empty());

        // 2. Feed ZFILE header + data subframe (what `sz` sends after
        //    receiving ZRINIT). The data subframe carries the filename
        //    and metadata: "k8s-node.ini\0size mtime mode\0".
        let zfile = HeaderFrame {
            frame_type: FrameType::ZFile,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let mut zfile_bytes = crate::frame::encode_hex_header(&zfile);
        let mut metadata = b"k8s-node.ini".to_vec();
        metadata.push(0x00); // NUL separator
        metadata.extend_from_slice(b"1024 1700000000 0644");
        metadata.push(0x00); // trailing NUL
        let subframe =
            crate::frame::encode_data_subframe(&metadata, DataEnd::AckEnd, CrcMode::Crc16);
        zfile_bytes.extend_from_slice(&subframe);
        s.feed(&zfile_bytes);

        // 3. The session should have emitted a FileOffer event with the
        //    filename extracted from the data subframe.
        let events = s.take_events();
        let offer = events
            .iter()
            .find_map(|e| match e {
                SessionEvent::FileOffer { name, size, .. } => Some((name.clone(), *size)),
                _ => None,
            })
            .expect("expected FileOffer event");
        assert_eq!(offer.0, "k8s-node.ini");
        assert_eq!(offer.1, Some(1024));
    }

    #[test]
    fn receive_path_full_transfer_with_zdata_and_zeof() {
        // Full transfer: ZRQINIT → ZRINIT → ZFILE + metadata →
        // (save path set) → ZRPOS sent → ZDATA + data → ZEOF → ZFIN.
        let mut s = ZmodemSession::new();

        // ZRQINIT
        let zrqinit = HeaderFrame {
            frame_type: FrameType::ZRQInit,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        s.feed(&crate::frame::encode_hex_header(&zrqinit));
        let _ = s.take_events();
        let _ = s.take_pty_output();

        // ZFILE + metadata
        let zfile = HeaderFrame {
            frame_type: FrameType::ZFile,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let mut zfile_bytes = crate::frame::encode_hex_header(&zfile);
        let mut metadata = b"data.bin".to_vec();
        metadata.push(0x00);
        metadata.extend_from_slice(b"5 0 0");
        metadata.push(0x00);
        zfile_bytes.extend_from_slice(&crate::frame::encode_data_subframe(
            &metadata,
            DataEnd::AckEnd,
            CrcMode::Crc16,
        ));
        s.feed(&zfile_bytes);
        let _ = s.take_events(); // FileOffer
        let _ = s.take_pty_output();

        // Simulate the UI setting the save path (sends ZRPOS).
        s.set_save_path(std::path::PathBuf::from("/dev/null"));
        let pty = s.take_pty_output();
        // ZRPOS should have been sent.
        assert!(!pty.is_empty());

        // ZDATA header + data subframe ("Hello" = 5 bytes).
        let zdata = HeaderFrame {
            frame_type: FrameType::ZData,
            data: 0u32.to_le_bytes(), // offset = 0
            crc_mode: CrcMode::Crc16,
        };
        let mut zdata_bytes = crate::frame::encode_hex_header(&zdata);
        zdata_bytes.extend_from_slice(&crate::frame::encode_data_subframe(
            b"Hello",
            DataEnd::AckEnd,
            CrcMode::Crc16,
        ));
        s.feed(&zdata_bytes);
        let events = s.take_events();
        // Should have DataReceived event with the file data.
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::DataReceived { data, .. } if data == b"Hello"
        )));

        // ZEOF
        let zeof = HeaderFrame {
            frame_type: FrameType::ZEof,
            data: 5u32.to_le_bytes(), // position = 5 (file size)
            crc_mode: CrcMode::Crc16,
        };
        s.feed(&crate::frame::encode_hex_header(&zeof));
        let events = s.take_events();
        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Done {
                direction: Direction::Receive,
                bytes: 5
            }
        )));
        assert!(s.is_finished());
    }
}
