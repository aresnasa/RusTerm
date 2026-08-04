//! Stream-based ZMODEM frame detection and parsing.
//!
//! The [`Detector`] is fed raw bytes from the PTY/SSH channel. It scans for
//! the ZMODEM frame leader (`ZPAD ZPAD ZDLE <fmt>`) and, once a complete frame
//! is assembled, emits a [`Detection`] the caller can act on. Bytes that are
//! NOT part of a ZMODEM frame are returned to the caller so they can be
//! forwarded to the terminal renderer unchanged.
//!
//! This split keeps the protocol logic UI-agnostic and fully unit-testable.

use crate::crc::{crc16_init, crc32_init};
use crate::frame::{
    CrcMode, DataEnd, FrameType, HeaderFrame, ZmodemFrame, zdle_decode, zdle_decode_n,
};
use crate::{ZBIN, ZBIN32, ZDLE, ZHEX, ZPAD};

/// The leader sequence `lrzsz` uses to open a hex header: `ZPAD ZPAD ZDLE ZHEX`.
const HEX_LEADER: [u8; 4] = [ZPAD, ZPAD, ZDLE, ZHEX];
/// The shorter binary leader: `ZPAD ZDLE <fmt>`. Some senders emit only one
/// ZPAD before the binary frame.
const BIN_LEADER: [u8; 3] = [ZPAD, ZDLE, ZBIN];
const BIN32_LEADER: [u8; 3] = [ZPAD, ZDLE, ZBIN32];

/// A successful detection emitted by the [`Detector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// A complete ZMODEM frame was parsed. Bytes consumed by the frame are
    /// NOT included in the `passthrough` returned by [`Detector::feed`].
    Frame(ZmodemFrame),
    /// The transfer was cancelled by the peer (5× `ZPAD ZDLE ZCAN` or a
    /// sequence of 8+ `CAN` bytes).
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    /// Scanning the raw stream for a leader byte.
    Idle,
    /// Seen one `ZPAD`.
    Pad1,
    /// Seen `ZPAD ZPAD`.
    Pad2,
    /// Seen `ZPAD ZDLE`.
    PadDle,
    /// Seen `ZPAD ZDLE <fmt>` — now collecting the frame body.
    InFrame { fmt: u8 },
    /// Collecting a data subframe (after a ZFILE or ZDATA header). Data
    /// subframes have no `ZPAD ZDLE` leader; they start directly with
    /// ZDLE-escaped bytes and end with `ZDLE <frameend> <crc>`.
    InData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameCollect {
    Hex,
    Bin16,
    Bin32,
}

#[derive(Debug, Default)]
struct Collector {
    buf: Vec<u8>,
    kind: Option<FrameCollect>,
}

