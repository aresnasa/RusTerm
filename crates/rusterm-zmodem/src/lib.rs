//! ZMODEM protocol parser and state machine for RusTerm.
//!
//! Interoperates with the system-installed `lrzsz` `rz`/`sz` programs running
//! on the remote side of an SSH (or local shell) session. RusTerm does NOT
//! shell out to a local `lrzsz` binary; instead it implements enough of the
//! ZMODEM protocol in pure Rust to:
//!
//!  * detect the start-of-transfer sequences emitted by `rz`/`sz`,
//!  * negotiate the file name/size,
//!  * receive (`sz` on the remote) or send (`rz` on the remote) the file
//!    payload with CRC validation,
//!  * and tear down the session cleanly.
//!
//! The protocol implementation is intentionally minimal: it targets the
//! framing subset that `lrzsz` actually uses (hex header + binary data with
//! ZDLE escaping, 16-bit CRC by default, 32-bit CRC fallback). Full ZMODEM
//! feature parity (ZSINIT attr/escctl, crash recovery windows, streaming
//! ZCRC) is out of scope.
//!
//! All protocol code is pure and unit-testable; the UI/PTY integration lives
//! in `rusterm-ui`.

#![forbid(unsafe_code)]

pub mod crc;
pub mod frame;
pub mod parser;
pub mod session;

pub use frame::{FrameType, HeaderFrame, ZmodemFrame};
pub use parser::{Detection, Detector};
pub use session::{Direction, SessionEvent, ZmodemSession};

/// The standard ZMODEM attention prefix emitted by `lrzsz` before a transfer
/// begins: `**\x1B\x0D` (ZPAD ZPAD ESC CR). `rz`/`sz` send this to grab the
/// receiver's attention and resynchronise the line before the first hex
/// header frame.
///
/// Detecting this sequence alone is NOT sufficient — it can appear in
/// terminal output by coincidence. The [`Detector`] waits for the full
/// `**\x18\x43` (ZPAD ZPAD ZDLE ZHEX) header start before arming.
pub const ATTENTION_PREFIX: &[u8] = b"**\x1b\x0d";

/// ZDLE (data-link escape). Bytes following ZDLE in a binary frame are XOR'd
/// with `0x40` (see [`frame::zdle_encode`]/[`frame::zdle_decode`]).
pub const ZDLE: u8 = 0x18;
/// ZPAD — frame leading pad byte (`'*'`).
pub const ZPAD: u8 = b'*';
/// ZBIN — binary header (4 data bytes + 2-byte CRC16).
pub const ZBIN: u8 = b'A';
/// ZBIN32 — binary header with 32-bit CRC (4 data bytes + 4-byte CRC32).
pub const ZBIN32: u8 = b'B';
/// ZHEX — hexadecimal header (4 data bytes + 2-byte CRC16, all hex-encoded).
pub const ZHEX: u8 = b'C';
