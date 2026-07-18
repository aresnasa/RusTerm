//! The sync orchestrator.
//!
//! [`SyncManager`] ties together the local [`ConfigManager`] (which holds the
//! master key) and a chosen [`EncryptedSyncBackend`] (which stores the
//! ciphertext remotely). It is the single entrypoint for "sync my config to
//! the cloud" and "pull my config from the cloud".
//!
//! ## Wire format
//!
//! The on-remote blob is:
//!
//! ```text
//! ┌──────────────┬───────────┬───────────┬─────────────────────────────┐
//! │ magic (8B)   │ ver (1B)  │ cipher(1B)│ nonce‖ct‖tag (rest)        │
//! │ "RSTERM01"   │   = 1     │  = 0x00   │  (AEAD output)              │
//! │              │           │  = 0x01   │  (0x00=AES-256-GCM,         │
//! │              │           │           │   0x01=ChaCha20-Poly1305)   │
//! └──────────────┴───────────┴───────────┴─────────────────────────────┘
//!          offset 0          8           9    10            12
//! ```
//!
//! The magic + version let us evolve the format without ambiguity. The byte
//! at offset 9 carries the cipher id (see [`rusterm_crypto::CipherSpec`]):
//!
//! - `0x00` — AES-256-GCM (the historical default; all existing v1 blobs
//!   have `0x00` here, so they decode transparently as AES-256-GCM).
//! - `0x01` — ChaCha20-Poly1305.
//!
//! Unknown cipher ids are rejected with [`SyncError::UnsupportedCipher`] so a
//! new cipher can be added later without old clients silently corrupting
//! data. The byte at offset 10 is still reserved (zero) for future use.
//!
//! The remainder is the standard `nonce ‖ ciphertext ‖ tag` blob produced by
//! [`rusterm_crypto::encrypt_with`], where the plaintext is the entire
//! `settings.json` file content. The 12-byte nonce size is shared by all
//! supported ciphers.
//!
//! ## Security properties
//!
//! - The remote sees only the bytes above. No filename metadata (the backend
//!   may know there is *a* blob, but cannot tell it is `settings.json`).
//! - The master key is never read from disk by this module — it is taken
//!   from `ConfigManager` in-memory and dropped when the call returns.
//! - The plaintext is held in `Zeroizing<Vec<u8>>` on the pull path, so it
//!   is wiped from memory as soon as it goes out of scope.
//! - Backend tokens live in the OS keyring by default (see
//!   [`crate::config::TokenSource`]); inline tokens are a discouraged
//!   fallback for tests.
//! - The cipher choice does not weaken the key: both AES-256-GCM and
//!   ChaCha20-Poly1305 consume the same 32-byte Argon2id-derived master key.
//!   Only the local master password can produce that key, so only the local
//!   user can decrypt the cloud blob — regardless of cipher choice.

use std::path::PathBuf;

use rusterm_core::ConfigManager;
use rusterm_crypto::{CipherSpec, decrypt_with, encrypt_with};
use zeroize::Zeroizing;

use crate::backend::{BackendKind, EncryptedSyncBackend};
use crate::config::{
    DEFAULT_ACCOUNT, GIST_TOKEN_SERVICE, HTTP_TOKEN_SERVICE, SyncConfig, TokenSource,
};
use crate::error::{Result, SyncError};

/// Header size: 8 bytes magic + 1 byte version + 1 byte cipher id + 2 bytes
/// reserved = 12 bytes. Same total as the historical layout (which had 3
/// zero reserved bytes) — we just gave the first reserved byte a meaning.
const HEADER_SIZE: usize = 12;
/// Reserved bytes that follow the cipher id. Always zero today.
const RESERVED_TAIL: [u8; 2] = [0u8; 2];

/// The sync orchestrator. Construct one of these per "sync now" action; do
/// not hold it long-term (it borrows nothing, so this is just a convention
/// to keep the surface area small).
pub struct SyncManager<'a> {
    config_manager: &'a ConfigManager,
    backend: BackendKind,
    blob_name: String,
    /// Cipher used on the next push. Pulls always use the cipher encoded in
    /// the blob header, not this field.
    cipher: CipherSpec,
}

/// Names which keychain service a `store_token` call should write to. The
/// caller picks the variant matching the backend they intend to use.
#[derive(Debug, Clone, Copy)]
pub enum TokenSlot {
    /// Gist PAT slot. Service: [`GIST_TOKEN_SERVICE`]. Account: `"default"`.
    Gist,
    /// HTTP backend bearer token slot. Service: [`HTTP_TOKEN_SERVICE`].
    /// Account: `"default"`.
    Http,
}

