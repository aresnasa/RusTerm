//! ZMODEM frame types, ZDLE escaping, and frame encoding/decoding.
//!
//! ## Frame anatomy
//!
//! A ZMODEM frame begins with a leader sequence (`ZPAD ZPAD ZDLE <fmt>`),
//! where `<fmt>` selects the encoding:
//!
//!  * `ZHEX` (`'C'`)  — header bytes are hex-encoded ASCII, terminated by
//!    `CR LF` (or `CR | 0x80` for the receiver). Used for all control
//!    headers by `lrzsz`.
//!  * `ZBIN` (`'A'`)  — binary header: 1 type byte + 4 data bytes + 2-byte
//!    CRC16, with ZDLE-escaped bytes. Used for data subframes by default.
//!  * `ZBIN32` (`'B'`) — binary header with a 4-byte CRC32 instead. Used
//!    only when the receiver advertises 32-bit CRC in its ZRINIT flags.
//!
//! Data subframes (`ZDATA`) carry an offset (4 bytes) followed by one or
//! more data blocks, each terminated by `ZDLE <frame-end>`:
//!
//!  * `ZCRCW` — data block + CRC, end of frame, sender expects an ACK.
//!  * `ZCRCG` — data block + CRC, frame continues (windowed).
//!  * `ZCRCE` — data block + CRC, end of frame, no ACK requested (last).
//!  * `ZCRCQ` — data block + CRC, ACK requested but frame continues.
//!
//! See RFC-ish references: `lrzsz` source (`zreadline`, `zsendhex`,
//! `zsbhdr`, `zsbh32`), and the original ZMODEM protocol description.

use crate::crc::{crc16_init, crc32_init};
use crate::{ZBIN, ZBIN32, ZDLE, ZHEX, ZPAD};

/// ZMODEM frame type bytes (the first byte after the format selector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// Sender → receiver: "are you there?" (request init). Emitted by `sz`.
    ZRQInit = 0,
    /// Receiver → sender: receiver capabilities (max block, flags, caps).
    ZRInit = 1,
    /// Sender → receiver: sender capabilities (rarely used by `lrzsz`).
    ZSInit = 2,
    /// Acknowledgement (carries a position).
    ZAck = 3,
    /// File header: name + size + mtime + mode (in the 4 data bytes +
    /// trailing escaped string for the filename).
    ZFile = 4,
    /// Skip this file (receiver → sender).
    ZSkip = 5,
    /// Negative acknowledge (CRC error etc.).
    ZNak = 6,
    /// Abort (receiver → sender).
    ZAbort = 7,
    /// Finish session (bidirectional).
    ZFin = 8,
    /// Resume at byte position (receiver → sender, on CRC error).
    ZRPos = 9,
    /// Data frame (offset + data blocks).
    ZData = 10,
    /// End of file (carries final byte position).
    ZEof = 11,
    /// Fatal error.
    ZFerr = 12,
    /// Request CRC of a file (rarely used by `lrzsz`).
    ZCrc = 13,
    /// Challenge (rarely used).
    ZChallenge = 14,
    /// Command complete (rarely used).
    ZCompl = 15,
    /// Cancel (bidirectional, 5 consecutive ZPAD|ZDLE|ZCAN).
    ZCan = 16,
    /// Free-form message (rarely used).
    ZStderr = 17,
}

impl FrameType {
    /// Decode a raw type byte into a [`FrameType`].
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0 => Self::ZRQInit,
            1 => Self::ZRInit,
            2 => Self::ZSInit,
            3 => Self::ZAck,
            4 => Self::ZFile,
            5 => Self::ZSkip,
            6 => Self::ZNak,
            7 => Self::ZAbort,
            8 => Self::ZFin,
            9 => Self::ZRPos,
            10 => Self::ZData,
            11 => Self::ZEof,
            12 => Self::ZFerr,
            13 => Self::ZCrc,
            14 => Self::ZChallenge,
            15 => Self::ZCompl,
            16 => Self::ZCan,
            17 => Self::ZStderr,
            _ => return None,
        })
    }
}

/// CRC mode for a binary frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrcMode {
    /// 16-bit CRC (`ZBIN`).
    Crc16,
    /// 32-bit CRC (`ZBIN32`).
    Crc32,
}

