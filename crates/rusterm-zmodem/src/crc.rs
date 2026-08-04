//! CRC-16 and CRC-32 implementations used by the ZMODEM protocol.
//!
//! ZMODEM uses CRC-16/ACORN (also called "CRC-CCITT (XMODEM)", polynomial
//! `0x1021`, init `0x0000`, no reflection) for the default 16-bit CRC
//! headers and data subframes. When the sender opts into 32-bit CRC
//! (`ZBIN32`), it uses CRC-32/ISO-HDLC (polynomial `0xEDB88320`, init
//! `0xFFFFFFFF`, reflected, final XOR `0xFFFFFFFF`) — the same variant as
//! Ethernet/PNG/gzip.

/// CRC-16/ACORN lookup table (poly 0x1021, no reflection).
static CRC16_TABLE: [u16; 256] = build_crc16_table();

const fn build_crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Compute CRC-16/ACORN over `data`, continuing from `init`.
pub fn crc16(init: u16, data: &[u8]) -> u16 {
    let mut crc = init;
    for &byte in data {
        // table index = high byte XOR input byte
        let idx = ((crc >> 8) as u8) ^ byte;
        crc = (crc << 8) ^ CRC16_TABLE[idx as usize];
    }
    crc
}

/// Compute CRC-16/ACORN from scratch over `data` (init = 0).
pub fn crc16_init(data: &[u8]) -> u16 {
    crc16(0, data)
}

/// CRC-32/ISO-HDLC lookup table (poly 0xEDB88320, reflected).
static CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Compute CRC-32/ISO-HDLC over `data`, continuing from `init`.
///
/// Note: callers are responsible for the `init = 0xFFFFFFFF` and final XOR
/// `0xFFFFFFFF` wrapping; this function performs the raw polynomial update
/// so it can be composed incrementally across data subframes.
pub fn crc32(init: u32, data: &[u8]) -> u32 {
    let mut crc = init;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc
}

/// Compute CRC-32/ISO-HDLC from scratch (init + final XOR).
pub fn crc32_init(data: &[u8]) -> u32 {
    crc32(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vectors for CRC-16/ACORN (poly 0x1021, init 0).
    #[test]
    fn crc16_known_vectors() {
        assert_eq!(crc16_init(b""), 0x0000);
        // CRC-16/XMODEM (init 0, poly 0x1021) over "123456789" = 0x31C3.
        // This is the canonical XMODEM/CRC test vector (init=0 variant).
        assert_eq!(crc16_init(b"123456789"), 0x31C3);
        // Single byte 'A' (0x41): table[0x41] = 0x58E5 (verified).
        assert_eq!(crc16_init(b"A"), 0x58E5);
    }

    #[test]
    fn crc16_incremental_matches_one_shot() {
        let data = b"hello zmodem world";
        let one = crc16_init(data);
        let inc = crc16(crc16(crc16(0, &data[..6]), &data[6..12]), &data[12..]);
        assert_eq!(one, inc);
    }

    /// CRC-32/ISO-HDLC over "123456789" → 0xCBF43926 (canonical test vector).
    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32_init(b""), 0x0000_0000);
        assert_eq!(crc32_init(b"123456789"), 0xCBF4_3926);
        // Same vector as gzip/Ethernet FCS for the ASCII digits.
        assert_eq!(
            crc32_init(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    #[test]
    fn crc32_incremental_matches_one_shot() {
        let data = b"zmodem binary data block payload";
        let one = crc32_init(data);
        let inc = crc32(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF;
        assert_eq!(one, inc);
        // Multi-chunk incremental must match.
        let mut acc = 0xFFFF_FFFFu32;
        for chunk in data.chunks(7) {
            acc = crc32(acc, chunk);
        }
        assert_eq!(acc ^ 0xFFFF_FFFF, one);
    }
}