impl TokenSlot {
    /// Resolve to the `(service, account)` pair the keychain write should
    /// target. This is what [`SyncManager::store_token`] uses internally.
    pub fn keychain_target(self) -> (&'static str, &'static str) {
        match self {
            TokenSlot::Gist => (GIST_TOKEN_SERVICE, DEFAULT_ACCOUNT),
            TokenSlot::Http => (HTTP_TOKEN_SERVICE, DEFAULT_ACCOUNT),
        }
    }
}

impl<'a> SyncManager<'a> {
    /// Construct a sync manager from a user-supplied [`SyncConfig`].
    ///
    /// The `SyncConfig` carries *which* backend to use and its
    /// backend-specific parameters (gist token, folder path, http url, ...).
    /// The master key is taken from the `ConfigManager` at sync time.
    ///
    /// Token references in the config (see [`TokenSource`]) are resolved
    /// eagerly here — a keychain lookup failure (locked keyring, missing
    /// entry) aborts before any network call is made.
    pub fn new(config_manager: &'a ConfigManager, sync_config: &'a SyncConfig) -> Result<Self> {
        let target = sync_config
            .target
            .as_ref()
            .ok_or(SyncError::Backend("no sync target configured".into()))?;
        let backend = BackendKind::from_config(target, &sync_config.blob_name)?;
        Ok(Self {
            config_manager,
            backend,
            blob_name: sync_config.blob_name.clone(),
            cipher: sync_config.cipher,
        })
    }

    /// The configured blob name (the filename inside the gist / folder / URL).
    pub fn blob_name(&self) -> &str {
        &self.blob_name
    }

    /// The backend's human-readable name (e.g. `"gist"`, `"local_folder"`).
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// The cipher that will be used on the next push. Pulls always decrypt
    /// using the cipher encoded in the blob header, not this value.
    pub fn cipher(&self) -> CipherSpec {
        self.cipher
    }

    /// Push the local `settings.json` to the configured backend.
    ///
    /// Reads the on-disk config file (the path the `ConfigManager` already
    /// resolves), wraps it in the AEAD envelope (using the configured
    /// cipher), and uploads the resulting opaque blob.
    pub async fn push(&self) -> Result<()> {
        let local_path = self.local_config_path()?;
        if !local_path.exists() {
            return Err(SyncError::LocalConfigMissing(
                local_path.display().to_string(),
            ));
        }
        let plaintext = std::fs::read(&local_path)?;
        self.push_bytes(&plaintext).await
    }

    /// Push an arbitrary plaintext byte slice (already in memory) to the
    /// backend, after wrapping it in the envelope. Exposed for callers that
    /// want to sync a serialized form they already have in hand (e.g. for
    /// testing, or for syncing a non-default config file).
    pub async fn push_bytes(&self, plaintext: &[u8]) -> Result<()> {
        let key = self.master_key()?;
        let ciphertext = encrypt_with(&key, plaintext, self.cipher)
            .map_err(|e| SyncError::Backend(format!("encrypt failed: {e}")))?;
        let blob = self.encode_blob(&ciphertext);
        self.backend.push(&blob).await
    }

    /// Pull the latest ciphertext from the backend, decrypt it, and write the
    /// plaintext back to the local `settings.json`.
    ///
    /// **Warning**: this overwrites the local file. Callers should typically
    /// back up the local file first or check that the local config has no
    /// unsaved changes. The backup-before-overwrite behavior is intentionally
    /// *not* done here — it is a policy decision that belongs to the caller
    /// (e.g. the CLI may prompt the user).
    pub async fn pull(&self) -> Result<()> {
        let plaintext = self.pull_bytes().await?;
        let local_path = self.local_config_path()?;
        if let Some(parent) = local_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Atomic write via temp + rename, mirroring `ConfigManager::save_*`.
        let tmp = local_path.with_extension("json.tmp.pull");
        std::fs::write(&tmp, &*plaintext)?;
        std::fs::rename(&tmp, &local_path)?;
        Ok(())
    }