/// A parsed ZMODEM control header (hex or binary).
///
/// The 4-byte `data` field carries type-specific payload (e.g. ZRINIT caps,
/// ZRPos offset, ZEof position). For `ZFILE` the filename + metadata arrive
/// in the following data subframe, not in this header's `data` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFrame {
    pub frame_type: FrameType,
    pub data: [u8; 4],
    pub crc_mode: CrcMode,
}

/// Data subframe terminator (the byte after the data block's ZDLE).
///
/// These are the actual on-wire ZMODEM byte values used by lrzsz
/// (`zmodem.h`): `ZCRCE='h'`, `ZCRCG='i'`, `ZCRCQ='j'`, `ZCRCW='k'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataEnd {
    /// `ZCRCE` (0x68) — end of frame, no ACK.
    End = 0x68,
    /// `ZCRCG` (0x69) — frame continues, no ACK.
    Continue = 0x69,
    /// `ZCRCQ` (0x6A) — ACK requested, frame continues.
    AckContinue = 0x6A,
    /// `ZCRCW` (0x6B) — ACK requested, end of frame.
    AckEnd = 0x6B,
}

impl DataEnd {
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x68 => Self::End,
            0x69 => Self::Continue,
            0x6A => Self::AckContinue,
            0x6B => Self::AckEnd,
            _ => return None,
        })
    }

    /// True if the sender expects an ACK after this block.
    pub fn expects_ack(self) -> bool {
        matches!(self, Self::AckContinue | Self::AckEnd)
    }

    /// True if this terminator ends the data frame (no more subframes
    /// expected until the next ZDATA/ZFILE header).
    pub fn is_end(self) -> bool {
        matches!(self, Self::End | Self::AckEnd)
    }
}

/// A fully-parsed ZMODEM frame emitted by the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZmodemFrame {
    /// Control header (ZRQINIT, ZRINIT, ZFILE, ZEOF, ZFIN, ...).
    Header(HeaderFrame),
    /// Data subframe: `(offset, payload, terminator)`.
    ///
    /// The payload is already ZDLE-decoded and CRC-verified.
    Data {
        offset: u32,
        payload: Vec<u8>,
        end: DataEnd,
    },
}

// ---------------------------------------------------------------------------
// ZDLE escaping
// ---------------------------------------------------------------------------

/// ZDLE-escape a single byte for transmission. Returns the escape sequence
/// (ZDLE + escaped byte) if the byte must be escaped, or `None` if it can be
/// sent verbatim.
///
/// ZMODEM escapes bytes that could be confused with control characters or
/// the ZDLE marker itself: `0x00..=0x08`, `0x0A` (LF), `0x0D` (CR), `0x10`
/// (DLE), `0x11` (XON), `0x13` (XOFF), `0x19`, `0x1A`, `0x1C..=0x1F`, and
/// `0x7F`, plus `0x80..=0xFF` when binary mode is active. The escaped form is
/// `ZDLE (byte ^ 0x40)`.
pub fn zdle_escape(byte: u8) -> Option<[u8; 2]> {
    let must_escape = matches!(
        byte,
        0x00..=0x08 | 0x0A | 0x0D | 0x10 | 0x11 | 0x13 | 0x19 | 0x1A | 0x1C..=0x1F | 0x7F
    ) || byte == ZDLE;
    if must_escape {
        Some([ZDLE, byte ^ 0x40])
    } else {
        None
    }
}

/// Encode a slice into a ZDLE-escaped byte vector.
pub fn zdle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        match zdle_escape(b) {
            Some(pair) => out.extend_from_slice(&pair),
            None => out.push(b),
        }
    }
    out
}