/// Streaming ZMODEM detector.
///
/// Feed it bytes via [`feed`]; it returns the bytes that should still be
/// forwarded to the terminal renderer (i.e. everything that is NOT part of a
/// completed ZMODEM frame), plus any [`Detection`]s that fired this call.
///
/// When a frame is mid-stream (the leader was seen but the frame is not yet
/// complete), the detector holds the partial bytes internally and returns an
/// empty passthrough — the caller must NOT render those bytes because they
/// belong to an in-progress ZMODEM frame.
pub struct Detector {
    state: ScanState,
    collector: Collector,
    /// Consecutive `CAN` (0x18) bytes seen outside a frame — 8+ triggers a
    /// cancel (lrzsz uses 5× ZPAD|ZDLE|ZCAN, but plain CAN×8 is the common
    /// user-initiated cancel).
    can_run: usize,
    /// True once a frame has been delivered and the detector is "armed" —
    /// between the first frame and session end, stray passthrough bytes are
    /// suppressed (they're protocol noise, not terminal output).
    armed: bool,
    /// CRC mode for data subframes. Defaults to Crc16 (we don't advertise
    /// CANFC32 in ZRINIT, so lrzsz uses 16-bit CRC). Updated when a ZBIN32
    /// binary header is seen.
    data_crc_mode: CrcMode,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Self {
            state: ScanState::Idle,
            collector: Collector::default(),
            can_run: 0,
            armed: false,
            data_crc_mode: CrcMode::Crc16,
        }
    }

    /// True if the detector has seen at least one complete frame and is now
    /// actively consuming a ZMODEM session (suppressed passthrough).
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Feed a chunk of bytes. Returns `(passthrough, detections)`:
    ///
    ///  * `passthrough` — bytes that should still be rendered to the terminal.
    ///    Empty while a frame is mid-stream.
    ///  * `detections` — completed frames / cancel events, in order.
    pub fn feed(&mut self, data: &[u8]) -> (Vec<u8>, Vec<Detection>) {
        let mut passthrough = Vec::new();
        let mut detections = Vec::new();
        for &b in data {
            self.process_byte(b, &mut passthrough, &mut detections);
        }
        (passthrough, detections)
    }

    /// Process a single byte. When a frame completes, any trailing bytes
    /// (e.g. a data subframe following a ZFILE header, or the next frame
    /// in a batch) are re-fed recursively.
    fn process_byte(&mut self, b: u8, passthrough: &mut Vec<u8>, detections: &mut Vec<Detection>) {
        // Outside a frame, count CAN runs for cancel detection.
        if self.state == ScanState::Idle && self.collector.kind.is_none() {
            if b == 0x18 {
                self.can_run += 1;
                if self.can_run >= 8 {
                    detections.push(Detection::Cancelled);
                    self.can_run = 0;
                    self.armed = false;
                    return;
                }
            } else {
                self.can_run = 0;
            }
        }

        match self.state {
            ScanState::Idle => {
                if b == ZPAD {
                    self.state = ScanState::Pad1;
                } else {
                    if !self.armed {
                        passthrough.push(b);
                    }
                    // If armed but the byte isn't a leader, it's protocol
                    // noise between frames (e.g. CR/LF) — suppress it.
                }
            }
            ScanState::Pad1 => {
                if b == ZPAD {
                    self.state = ScanState::Pad2;
                } else if b == ZDLE {
                    // `ZPAD ZDLE <fmt>` (single-pad binary leader).
                    self.state = ScanState::PadDle;
                } else {
                    // Not a leader — flush the pending ZPAD as passthrough.
                    if !self.armed {
                        passthrough.push(ZPAD);
                        passthrough.push(b);
                    }
                    self.state = ScanState::Idle;
                }
            }
            ScanState::Pad2 => {
                if b == ZDLE {
                    self.state = ScanState::PadDle;
                } else {
                    if !self.armed {
                        passthrough.push(ZPAD);
                        passthrough.push(ZPAD);
                        passthrough.push(b);
                    }
                    self.state = ScanState::Idle;
                }
            }
            ScanState::PadDle => {
                match b {
                    ZHEX => self.begin_frame(FrameCollect::Hex, b),
                    ZBIN => self.begin_frame(FrameCollect::Bin16, b),
                    ZBIN32 => self.begin_frame(FrameCollect::Bin32, b),
                    _ => {
                        // False alarm — not a ZMODEM leader.
                        if !self.armed {
                            // Flush whatever pads we accumulated.
                            passthrough.push(ZPAD);
                            passthrough.push(ZDLE);
                            passthrough.push(b);
                        }
                        self.state = ScanState::Idle;
                    }
                }
            }
            ScanState::InFrame { fmt } => {
                self.collector.buf.push(b);
                if let Some((detection, consumed)) = self.try_complete(fmt) {
                    self.armed = true;
                    // Update CRC mode based on the header format.
                    if let Detection::Frame(ZmodemFrame::Header(ref h)) = detection {
                        if h.crc_mode == CrcMode::Crc32 {
                            self.data_crc_mode = CrcMode::Crc32;
                        }
                    }
                    // Check if this header expects a following data subframe.
                    let expect_data = matches!(
                        &detection,
                        Detection::Frame(ZmodemFrame::Header(h))
                            if matches!(h.frame_type, FrameType::ZFile | FrameType::ZData)
                    );
                    detections.push(detection);
                    // Save remaining bytes (trailing data after the header).
                    let remaining = self.collector.buf[consumed..].to_vec();
                    self.collector = Collector::default();
                    self.state = if expect_data {
                        ScanState::InData
                    } else {
                        ScanState::Idle
                    };
                    // Re-feed remaining bytes recursively.
                    for &rb in &remaining {
                        self.process_byte(rb, passthrough, detections);
                    }
                }
            }
            ScanState::InData => {
                self.collector.buf.push(b);
                if let Some((detection, consumed)) = self.try_complete_data() {
                    // Check if more data subframes are expected (Continue /
                    // AckContinue = more data follows; End / AckEnd = done).
                    let more_data = matches!(
                        &detection,
                        Detection::Frame(ZmodemFrame::Data { end, .. })
                            if !end.is_end()
                    );
                    detections.push(detection);
                    // Save remaining bytes.
                    let remaining = self.collector.buf[consumed..].to_vec();
                    self.collector = Collector::default();
                    self.state = if more_data {
                        ScanState::InData
                    } else {
                        ScanState::Idle
                    };
                    // Re-feed remaining bytes recursively.
                    for &rb in &remaining {
                        self.process_byte(rb, passthrough, detections);
                    }
                }
            }
        }
    }

    fn begin_frame(&mut self, kind: FrameCollect, fmt: u8) {
        self.collector = Collector {
            buf: Vec::new(),
            kind: Some(kind),
        };
        self.state = ScanState::InFrame { fmt };
    }

    /// Attempt to parse a complete header frame from the collector buffer.
    /// Returns `(detection, consumed_byte_count)` or `None` if more bytes
    /// are needed.
    fn try_complete(&self, _fmt: u8) -> Option<(Detection, usize)> {
        let kind = self.collector.kind?;
        let buf = &self.collector.buf;
        let (frame, consumed) = match kind {
            FrameCollect::Hex => Self::complete_hex(buf)?,
            FrameCollect::Bin16 => Self::complete_bin(buf, CrcMode::Crc16)?,
            FrameCollect::Bin32 => Self::complete_bin(buf, CrcMode::Crc32)?,
        };
        Some((Detection::Frame(frame), consumed))
    }

    fn complete_hex(buf: &[u8]) -> Option<(ZmodemFrame, usize)> {
        // Hex header: <type:2> <data:8> <crc:4> CR (LF|0x80)
        // Need at least 14 hex chars + 1 CR = 15 bytes.
        if buf.len() < 15 {
            return None;
        }
        // Must end with CR (optionally followed by LF or 0x80).
        let cr_idx = buf.iter().position(|&b| b == b'\r')?;
        if cr_idx < 14 || cr_idx % 2 != 0 {
            return None;
        }
        let hex_end = cr_idx;
        let hex_bytes = &buf[..hex_end];
        if hex_bytes.len() != 14 {
            return None;
        }
        let decoded = decode_hex(hex_bytes)?;
        // decoded = [type, d0, d1, d2, d3, crc_hi, crc_lo]
        let frame_type = FrameType::from_byte(decoded[0])?;
        let data = [decoded[1], decoded[2], decoded[3], decoded[4]];
        let expected_crc = u16::from_be_bytes([decoded[5], decoded[6]]);
        let actual_crc = crc16_init(&decoded[..5]);
        if expected_crc != actual_crc {
            return None;
        }
        // Consumed = 14 hex bytes + CR + optional LF/0x80.
        // We must wait for at least one byte after CR so we can decide
        // whether to consume the LF/0x80. Without this, the LF would be
        // left in the collector and misinterpreted as the first byte of a
        // data subframe.
        if cr_idx + 1 >= buf.len() {
            return None; // need at least one more byte after CR
        }
        let mut consumed = cr_idx + 1; // hex + CR
        if buf[consumed] == b'\n' || buf[consumed] == 0x80 {
            consumed += 1;
        }
        Some((
            ZmodemFrame::Header(HeaderFrame {
                frame_type,
                data,
                crc_mode: CrcMode::Crc16,
            }),
            consumed,
        ))
    }

    fn complete_bin(buf: &[u8], crc_mode: CrcMode) -> Option<(ZmodemFrame, usize)> {
        // Binary header: <type:1 ZDLE-esc> <4 data bytes ZDLE-esc> <crc>
        // CRC is 2 bytes (Crc16) or 4 bytes (Crc32).
        let crc_len = match crc_mode {
            CrcMode::Crc16 => 2,
            CrcMode::Crc32 => 4,
        };
        let needed = 1 + 4 + crc_len;
        // Decode exactly `needed` bytes from the buffer. This stops after the
        // header body, leaving any trailing bytes (e.g. a data subframe) in
        // the collector for re-processing.
        let (decoded, consumed) = zdle_decode_n(buf, needed)?;
        let frame_type = FrameType::from_byte(decoded[0])?;
        let data = [decoded[1], decoded[2], decoded[3], decoded[4]];
        match crc_mode {
            CrcMode::Crc16 => {
                let expected = u16::from_be_bytes([decoded[5], decoded[6]]);
                let actual = crc16_init(&decoded[..5]);
                if expected != actual {
                    return None;
                }
            }
            CrcMode::Crc32 => {
                let expected = u32::from_le_bytes([decoded[5], decoded[6], decoded[7], decoded[8]]);
                let actual = crc32_init(&decoded[..5]);
                if expected != actual {
                    return None;
                }
            }
        }
        Some((
            ZmodemFrame::Header(HeaderFrame {
                frame_type,
                data,
                crc_mode,
            }),
            consumed,
        ))
    }

    /// Attempt to parse a complete data subframe from the collector buffer.
    /// Data subframes have the format: `<escaped data> ZDLE <frameend> <crc>`.
    /// Returns `(detection, consumed_byte_count)` or `None` if more bytes
    /// are needed.
    fn try_complete_data(&self) -> Option<(Detection, usize)> {
        let buf = &self.collector.buf;
        let crc_len = match self.data_crc_mode {
            CrcMode::Crc16 => 2,
            CrcMode::Crc32 => 4,
        };
        // Scan for ZDLE <frameend> (0x68..=0x6B) — the data subframe
        // terminator. ZDLE followed by 0x68-0x6B is ALWAYS a terminator,
        // never a data escape, so this scan is unambiguous.
        let mut i = 0;
        let mut found = None;
        while i + 1 < buf.len() {
            if buf[i] == ZDLE {
                let next = buf[i + 1];
                if DataEnd::from_byte(next).is_some() {
                    found = Some((i, next));
                    break;
                }
            }
            i += 1;
        }
        let (term_pos, end_byte) = found?;
        let end = DataEnd::from_byte(end_byte)?;
        // Data is buf[..term_pos] — ZDLE-decode it (no terminators in data).
        let (data, _) = zdle_decode(&buf[..term_pos], &[]);
        // CRC follows the terminator: ZDLE-escaped, `crc_len` decoded bytes.
        let crc_start = term_pos + 2; // skip ZDLE + end_byte
        let (crc_decoded, crc_consumed) = zdle_decode_n(&buf[crc_start..], crc_len)?;
        // Verify CRC.
        let crc_ok = match self.data_crc_mode {
            CrcMode::Crc16 => {
                let expected = u16::from_be_bytes([crc_decoded[0], crc_decoded[1]]);
                let actual = crc16_init(&data);
                expected == actual
            }
            CrcMode::Crc32 => {
                let expected = u32::from_le_bytes([
                    crc_decoded[0],
                    crc_decoded[1],
                    crc_decoded[2],
                    crc_decoded[3],
                ]);
                let actual = crc32_init(&data);
                expected == actual
            }
        };
        if !crc_ok {
            // CRC mismatch — silently drop the subframe. This is unlikely
            // in practice; if it happens the session will time out.
            return None;
        }
        let consumed = crc_start + crc_consumed;
        Some((
            Detection::Frame(ZmodemFrame::Data {
                offset: 0, // offset is in the ZDATA header, not the subframe
                payload: data,
                end,
            }),
            consumed,
        ))
    }
}

