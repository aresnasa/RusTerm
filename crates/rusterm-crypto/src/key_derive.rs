use argon2::{Argon2, Algorithm, Version, Params};
use argon2::password_hash::{SaltString, PasswordHash, PasswordHasher, PasswordVerifier};

pub fn derive_key(password: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let params = Params::new(
        Params::DEFAULT_M_COST,
        Params::DEFAULT_T_COST,
        Params::DEFAULT_P_COST,
        Some(32),
    )
    .map_err(|e| anyhow::anyhow!("Params error: {}", e))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let salt = SaltString::encode_b64(salt)
        .map_err(|e| anyhow::anyhow!("Salt error: {}", e))?;

    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("Hash error: {}", e))?
        .hash
        .ok_or_else(|| anyhow::anyhow!("Hash computation failed"))?;

    let mut key = [0u8; 32];
    let hash_bytes = hash.as_bytes();
    let len = hash_bytes.len().min(32);
    key[..len].copy_from_slice(&hash_bytes[..len]);

    Ok(key)
}

pub fn verify_password(password: &str, hash: &str) -> anyhow::Result<bool> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| anyhow::anyhow!("Parse error: {}", e))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::default());
    Ok(argon2.verify_password(password.as_bytes(), &parsed).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_salt() -> Vec<u8> {
        // SaltString::encode_b64 needs at least 8 bytes of valid data
        vec![0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89]
    }

    fn make_salt_2() -> Vec<u8> {
        vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00]
    }

    #[test]
    fn test_derive_key_deterministic() {
        let salt = make_salt();
        let key1 = derive_key("password123", &salt).unwrap();
        let key2 = derive_key("password123", &salt).unwrap();
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_derive_key_different_passwords() {
        let salt = make_salt();
        let key1 = derive_key("password1", &salt).unwrap();
        let key2 = derive_key("password2", &salt).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_derive_key_different_salts() {
        let key1 = derive_key("password", &make_salt()).unwrap();
        let key2 = derive_key("password", &make_salt_2()).unwrap();
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_derive_key_produces_32_bytes() {
        let key = derive_key("test", &make_salt()).unwrap();
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_derive_key_not_all_zeros() {
        let key = derive_key("test", &make_salt()).unwrap();
        assert_ne!(key, [0u8; 32]);
    }
}