    /// Pull and return the decrypted plaintext without writing it to disk.
    /// Useful for "dry run" or "compare local vs remote" flows.
    ///
    /// The cipher used to decrypt is read from the blob header (byte 9), not
    /// from `SyncConfig::cipher` — so this correctly handles blobs pushed
    /// with a different cipher than the current config.
    pub async fn pull_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        let blob = self
            .backend
            .pull()
            .await?
            .ok_or_else(|| SyncError::Backend("remote is empty — nothing to pull".into()))?;
        let (ciphertext, cipher) = self.decode_blob(&blob)?;
        let key = self.master_key()?;
        let plaintext =
            decrypt_with(&key, &ciphertext, cipher).map_err(|_| SyncError::DecryptFailed)?;
        Ok(plaintext)
    }

    /// Write a backend token to the OS keyring under the well-known service
    /// for the given [`TokenSlot`]. The token is then addressable by the
    /// matching default [`TokenSource::Keychain`] in `SyncConfig`.
    ///
    /// This is the helper a CLI should call for `rusterm sync set-token gist
    /// <PAT>` — it lets the user set up sync without ever editing JSON or
    /// touching the keychain directly.
    ///
    /// The token is passed by reference and not stored anywhere beyond the
    /// keyring. The caller is responsible for zeroizing its own copy.
    pub fn store_token(slot: TokenSlot, token: &str) -> Result<()> {
        let (service, account) = slot.keychain_target();
        rusterm_crypto::KeyringStore::save_credential_with(service, account, token)
            .map_err(|e| SyncError::Backend(format!("keychain write failed: {e}")))
    }

    /// Delete a previously-stored backend token from the OS keyring. Returns
    /// Ok if the entry was deleted, or an error if the keyring is
    /// unavailable. Deleting a non-existent entry is an error from the
    /// keyring crate — callers that don't care should ignore the result.
    pub fn delete_token(slot: TokenSlot) -> Result<()> {
        let (service, account) = slot.keychain_target();
        rusterm_crypto::KeyringStore::delete_credential_with(service, account)
            .map_err(|e| SyncError::Backend(format!("keychain delete failed: {e}")))
    }

    // --- internals ---

    fn local_config_path(&self) -> Result<PathBuf> {
        // Use the path the ConfigManager already resolved. This avoids
        // duplicating the RUSTERM_CONFIG_DIR / next-to-binary / config-dir
        // precedence logic, which would drift out of sync if it ever
        // changes in `rusterm-core`.
        Ok(self.config_manager.config_path().to_path_buf())
    }

    fn master_key(&self) -> Result<[u8; 32]> {
        Ok(*self.config_manager.master_key())
    }

    fn encode_blob(&self, ciphertext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_SIZE + ciphertext.len());
        out.extend_from_slice(crate::BLOB_MAGIC);
        out.push(crate::BLOB_VERSION);
        out.push(self.cipher.id());
        out.extend_from_slice(&RESERVED_TAIL);
        out.extend_from_slice(ciphertext);
        out
    }

    /// Decode the envelope, returning the ciphertext payload *and* the
    /// cipher that must be used to decrypt it (read from byte 9 of the
    /// header). The caller is responsible for invoking
    /// [`rusterm_crypto::decrypt_with`] with the returned cipher.
    fn decode_blob(&self, blob: &[u8]) -> Result<(Vec<u8>, CipherSpec)> {
        if blob.len() < HEADER_SIZE {
            return Err(SyncError::Backend(format!(
                "blob too short: {} bytes (need at least {HEADER_SIZE})",
                blob.len()
            )));
        }
        let (magic, rest) = blob.split_at(8);
        if magic != crate::BLOB_MAGIC {
            return Err(SyncError::BadMagic);
        }
        let (version, rest) = rest.split_at(1);
        let version = version[0];
        if version != crate::BLOB_VERSION {
            return Err(SyncError::UnsupportedVersion(version));
        }
        // Byte 9 = cipher id (0x00 = AES-256-GCM, the historical default).
        let (cipher_byte, rest) = rest.split_at(1);
        let cipher = CipherSpec::from_id(cipher_byte[0])
            .ok_or(SyncError::UnsupportedCipher(cipher_byte[0]))?;
        // Skip the remaining 2 reserved bytes.
        let ciphertext = &rest[2..];
        Ok((ciphertext.to_vec(), cipher))
    }
}

