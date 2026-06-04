use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use zeroize::Zeroizing;
use rand::RngCore;

const NONCE_SIZE: usize = 12;

pub fn encrypt_data(
    key: &[u8; 32],
    plaintext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);

    Ok(result)
}

pub fn decrypt_data(
    key: &[u8; 32],
    data: &[u8],
) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if data.len() < NONCE_SIZE {
        anyhow::bail!("Data too short for nonce");
    }

    let (nonce_bytes, ciphertext) = data.split_at(NONCE_SIZE);
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| anyhow::anyhow!("Invalid key length: {:?}", e))?;
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

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
}
