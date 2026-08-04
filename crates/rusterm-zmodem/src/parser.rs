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
use crate::frame::{CrcMode, FrameType, HeaderFrame, ZmodemFrame};
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
            // Outside a frame, count CAN runs for cancel detection.
            if self.state == ScanState::Idle && self.collector.kind.is_none() {
                if b == 0x18 {
                    self.can_run += 1;
                    if self.can_run >= 8 {
                        detections.push(Detection::Cancelled);
                        self.can_run = 0;
                        self.armed = false;
                        continue;
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
                    if let Some(detection) = self.try_complete(fmt) {
                        self.armed = true;
                        detections.push(detection);
                        self.state = ScanState::Idle;
                        self.collector = Collector::default();
                    }
                }
            }
        }

        (passthrough, detections)
    }

    fn begin_frame(&mut self, kind: FrameCollect, fmt: u8) {
        self.collector = Collector {
            buf: Vec::new(),
            kind: Some(kind),
        };
        self.state = ScanState::InFrame { fmt };
    }

    /// Attempt to parse a complete frame from the collector buffer. Returns
    /// `None` if more bytes are needed.
    fn try_complete(&self, fmt: u8) -> Option<Detection> {
        let kind = self.collector.kind?;
        let buf = &self.collector.buf;
        match kind {
            FrameCollect::Hex => Self::complete_hex(buf),
            FrameCollect::Bin16 => Self::complete_bin(buf, CrcMode::Crc16),
            FrameCollect::Bin32 => Self::complete_bin(buf, CrcMode::Crc32),
        }
        .inspect(|_| {
            let _ = fmt; // fmt unused for parse, kept for symmetry
        })
        .map(Detection::Frame)
    }

    fn complete_hex(buf: &[u8]) -> Option<ZmodemFrame> {
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
        Some(ZmodemFrame::Header(HeaderFrame {
            frame_type,
            data,
            crc_mode: CrcMode::Crc16,
        }))
    }

    fn complete_bin(buf: &[u8], crc_mode: CrcMode) -> Option<ZmodemFrame> {
        // Binary header: <type:1 ZDLE-esc> <4 data bytes ZDLE-esc> <crc>
        // CRC is 2 bytes (Crc16) or 4 bytes (Crc32).
        let crc_len = match crc_mode {
            CrcMode::Crc16 => 2,
            CrcMode::Crc32 => 4,
        };
        // Decode the whole buffer (no terminators in a header).
        let (decoded, consumed) = crate::frame::zdle_decode(buf, &[]);
        let needed = 1 + 4 + crc_len;
        if decoded.len() < needed {
            return None;
        }
        // Ensure we consumed the entire buffer (no trailing junk).
        if consumed != buf.len() {
            return None;
        }
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
        Some(ZmodemFrame::Header(HeaderFrame {
            frame_type,
            data,
            crc_mode,
        }))
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
    use crate::frame::{encode_bin_header, encode_bin32_header, encode_hex_header};

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
}