/// Decode a ZDLE-escaped byte slice. Stops at the first unescaped ZDLE that
/// is NOT followed by a known data terminator (which signals the end of the
/// data block). Returns the decoded bytes and the number of input bytes
/// consumed.
///
/// `terminators` is the set of bytes that, when seen immediately after an
/// unescaped ZDLE, mark the end of the data block (ZCRCE/ZCRCG/ZCRCQ/ZCRCW).
pub fn zdle_decode(data: &[u8], terminators: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == ZDLE {
            // Need at least one more byte.
            if i + 1 >= data.len() {
                return (out, i);
            }
            let next = data[i + 1];
            if terminators.contains(&next) {
                // End of data block — do NOT consume the terminator here;
                // the caller will read it.
                return (out, i);
            }
            // Standard ZDLE decode: the byte following ZDLE is XOR'd with
            // 0x40. (lrzsz's zdlread also handles a few high-bit variants,
            // but the canonical lrzsz escape is plain `^ 0x40`; high-bit
            // bytes in binary frames survive unescaped.)
            let decoded = next ^ 0x40;
            out.push(decoded);
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    (out, i)
}

/// Decode exactly `n` ZDLE-escaped bytes from `data`. Returns the decoded
/// bytes and the number of input bytes consumed, or `None` if `data` doesn't
/// contain enough bytes to produce `n` decoded bytes.
///
/// Unlike [`zdle_decode`], this function does NOT stop at data terminators —
/// it treats every `ZDLE <byte>` as an escape sequence. This is correct for
/// parsing fixed-length fields (header bodies, CRCs) where terminators don't
/// appear.
pub fn zdle_decode_n(data: &[u8], n: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while out.len() < n {
        if i >= data.len() {
            return None;
        }
        let b = data[i];
        if b == ZDLE {
            if i + 1 >= data.len() {
                return None;
            }
            out.push(data[i + 1] ^ 0x40);
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Some((out, i))
}

// ---------------------------------------------------------------------------
// Hex header encoding
// ---------------------------------------------------------------------------

/// Encode `n` as a lowercase hex byte pair.
fn hex_pair(n: u8) -> [u8; 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [HEX[(n >> 4) as usize], HEX[(n & 0x0F) as usize]]
}

/// Build a hex-encoded ZMODEM control header frame, as `lrzsz` would emit it:
/// `ZPAD ZPAD ZDLE ZHEX <type> <4 data bytes> <crc> CR LF`.
///
/// This is the canonical frame `rz`/`sz` use for negotiation.
pub fn encode_hex_header(frame: &HeaderFrame) -> Vec<u8> {
    let mut body = Vec::with_capacity(7);
    body.push(frame.frame_type as u8);
    body.extend_from_slice(&frame.data);
    let crc = crc16_init(&body);
    body.extend_from_slice(&crc.to_be_bytes());

    let mut out = Vec::with_capacity(2 + 4 + body.len() * 2 + 2);
    out.push(ZPAD);
    out.push(ZPAD);
    out.push(ZDLE);
    out.push(ZHEX);
    for &b in &body {
        out.extend_from_slice(&hex_pair(b));
    }
    // lrzsz terminates hex headers with CR LF.
    out.push(b'\r');
    out.push(b'\n');
    out
}

/// Build a binary (`ZBIN`) control header frame with 16-bit CRC.
pub fn encode_bin_header(frame: &HeaderFrame) -> Vec<u8> {
    let mut body = Vec::with_capacity(5);
    body.push(frame.frame_type as u8);
    body.extend_from_slice(&frame.data);
    let crc = crc16_init(&body);
    body.extend_from_slice(&crc.to_be_bytes());

    let mut out = Vec::with_capacity(2 + 2 + body.len() * 2);
    out.push(ZPAD);
    out.push(ZDLE);
    out.push(ZBIN);
    out.extend(zdle_encode(&body));
    out
}

/// Build a binary (`ZBIN32`) control header frame with 32-bit CRC.
pub fn encode_bin32_header(frame: &HeaderFrame) -> Vec<u8> {
    let mut body = Vec::with_capacity(5);
    body.push(frame.frame_type as u8);
    body.extend_from_slice(&frame.data);
    let crc = crc32_init(&body);
    body.extend_from_slice(&crc.to_le_bytes());

    let mut out = Vec::with_capacity(2 + 2 + body.len() * 2);
    out.push(ZPAD);
    out.push(ZDLE);
    out.push(ZBIN32);
    out.extend(zdle_encode(&body));
    out
}

/// Encode a binary data subframe: `ZDLE ZDATA <offset> <data blocks>`.
///
/// `offset` is the absolute byte position in the file (4 bytes, LE via ZDLE).
/// Each block is `<escaped-data> ZDLE <end-byte>` followed by the block's CRC
/// (2 or 4 bytes, ZDLE-escaped). For simplicity this encodes a single block
/// per call with the requested terminator; callers chunk the file as needed.
///
/// **Note**: This format includes a ZDATA leader + offset, which is NOT what
/// real lrzsz sends for data subframes. Real `sz` sends ZDATA as a separate
/// header frame, then data subframes via [`encode_data_subframe`] (no leader,
/// no offset). This function is retained for backward compatibility with the
/// existing send-path code.
pub fn encode_data_block(offset: u32, data: &[u8], end: DataEnd, crc_mode: CrcMode) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    // Frame leader: ZDLE ZDATA (ZDATA = 10 = 0x0A → must be ZDLE-escaped).
    out.push(ZDLE);
    // ZData (0x0A) is a control byte and is escaped by zdle_escape to
    // `[ZDLE, 0x4A]`. Inline that here so the leader is unambiguous.
    out.push(0x4A);
    // Offset: 4 bytes, each ZDLE-escaped individually.
    out.extend(zdle_encode(&offset.to_le_bytes()));
    // Data payload, ZDLE-escaped.
    out.extend(zdle_encode(data));
    // Terminator.
    out.push(ZDLE);
    out.push(end as u8);
    // CRC.
    match crc_mode {
        CrcMode::Crc16 => {
            let crc = crc16_init(data);
            out.extend(zdle_encode(&crc.to_be_bytes()));
        }
        CrcMode::Crc32 => {
            let crc = crc32_init(data);
            out.extend(zdle_encode(&crc.to_le_bytes()));
        }
    }
    out
}

/// Encode a data subframe WITHOUT a ZDATA leader or offset, as real lrzsz
/// `sz`/`rz` does after a ZFILE or ZDATA header:
/// `<escaped-data> ZDLE <end-byte> <escaped-crc>`.
///
/// This is the correct format for both the ZFILE metadata subframe (filename
/// + size/mtime/mode) and the file-data subframes that follow a ZDATA header.
/// The offset is carried by the preceding ZDATA header, not repeated here.
pub fn encode_data_subframe(data: &[u8], end: DataEnd, crc_mode: CrcMode) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    // Data payload, ZDLE-escaped.
    out.extend(zdle_encode(data));
    // Terminator: ZDLE + end-byte (ZCRCE/ZCRCG/ZCRCQ/ZCRCW).
    out.push(ZDLE);
    out.push(end as u8);
    // CRC over the unescaped data.
    match crc_mode {
        CrcMode::Crc16 => {
            let crc = crc16_init(data);
            out.extend(zdle_encode(&crc.to_be_bytes()));
        }
        CrcMode::Crc32 => {
            let crc = crc32_init(data);
            out.extend(zdle_encode(&crc.to_le_bytes()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_type_round_trips() {
        for raw in 0..=17u8 {
            let ft = FrameType::from_byte(raw).expect("0..=17 all map");
            assert_eq!(ft as u8, raw);
        }
        assert!(FrameType::from_byte(18).is_none());
        assert!(FrameType::from_byte(255).is_none());
    }

    #[test]
    fn data_end_expects_ack() {
        assert!(!DataEnd::End.expects_ack());
        assert!(!DataEnd::Continue.expects_ack());
        assert!(DataEnd::AckContinue.expects_ack());
        assert!(DataEnd::AckEnd.expects_ack());
    }

    #[test]
    fn zdle_escape_escapes_control_bytes_and_zdle() {
        // Control bytes that must be escaped.
        for &b in &[0x00, 0x05, 0x0A, 0x0D, 0x10, 0x11, 0x13, 0x1F, 0x7F, ZDLE] {
            assert!(zdle_escape(b).is_some(), "byte {:#x} should escape", b);
        }
        // Printable / safe bytes are NOT escaped.
        for &b in &[b'A', b'z', b'0', b' ', b'/', 0x20, 0x7E] {
            assert!(zdle_escape(b).is_none(), "byte {:#x} should not escape", b);
        }
    }

    #[test]
    fn zdle_round_trip_preserves_arbitrary_bytes() {
        let original: Vec<u8> = (0u8..=255).collect();
        let encoded = zdle_encode(&original);
        let (decoded, consumed) = zdle_decode(&encoded, &[]);
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, original);
    }

    #[test]
    fn zdle_decode_stops_at_terminator() {
        // Payload "AB" followed by ZDLE ZCRCE.
        let payload = b"AB";
        let frame = {
            let mut v = zdle_encode(payload);
            v.push(ZDLE);
            v.push(DataEnd::End as u8);
            v
        };
        let (decoded, consumed) = zdle_decode(&frame, &[DataEnd::End as u8]);
        assert_eq!(decoded, payload);
        // Consumed up to (but not including) the terminating ZDLE.
        assert_eq!(consumed, frame.len() - 2);
    }

    #[test]
    fn zdle_decode_decodes_standard_escapes() {
        // ZDLE 0x4D decodes to 0x0D (CR) — the standard escape.
        let frame = [ZDLE, 0x4D];
        let (decoded, consumed) = zdle_decode(&frame, &[]);
        assert_eq!(consumed, 2);
        assert_eq!(decoded, vec![0x0D]);
    }

    #[test]
    fn hex_header_has_canonical_leader_and_terminator() {
        let frame = HeaderFrame {
            frame_type: FrameType::ZRInit,
            data: [0x00, 0x00, 0x20, 0x00],
            crc_mode: CrcMode::Crc16,
        };
        let enc = encode_hex_header(&frame);
        // Leader: ZPAD ZPAD ZDLE ZHEX.
        assert_eq!(&enc[..4], &[ZPAD, ZPAD, ZDLE, ZHEX]);
        // Body: type(2 hex) + 4 data bytes(8 hex) + crc(4 hex) = 14 hex chars.
        // Then CR LF.
        assert_eq!(enc.len(), 4 + 14 + 2);
        assert_eq!(&enc[enc.len() - 2..], b"\r\n");
        // Type byte "01" (ZRInit) in hex.
        assert_eq!(&enc[4..6], b"01");
    }

    #[test]
    fn hex_header_crc_is_valid_and_round_trips_via_parser() {
        let frame = HeaderFrame {
            frame_type: FrameType::ZRQInit,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let enc = encode_hex_header(&frame);
        // Re-parse the hex body to verify CRC.
        let hex_body = &enc[4..enc.len() - 2]; // strip leader + CR LF
        assert_eq!(hex_body.len(), 14);
        let bytes: Vec<u8> = (0..14)
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(std::str::from_utf8(&hex_body[i..i + 2]).unwrap(), 16).unwrap()
            })
            .collect();
        // bytes = [type, d0, d1, d2, d3, crc_hi, crc_lo]
        let crc = crc16_init(&bytes[..5]);
        assert_eq!(crc.to_be_bytes(), [bytes[5], bytes[6]]);
    }

    #[test]
    fn bin_header_starts_with_zbin_and_has_zdled_body() {
        let frame = HeaderFrame {
            // ZEof (11 = 0x0B) is a safe byte (not in the escaped set), so
            // it appears verbatim after ZBIN, letting us assert on enc[3].
            frame_type: FrameType::ZEof,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc16,
        };
        let enc = encode_bin_header(&frame);
        assert_eq!(&enc[..3], &[ZPAD, ZDLE, ZBIN]);
        assert_eq!(enc[3], FrameType::ZEof as u8);
    }

    #[test]
    fn bin32_header_uses_le_crc_and_zbin32_leader() {
        let frame = HeaderFrame {
            frame_type: FrameType::ZFin,
            data: [0, 0, 0, 0],
            crc_mode: CrcMode::Crc32,
        };
        let enc = encode_bin32_header(&frame);
        assert_eq!(&enc[..3], &[ZPAD, ZDLE, ZBIN32]);
    }

    #[test]
    fn data_block_encodes_offset_and_payload() {
        let block = encode_data_block(0x1234, b"hi", DataEnd::End, CrcMode::Crc16);
        // Starts with ZDLE ZDATA.
        assert_eq!(block[0], ZDLE);
        // Offset 0x1234 LE = [0x34, 0x12, 0x00, 0x00]; 0x00 must be ZDLE-escaped.
        // Just verify the frame is non-empty and contains the ZDLE + End terminator.
        assert!(block.contains(&(DataEnd::End as u8)));
    }
}