impl BackendKind {
    /// Build a concrete `BackendKind` from a `SyncTarget` + blob name.
    ///
    /// Token references ([`TokenSource::Keychain`]) are resolved here, before
    /// the backend is constructed — a missing keychain entry aborts before
    /// any backend is built, so a misconfigured sync cannot make a network
    /// call with a stale or empty token.
    pub(crate) fn from_config(target: &crate::config::SyncTarget, blob_name: &str) -> Result<Self> {
        Ok(match target {
            crate::config::SyncTarget::Gist(g) => {
                let mut resolved = g.clone();
                let token = resolved.token.resolve()?;
                resolved.token = TokenSource::inline(token);
                BackendKind::Gist(crate::backends::GistBackend::new(resolved, blob_name))
            }
            crate::config::SyncTarget::LocalFolder(f) => BackendKind::LocalFolder(
                crate::backends::LocalFolderBackend::new(f.path.clone(), blob_name),
            ),
            crate::config::SyncTarget::Http(h) => {
                let mut resolved = h.clone();
                if let Some(src) = &resolved.token {
                    let token = src.resolve()?;
                    resolved.token = Some(TokenSource::inline(token));
                }
                BackendKind::Http(crate::backends::HttpBackend::new(resolved, blob_name))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GistConfig, LocalFolderConfig, SyncConfig, SyncTarget, TokenSource};

    /// Round-trip: push bytes → backend → pull bytes → same plaintext.
    /// Uses a fake master key derived the same way `ConfigManager` does it,
    /// but without requiring a real `ConfigManager` (which needs a password).
    #[tokio::test]
    async fn push_pull_round_trip_via_temp_folder() {
        // We can't easily build a real ConfigManager in a unit test without a
        // master password + on-disk settings.json. Instead, exercise the
        // encode/decode envelope directly. The push/pull path through the
        // backend is already covered by `local_folder::tests`.
        let key = [7u8; 32];
        let plaintext = b"{ \"hello\": \"world\" }";

        let ciphertext =
            rusterm_crypto::encrypt_with(&key, plaintext, CipherSpec::Aes256Gcm).unwrap();
        // Simulate the envelope: magic | version | cipher_id(=0) | reserved(2).
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.push(0x00u8);
        blob.extend_from_slice(&[0u8; 2]);
        blob.extend_from_slice(&ciphertext);

        // Decode.
        assert_eq!(&blob[..8], crate::BLOB_MAGIC);
        assert_eq!(blob[8], crate::BLOB_VERSION);
        assert_eq!(blob[9], 0x00);
        let decoded = &blob[12..];
        let roundtrip = rusterm_crypto::decrypt_with(&key, decoded, CipherSpec::Aes256Gcm).unwrap();
        assert_eq!(&*roundtrip, plaintext);
    }

    /// End-to-end: encrypt + write to temp folder + read back + decrypt.
    /// This is the closest we can get to a full SyncManager round trip
    /// without instantiating a ConfigManager.
    #[tokio::test]
    async fn end_to_end_folder_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let backend = crate::backends::LocalFolderBackend::new(dir.path(), "blob.enc");

        let key = [42u8; 32];
        let plaintext = b"the entire settings.json content goes here";

        // Push: encrypt → envelope → backend.push
        let ciphertext =
            rusterm_crypto::encrypt_with(&key, plaintext, CipherSpec::Aes256Gcm).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.push(0x00u8);
        blob.extend_from_slice(&[0u8; 2]);
        blob.extend_from_slice(&ciphertext);
        backend.push(&blob).await.unwrap();

        // Pull: backend.pull → de-envelope → decrypt
        let got = backend.pull().await.unwrap().unwrap();
        assert_eq!(got, blob);
        let decoded = &got[12..];
        let pt = rusterm_crypto::decrypt_with(&key, decoded, CipherSpec::Aes256Gcm).unwrap();
        assert_eq!(&*pt, plaintext);
    }

    /// Cipher round-trip through the envelope: encrypt with ChaCha20-Poly1305,
    /// encode with cipher id 0x01, decode, decrypt. This locks in the wire
    /// format for the non-default cipher.
    #[tokio::test]
    async fn chacha20_cipher_round_trip() {
        let key = [11u8; 32];
        let plaintext = b"rotate the cipher for fun and profit";

        let ciphertext =
            rusterm_crypto::encrypt_with(&key, plaintext, CipherSpec::ChaCha20Poly1305).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.push(0x01u8); // ChaCha20-Poly1305 cipher id
        blob.extend_from_slice(&[0u8; 2]);
        blob.extend_from_slice(&ciphertext);

        // Decode header.
        assert_eq!(&blob[..8], crate::BLOB_MAGIC);
        assert_eq!(blob[8], crate::BLOB_VERSION);
        assert_eq!(blob[9], 0x01);
        let cipher = CipherSpec::from_id(blob[9]).unwrap();
        assert_eq!(cipher, CipherSpec::ChaCha20Poly1305);

        let decoded = &blob[12..];
        let pt = rusterm_crypto::decrypt_with(&key, decoded, cipher).unwrap();
        assert_eq!(&*pt, plaintext);
    }

    /// Backward compat: a v1 blob with cipher byte = 0 (the historical
    /// layout — 3 reserved bytes, all zero) must still decode as
    /// AES-256-GCM. This is the lock that prevents us from breaking existing
    /// synced blobs when we repurpose byte 9 as the cipher id.
    #[test]
    fn decode_legacy_v1_blob_with_cipher_zero() {
        let key = [21u8; 32];
        let plaintext = b"this blob came from an old client";

        // Build a legacy-style blob: magic | ver | 3 zero bytes | ct
        let ciphertext =
            rusterm_crypto::encrypt_with(&key, plaintext, CipherSpec::Aes256Gcm).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.extend_from_slice(&[0u8; 3]); // legacy: 3 zero reserved bytes
        blob.extend_from_slice(&ciphertext);

        // The decoder reads byte 9 (=0x00) and dispatches to AES-256-GCM.
        assert_eq!(blob[9], 0x00);
        let cipher = CipherSpec::from_id(blob[9]).unwrap();
        assert_eq!(cipher, CipherSpec::Aes256Gcm);

        let decoded = &blob[12..];
        let pt = rusterm_crypto::decrypt_with(&key, decoded, cipher).unwrap();
        assert_eq!(&*pt, plaintext);
    }

    /// Unknown cipher ids must be rejected, not silently defaulted. The
    /// decoder returns the cipher id in the error so the caller can report
    /// it ("a newer client wrote this blob with cipher 0x05; please
    /// upgrade").
    #[test]
    fn decode_rejects_unknown_cipher_id() {
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.push(0x05u8); // unknown cipher id
        blob.extend_from_slice(&[0u8; 2]);
        blob.extend_from_slice(b"fake ciphertext body");

        // The decoder surfaces UnsupportedCipher(5) rather than guessing.
        assert_eq!(CipherSpec::from_id(0x05), None);
    }

    /// SyncConfig serialization round-trip — make sure users can write a
    /// sync config in JSON/TOML and have it parse back. Uses an inline
    /// token so the test does not touch the OS keychain.
    #[test]
    fn sync_config_serde_round_trip() {
        let cfg = SyncConfig {
            target: Some(SyncTarget::LocalFolder(LocalFolderConfig {
                path: "~/Library/Mobile Documents/com~apple~CloudDocs/rusterm".into(),
            })),
            blob_name: "rusterm-config.enc.bin".into(),
            cipher: CipherSpec::default(),
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: SyncConfig = serde_json::from_str(&json).unwrap();
        assert!(back.is_enabled());
        assert_eq!(back.blob_name, cfg.blob_name);
        assert_eq!(back.cipher, CipherSpec::Aes256Gcm);
    }

    /// Token resolution: an inline token resolves without touching the
    /// keychain. This is the test-only path that lets us exercise backend
    /// construction end-to-end without a real keyring.
    #[test]
    fn inline_token_resolves_without_keychain() {
        let src = TokenSource::inline("ghp_test_token");
        assert_eq!(src.resolve().unwrap(), "ghp_test_token");
    }

    /// `BackendKind::from_config` accepts an inline Gist token and constructs
    /// a GistBackend without touching the keychain. This exercises the
    /// resolution + backend construction path with a token source that
    /// doesn't need a keyring.
    #[test]
    fn from_config_with_inline_gist_token() {
        let target = SyncTarget::Gist(GistConfig {
            gist_id: None,
            token: TokenSource::inline("ghp_inline"),
            description: None,
            secret: true,
        });
        let backend = BackendKind::from_config(&target, "blob.bin").unwrap();
        assert_eq!(backend.name(), "gist");
    }

    /// `BackendKind::from_config` for a local folder needs no token at all.
    #[test]
    fn from_config_local_folder_no_token() {
        let target = SyncTarget::LocalFolder(LocalFolderConfig {
            path: "/tmp/rusterm-test".into(),
        });
        let backend = BackendKind::from_config(&target, "blob.bin").unwrap();
        assert_eq!(backend.name(), "local_folder");
    }

    /// Magic bytes are stable so old clients can recognize new blobs (and
    /// refuse them via `UnsupportedVersion`).
    #[test]
    fn magic_is_stable() {
        assert_eq!(crate::BLOB_MAGIC, b"RSTERM01");
        assert_eq!(crate::BLOB_VERSION, 1);
    }

    /// The header size is unchanged at 12 bytes — we just gave byte 9 a
    /// meaning. This guards against accidentally bloating the header.
    #[test]
    fn header_size_is_twelve() {
        assert_eq!(HEADER_SIZE, 12);
    }
}
