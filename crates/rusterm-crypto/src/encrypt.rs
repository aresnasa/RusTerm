use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use chacha20poly1305::ChaCha20Poly1305;
use rand::RngCore;
use zeroize::Zeroizing;

use crate::cipher::CipherSpec;

const NONCE_SIZE: usize = 12;

/// Encrypt `plaintext` with the AES-256-GCM primitive and a random 12-byte
/// nonce. Returns `nonce ‖ ciphertext ‖ tag` (the standard AEAD layout).
///
/// Use [`encrypt_with`] if you need to select the cipher at runtime.
pub fn encrypt_data(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    encrypt_with(key, plaintext, CipherSpec::Aes256Gcm)
}

/// Decrypt a blob produced by [`encrypt_data`]. The input must be at least
/// `NONCE_SIZE` bytes long and contain `nonce ‖ ciphertext ‖ tag`.
///
/// Use [`decrypt_with`] if the cipher was selected at runtime (e.g. read
/// from a sync blob header).
pub fn decrypt_data(key: &[u8; 32], data: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    decrypt_with(key, data, CipherSpec::Aes256Gcm)
}

/// Cipher-aware encrypt. Dispatches to the AEAD implementation selected by
/// `cipher`. The output layout (`nonce ‖ ct ‖ tag`, 12-byte nonce) is the
/// same for all supported ciphers so callers do not need to know which one
/// was used to produce the blob — only which one to use to decrypt it.
pub fn encrypt_with(
    key: &[u8; 32],
    plaintext: &[u8],
    cipher: CipherSpec,
) -> anyhow::Result<Vec<u8>> {
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = match cipher {
        CipherSpec::Aes256Gcm => {
            let c = Aes256Gcm::new_from_slice(key)
                .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
            c.encrypt(nonce, plaintext)
                .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?
        }
        CipherSpec::ChaCha20Poly1305 => {
            let c = ChaCha20Poly1305::new_from_slice(key)
                .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
            c.encrypt(nonce, plaintext)
                .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?
        }
    };

    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

/// Cipher-aware decrypt. Inverse of [`encrypt_with`]. The caller must know
/// which cipher was used to produce `data` (e.g. from a header byte) and pass
/// the matching `CipherSpec` — there is no way to auto-detect the cipher from
/// the ciphertext alone.
pub fn decrypt_with(
    key: &[u8; 32],
    data: &[u8],
    cipher: CipherSpec,
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if data.len() < NONCE_SIZE {
        anyhow::bail!("Data too short for nonce");
    }

    let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = match cipher {
        CipherSpec::Aes256Gcm => {
            let c = Aes256Gcm::new_from_slice(key)
                .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
            c.decrypt(nonce, ciphertext)
                .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?
        }
        CipherSpec::ChaCha20Poly1305 => {
            let c = ChaCha20Poly1305::new_from_slice(key)
                .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
            c.decrypt(nonce, ciphertext)
                .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?
        }
    };

    Ok(Zeroizing::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [42u8; 32];
        let plaintext = b"Hello, RusTerm! This is a secret message.";

        let encrypted = encrypt_data(&key, plaintext).unwrap();
        let decrypted = decrypt_data(&key, &encrypted).unwrap();

        assert_eq!(&*decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_produces_different_ciphertext() {
        let key = [42u8; 32];
        let plaintext = b"same message";

        let enc1 = encrypt_data(&key, plaintext).unwrap();
        let enc2 = encrypt_data(&key, plaintext).unwrap();

        // Nonce is random, so ciphertext should differ
        assert_ne!(enc1, enc2);
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let key1 = [42u8; 32];
        let key2 = [99u8; 32];
        let plaintext = b"secret data";

        let encrypted = encrypt_data(&key1, plaintext).unwrap();
        let result = decrypt_data(&key2, &encrypted);

        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_too_short_data_fails() {
        let key = [42u8; 32];
        let short_data = [0u8; 5];

        let result = decrypt_data(&key, &short_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypt_empty_plaintext() {
        let key = [42u8; 32];
        let plaintext = b"";

        let encrypted = encrypt_data(&key, plaintext).unwrap();
        let decrypted = decrypt_data(&key, &encrypted).unwrap();

        assert_eq!(&*decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_large_data() {
        let key = [42u8; 32];
        let plaintext = vec![0xABu8; 1_000_000];

        let encrypted = encrypt_data(&key, &plaintext).unwrap();
        let decrypted = decrypt_data(&key, &encrypted).unwrap();

        assert_eq!(&*decrypted, &plaintext);
    }

    // --- cipher-aware API ---

    #[test]
    fn chacha20_round_trip() {
        let key = [7u8; 32];
        let plaintext = b"cha cha cha";
        let enc = encrypt_with(&key, plaintext, CipherSpec::ChaCha20Poly1305).unwrap();
        let dec = decrypt_with(&key, &enc, CipherSpec::ChaCha20Poly1305).unwrap();
        assert_eq!(&*dec, plaintext);
    }

    #[test]
    fn cipher_dispatch_is_symmetric() {
        // Encrypting with one cipher and decrypting with another must fail.
        let key = [7u8; 32];
        let plaintext = b"never cross the streams";
        let aes = encrypt_with(&key, plaintext, CipherSpec::Aes256Gcm).unwrap();
        let chacha = encrypt_with(&key, plaintext, CipherSpec::ChaCha20Poly1305).unwrap();
        assert!(decrypt_with(&key, &aes, CipherSpec::ChaCha20Poly1305).is_err());
        assert!(decrypt_with(&key, &chacha, CipherSpec::Aes256Gcm).is_err());
    }

    #[test]
    fn legacy_encrypt_data_uses_aes_id_zero() {
        // `encrypt_data` is the historical API. Its output must decrypt under
        // AES-256-GCM (cipher id 0x00). This pins the legacy contract.
        let key = [7u8; 32];
        let enc = encrypt_data(&key, b"legacy").unwrap();
        let dec = decrypt_with(&key, &enc, CipherSpec::Aes256Gcm).unwrap();
        assert_eq!(&*dec, b"legacy");
    }
}
