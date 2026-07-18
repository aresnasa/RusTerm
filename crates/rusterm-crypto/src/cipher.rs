//! Cipher selection for AEAD encryption.
//!
//! [`CipherSpec`] lets callers choose between supported AEAD ciphers. All
//! supported ciphers take a 32-byte key (so they can share the Argon2id-derived
//! master key from [`crate::key_derive`]) and a 12-byte nonce, and emit a
//! `nonce ‖ ciphertext ‖ tag` blob consumable by the matching decrypt call.
//!
//! ## Wire encoding
//!
//! [`CipherSpec::id`] returns a single-byte identifier suitable for embedding
//! in a header byte. [`CipherSpec::from_id`] is the inverse. The id `0x00` is
//! always AES-256-GCM, the historical default — old blobs that predate cipher
//! selection always had `0x00` in what is now the cipher-id byte, so they
//! decode transparently as AES-256-GCM.

use serde::{Deserialize, Serialize};

/// Supported AEAD ciphers. Add new variants here and extend [`id`]/[`from_id`]
/// accordingly. Wire-format consumers must reject unknown ids rather than
/// guessing a default.
///
/// [`id`]: CipherSpec::id
/// [`from_id`]: CipherSpec::from_id
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CipherSpec {
    /// AES-256-GCM. The default. Backward compatible with all historical
    /// RusTerm blobs — both `settings.json` field-level encryption and the
    /// sync envelope use this cipher today.
    #[default]
    Aes256Gcm,

    /// ChaCha20-Poly1305. Provides an alternative AEAD construction with the
    /// same key/nonce sizes as AES-256-GCM. Useful on platforms without AES-NI
    /// acceleration, or as a defence-in-depth choice for users who want to
    /// avoid AES-specific cryptanalysis if any ever emerges.
    ChaCha20Poly1305,
}

impl CipherSpec {
    /// Single-byte wire identifier. Stable across releases — never reassign.
    /// `0x00` is permanently AES-256-GCM so legacy blobs keep decoding.
    pub const fn id(self) -> u8 {
        match self {
            CipherSpec::Aes256Gcm => 0x00,
            CipherSpec::ChaCha20Poly1305 => 0x01,
        }
    }

    /// Inverse of [`id`](Self::id). Returns `None` for unknown ids so callers
    /// can surface an "unsupported cipher" error instead of silently guessing.
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            0x00 => Some(CipherSpec::Aes256Gcm),
            0x01 => Some(CipherSpec::ChaCha20Poly1305),
            _ => None,
        }
    }

    /// Human-readable name suitable for logging — never contains secrets.
    pub const fn name(self) -> &'static str {
        match self {
            CipherSpec::Aes256Gcm => "aes-256-gcm",
            CipherSpec::ChaCha20Poly1305 => "chacha20-poly1305",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trip() {
        for spec in [CipherSpec::Aes256Gcm, CipherSpec::ChaCha20Poly1305] {
            assert_eq!(CipherSpec::from_id(spec.id()), Some(spec));
        }
    }

    #[test]
    fn aes_is_id_zero_for_legacy_compat() {
        // Legacy blobs always had 0x00 in the byte we now use as cipher id.
        // That must decode as AES-256-GCM forever.
        assert_eq!(CipherSpec::from_id(0x00), Some(CipherSpec::Aes256Gcm));
        assert_eq!(CipherSpec::default(), CipherSpec::Aes256Gcm);
    }

    #[test]
    fn unknown_id_is_none() {
        assert_eq!(CipherSpec::from_id(0x02), None);
        assert_eq!(CipherSpec::from_id(0xFF), None);
    }

    #[test]
    fn serde_round_trip() {
        for spec in [CipherSpec::Aes256Gcm, CipherSpec::ChaCha20Poly1305] {
            let s = serde_json::to_string(&spec).unwrap();
            let back: CipherSpec = serde_json::from_str(&s).unwrap();
            assert_eq!(spec, back);
        }
    }

    #[test]
    fn default_is_aes() {
        assert_eq!(CipherSpec::default(), CipherSpec::Aes256Gcm);
    }
}
