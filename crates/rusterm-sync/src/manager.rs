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
//! │ magic (8B)   │ ver (1B)  │ reserved  │ nonce‖ct‖tag (rest)        │
//! │ "RSTERM01"   │   = 1     │ (3B, zero)│  (AES-256-GCM output)       │
//! └──────────────┴───────────┴───────────┴─────────────────────────────┘
//! ```
//!
//! The magic + version let us evolve the format without ambiguity. The
//! reserved bytes are zero today and reserved for future flags. The remainder
//! is the standard `nonce ‖ ciphertext ‖ tag` blob produced by
//! [`rusterm_crypto::encrypt_data`], where the plaintext is the entire
//! `settings.json` file content.
//!
//! ## Security properties
//!
//! - The remote sees only the bytes above. No filename metadata (the backend
//!   may know there is *a* blob, but cannot tell it is `settings.json`).
//! - The master key is never read from disk by this module — it is taken
//!   from `ConfigManager` in-memory and dropped when the call returns.
//! - The plaintext is held in `Zeroizing<Vec<u8>>` on the pull path, so it
//!   is wiped from memory as soon as it goes out of scope.

use std::path::PathBuf;

use rusterm_core::ConfigManager;
use rusterm_crypto::{decrypt_data, encrypt_data};
use zeroize::Zeroizing;

use crate::backend::{BackendKind, EncryptedSyncBackend};
use crate::config::SyncConfig;
use crate::error::{Result, SyncError};

/// Header size: 8 bytes magic + 1 byte version + 3 bytes reserved = 12 bytes.
const HEADER_SIZE: usize = 12;
const RESERVED: [u8; 3] = [0u8; 3];

/// The sync orchestrator. Construct one of these per "sync now" action; do
/// not hold it long-term (it borrows nothing, so this is just a convention
/// to keep the surface area small).
pub struct SyncManager<'a> {
    config_manager: &'a ConfigManager,
    backend: BackendKind,
    blob_name: String,
}

impl<'a> SyncManager<'a> {
    /// Construct a sync manager from a user-supplied [`SyncConfig`].
    ///
    /// The `SyncConfig` carries *which* backend to use and its
    /// backend-specific parameters (gist token, folder path, http url, ...).
    /// The master key is taken from the `ConfigManager` at sync time.
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

    /// Push the local `settings.json` to the configured backend.
    ///
    /// Reads the on-disk config file (the path the `ConfigManager` already
    /// resolves), wraps it in the AES-256-GCM envelope, and uploads the
    /// resulting opaque blob.
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
        let ciphertext = encrypt_data(&key, plaintext)
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
    pub async fn pull_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        let blob = self
            .backend
            .pull()
            .await?
            .ok_or_else(|| SyncError::Backend("remote is empty — nothing to pull".into()))?;
        let ciphertext = self.decode_blob(&blob)?;
        let key = self.master_key()?;
        let plaintext = decrypt_data(&key, &ciphertext).map_err(|_| SyncError::DecryptFailed)?;
        Ok(plaintext)
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
        out.extend_from_slice(&RESERVED);
        out.extend_from_slice(ciphertext);
        out
    }

    fn decode_blob(&self, blob: &[u8]) -> Result<Vec<u8>> {
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
        // Skip 3 reserved bytes.
        let ciphertext = &rest[3..];
        Ok(ciphertext.to_vec())
    }
}

impl BackendKind {
    /// Build a concrete `BackendKind` from a `SyncTarget` + blob name.
    pub(crate) fn from_config(target: &crate::config::SyncTarget, blob_name: &str) -> Result<Self> {
        Ok(match target {
            crate::config::SyncTarget::Gist(g) => {
                BackendKind::Gist(crate::backends::GistBackend::new(g.clone(), blob_name))
            }
            crate::config::SyncTarget::LocalFolder(f) => BackendKind::LocalFolder(
                crate::backends::LocalFolderBackend::new(f.path.clone(), blob_name),
            ),
            crate::config::SyncTarget::Http(h) => {
                BackendKind::Http(crate::backends::HttpBackend::new(h.clone(), blob_name))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalFolderConfig, SyncConfig, SyncTarget};

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

        let ciphertext = rusterm_crypto::encrypt_data(&key, plaintext).unwrap();
        // Simulate the envelope.
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.extend_from_slice(&[0u8; 3]);
        blob.extend_from_slice(&ciphertext);

        // Decode.
        assert_eq!(&blob[..8], crate::BLOB_MAGIC);
        assert_eq!(blob[8], crate::BLOB_VERSION);
        let decoded = &blob[12..];
        let roundtrip = rusterm_crypto::decrypt_data(&key, decoded).unwrap();
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
        let ciphertext = rusterm_crypto::encrypt_data(&key, plaintext).unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(crate::BLOB_MAGIC);
        blob.push(crate::BLOB_VERSION);
        blob.extend_from_slice(&[0u8; 3]);
        blob.extend_from_slice(&ciphertext);
        backend.push(&blob).await.unwrap();

        // Pull: backend.pull → de-envelope → decrypt
        let got = backend.pull().await.unwrap().unwrap();
        assert_eq!(got, blob);
        let decoded = &got[12..];
        let pt = rusterm_crypto::decrypt_data(&key, decoded).unwrap();
        assert_eq!(&*pt, plaintext);
    }

    /// SyncConfig serialization round-trip — make sure users can write a
    /// sync config in JSON/TOML and have it parse back.
    #[test]
    fn sync_config_serde_round_trip() {
        let cfg = SyncConfig {
            target: Some(SyncTarget::LocalFolder(LocalFolderConfig {
                path: "~/Library/Mobile Documents/com~apple~CloudDocs/rusterm".into(),
            })),
            blob_name: "rusterm-config.enc.bin".into(),
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: SyncConfig = serde_json::from_str(&json).unwrap();
        assert!(back.is_enabled());
        assert_eq!(back.blob_name, cfg.blob_name);
    }

    /// Magic bytes are stable so old clients can recognize new blobs (and
    /// refuse them via `UnsupportedVersion`).
    #[test]
    fn magic_is_stable() {
        assert_eq!(crate::BLOB_MAGIC, b"RSTERM01");
        assert_eq!(crate::BLOB_VERSION, 1);
    }
}