fn decode_hex(hex: &[u8]) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for chunk in hex.chunks_exact(2) {
        let hi = hex_val(chunk[0])?;
        let lo = hex_val(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// Suppress unused warnings for the leader constants — they document the
// protocol even though the detector matches byte-by-byte.
#[allow(dead_code)]
const _: [u8; 4] = HEX_LEADER;
#[allow(dead_code)]
const _: [u8; 3] = BIN_LEADER;
#[allow(dead_code)]
const _: [u8; 3] = BIN32_LEADER;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZHEX;
    use crate::frame::{DataEnd, encode_bin_header, encode_bin32_header, encode_hex_header};

    fn make_header(ft: FrameType, data: [u8; 4]) -> HeaderFrame {
        HeaderFrame {
            frame_type: ft,
            data,
            crc_mode: CrcMode::Crc16,
        }
    }

    #[test]
    fn non_zmodem_bytes_pass_through_unchanged() {
        let mut d = Detector::new();
        let (pass, det) = d.feed(b"hello world\n");
        assert_eq!(pass, b"hello world\n");
        assert!(det.is_empty());
        assert!(!d.is_armed());
    }

    #[test]
    fn zpad_not_followed_by_zdle_passes_through() {
        let mut d = Detector::new();
        // "*" then "A" — not a leader.
        let (pass, det) = d.feed(b"*A");
        assert_eq!(pass, b"*A");
        assert!(det.is_empty());
    }

    #[test]
    fn detects_hex_zrqinit_frame() {
        let frame = make_header(FrameType::ZRQInit, [0, 0, 0, 0]);
        let enc = encode_hex_header(&frame);
        let mut d = Detector::new();
        let (pass, det) = d.feed(&enc);
        // Hex frame fully consumed → no passthrough.
        assert_eq!(pass, b"");
        assert_eq!(det.len(), 1);
        match &det[0] {
            Detection::Frame(ZmodemFrame::Header(h)) => {
                assert_eq!(h.frame_type, FrameType::ZRQInit);
                assert_eq!(h.data, [0, 0, 0, 0]);
            }
            other => panic!("expected ZRQInit header, got {:?}", other),
        }
        assert!(d.is_armed());
    }

    #[test]
    fn detects_hex_zrinit_frame_with_capability_flags() {
        // ZRINIT with data = [0x00, 0x00, 0x20, 0x00] (CANFDX | CANOVIO).
        let frame = make_header(FrameType::ZRInit, [0x00, 0x00, 0x20, 0x00]);
        let enc = encode_hex_header(&frame);
        let mut d = Detector::new();
        let (_, det) = d.feed(&enc);
        assert_eq!(det.len(), 1);
        if let Detection::Frame(ZmodemFrame::Header(h)) = &det[0] {
            assert_eq!(h.frame_type, FrameType::ZRInit);
            assert_eq!(h.data, [0x00, 0x00, 0x20, 0x00]);
        } else {
            panic!("wrong detection");
        }
    }

    #[test]
    fn detects_hex_zfin_frame() {
        let frame = make_header(FrameType::ZFin, [0, 0, 0, 0]);
        let enc = encode_hex_header(&frame);
        let mut d = Detector::new();
        let (_, det) = d.feed(&enc);
        assert_eq!(det.len(), 1);
        if let Detection::Frame(ZmodemFrame::Header(h)) = &det[0] {
            assert_eq!(h.frame_type, FrameType::ZFin);
        } else {
            panic!("wrong detection");
        }
    }

    #[test]
    fn detects_bin_header_frame() {
        let frame = make_header(FrameType::ZFin, [0, 0, 0, 0]);
        let enc = encode_bin_header(&frame);
        let mut d = Detector::new();
        let (_, det) = d.feed(&enc);
        assert_eq!(det.len(), 1);
        if let Detection::Frame(ZmodemFrame::Header(h)) = &det[0] {
            assert_eq!(h.frame_type, FrameType::ZFin);
            assert_eq!(h.crc_mode, CrcMode::Crc16);
        } else {
            panic!("wrong detection");
        }
    }

    #[test]
    fn detects_bin32_header_frame() {
        let frame = HeaderFrame {
            frame_type: FrameType::ZFin,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc32,
        };
        let enc = encode_bin32_header(&frame);
        let mut d = Detector::new();
        let (_, det) = d.feed(&enc);
        assert_eq!(det.len(), 1);
        if let Detection::Frame(ZmodemFrame::Header(h)) = &det[0] {
            assert_eq!(h.frame_type, FrameType::ZFin);
            assert_eq!(h.crc_mode, CrcMode::Crc32);
        } else {
            panic!("wrong detection");
        }
    }

    #[test]
    fn partial_frame_holds_bytes_and_suppresses_passthrough() {
        let frame = make_header(FrameType::ZRQInit, [0, 0, 0, 0]);
        let enc = encode_hex_header(&frame);
        let (first, rest) = enc.split_at(6);
        let mut d = Detector::new();
        let (pass1, det1) = d.feed(first);
        // Partial frame → no passthrough, no detection yet.
        assert_eq!(pass1, b"");
        assert!(det1.is_empty());
        let (pass2, det2) = d.feed(rest);
        assert_eq!(pass2, b"");
        assert_eq!(det2.len(), 1);
    }

    #[test]
    fn bytes_before_frame_pass_through_then_frame_is_consumed() {
        let mut d = Detector::new();
        let frame = make_header(FrameType::ZRQInit, [0, 0, 0, 0]);
        let enc = encode_hex_header(&frame);
        let mut input = b"prompt$ ".to_vec();
        input.extend_from_slice(&enc);
        let (pass, det) = d.feed(&input);
        assert_eq!(&pass, b"prompt$ ");
        assert_eq!(det.len(), 1);
    }

    #[test]
    fn rejects_hex_frame_with_bad_crc() {
        // Build a hex frame then corrupt the last CRC nibble.
        let frame = make_header(FrameType::ZRQInit, [0, 0, 0, 0]);
        let mut enc = encode_hex_header(&frame);
        // Flip the last CRC hex char.
        let last = enc.len() - 3; // last hex char before CR LF
        enc[last] = if enc[last] == b'0' { b'1' } else { b'0' };
        let mut d = Detector::new();
        let (_, det) = d.feed(&enc);
        // Bad CRC → no detection (frame silently dropped).
        assert!(det.is_empty());
    }

    #[test]
    fn cancels_on_eight_can_bytes() {
        let mut d = Detector::new();
        let (_, det) = d.feed(&[0x18; 8]);
        assert_eq!(det.len(), 1);
        assert_eq!(det[0], Detection::Cancelled);
    }

    #[test]
    fn armed_detector_suppresses_inter_frame_noise() {
        let mut d = Detector::new();
        let frame = make_header(FrameType::ZRQInit, [0, 0, 0, 0]);
        let enc = encode_hex_header(&frame);
        let (_, det) = d.feed(&enc);
        assert_eq!(det.len(), 1);
        assert!(d.is_armed());
        // After arming, stray CR/LF between frames is suppressed.
        let (pass, _) = d.feed(b"\r\n");
        assert_eq!(pass, b"");
    }

    // ---- Regression tests for ZFILE header + data subframe ----

    #[test]
    fn zfile_hex_header_followed_by_data_subframe_in_one_chunk() {
        // Simulate what real `sz` sends: ZFILE hex header immediately
        // followed by the metadata data subframe (filename + size + ZCRCW).
        let header = make_header(FrameType::ZFile, [0, 0, 0, 0]);
        let header_enc = encode_hex_header(&header);
        let payload = b"test.txt\000 12345 0 0\0";
        let subframe = crate::frame::encode_data_subframe(payload, DataEnd::AckEnd, CrcMode::Crc16);
        let mut input = header_enc.clone();
        input.extend_from_slice(&subframe);

        let mut d = Detector::new();
        let (pass, det) = d.feed(&input);
        // Both header and data subframe fully consumed → no passthrough.
        assert_eq!(pass, b"");
        // Both frames detected.
        assert_eq!(det.len(), 2);
        // First: ZFILE header.
        assert!(matches!(
            &det[0],
            Detection::Frame(ZmodemFrame::Header(h)) if h.frame_type == FrameType::ZFile
        ));
        // Second: data subframe with the filename payload.
        match &det[1] {
            Detection::Frame(ZmodemFrame::Data {
                payload: p, end, ..
            }) => {
                assert_eq!(p, payload);
                assert_eq!(*end, DataEnd::AckEnd);
            }
            other => panic!("expected data subframe, got {:?}", other),
        }
    }

    #[test]
    fn zfile_hex_header_then_data_subframe_in_separate_chunks() {
        // Same as above but the data subframe arrives in a separate feed()
        // call, as would happen if the bytes arrive in multiple reads.
        let header = make_header(FrameType::ZFile, [0, 0, 0, 0]);
        let header_enc = encode_hex_header(&header);
        let payload = b"doc.pdf\000 99999 0 0\0";
        let subframe = crate::frame::encode_data_subframe(payload, DataEnd::AckEnd, CrcMode::Crc16);

        let mut d = Detector::new();
        let (pass1, det1) = d.feed(&header_enc);
        assert_eq!(pass1, b"");
        assert_eq!(det1.len(), 1); // ZFILE header only
        let (pass2, det2) = d.feed(&subframe);
        assert_eq!(pass2, b"");
        assert_eq!(det2.len(), 1); // data subframe
        match &det2[0] {
            Detection::Frame(ZmodemFrame::Data { payload: p, .. }) => {
                assert_eq!(p, payload);
            }
            other => panic!("expected data subframe, got {:?}", other),
        }
    }

    #[test]
    fn zdata_header_followed_by_continue_subframe_then_end_subframe() {
        // Simulate ZDATA header + multiple data subframes (Continue then End).
        let header = make_header(FrameType::ZData, [0x10, 0x00, 0x00, 0x00]); // offset=16
        let header_enc = encode_hex_header(&header);
        let chunk1 = b"Hello, ";
        let chunk2 = b"World!";
        let sub1 = crate::frame::encode_data_subframe(chunk1, DataEnd::Continue, CrcMode::Crc16);
        let sub2 = crate::frame::encode_data_subframe(chunk2, DataEnd::End, CrcMode::Crc16);

        let mut input = header_enc;
        input.extend_from_slice(&sub1);
        input.extend_from_slice(&sub2);

        let mut d = Detector::new();
        let (pass, det) = d.feed(&input);
        assert_eq!(pass, b"");
        // 3 detections: ZDATA header + 2 data subframes.
        assert_eq!(det.len(), 3);
        assert!(matches!(
            &det[0],
            Detection::Frame(ZmodemFrame::Header(h)) if h.frame_type == FrameType::ZData
        ));
        match &det[1] {
            Detection::Frame(ZmodemFrame::Data { payload, end, .. }) => {
                assert_eq!(payload, chunk1);
                assert_eq!(*end, DataEnd::Continue);
            }
            other => panic!("expected first data subframe, got {:?}", other),
        }
        match &det[2] {
            Detection::Frame(ZmodemFrame::Data { payload, end, .. }) => {
                assert_eq!(payload, chunk2);
                assert_eq!(*end, DataEnd::End);
            }
            other => panic!("expected second data subframe, got {:?}", other),
        }
    }

    #[test]
    fn data_subframe_with_control_bytes_in_payload() {
        // Verify that ZDLE-escaped control bytes in the payload decode
        // correctly (e.g. NUL, CR, LF in the filename).
        let header = make_header(FrameType::ZFile, [0, 0, 0, 0]);
        let header_enc = encode_hex_header(&header);
        let payload = b"file\012name\0size 0 0\0"; // contains 0x01, 0x02
        let subframe = crate::frame::encode_data_subframe(payload, DataEnd::AckEnd, CrcMode::Crc16);
        let mut input = header_enc;
        input.extend_from_slice(&subframe);

        let mut d = Detector::new();
        let (_, det) = d.feed(&input);
        assert_eq!(det.len(), 2);
        match &det[1] {
            Detection::Frame(ZmodemFrame::Data { payload: p, .. }) => {
                assert_eq!(p, payload);
            }
            other => panic!("expected data subframe, got {:?}", other),
        }
    }
}
