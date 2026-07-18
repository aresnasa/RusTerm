//! Local-folder backend.
//!
//! Writes the ciphertext blob to a file inside a user-chosen folder. On
//! macOS, pointing this at `~/Library/Mobile Documents/com~apple~CloudDocs/`
//! (or a subdirectory) makes iCloud Drive sync the file automatically — no
//! API, no git, no auth needed.
//!
//! The same backend works for any folder-backed sync (Syncthing, Dropbox,
//! Google Drive desktop, etc.): from this crate's perspective it's just
//! `fs::write` and `fs::read`.
//!
//! ## Atomicity
//!
//! Writes go to a temp file in the same directory, then `rename`d into place.
//! On POSIX, `rename` is atomic, so a concurrent reader (or a sync agent that
//! triggers mid-write) will see either the old or the new blob, never a
//! partial mix.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::fs;

use crate::backend::EncryptedSyncBackend;
use crate::error::{Result, SyncError};

/// A backend that stores the blob as a single file in a local folder.
pub struct LocalFolderBackend {
    folder: PathBuf,
    blob_name: String,
}

impl LocalFolderBackend {
    /// Construct a new backend. The folder does not need to exist yet — it
    /// will be created on the first `push`.
    pub fn new(folder: impl Into<PathBuf>, blob_name: impl Into<String>) -> Self {
        Self {
            folder: folder.into(),
            blob_name: blob_name.into(),
        }
    }

    /// Expand a leading `~` in the folder path. We do this ourselves rather
    /// than depending on the `shellexpand` crate because the only expansion
    /// we need is `~` → home dir, which `dirs::home_dir` gives us directly.
    fn expand_tilde(path: &Path) -> Result<PathBuf> {
        let s = path.to_str().ok_or_else(|| {
            SyncError::Backend(format!("non-utf8 folder path: {}", path.display()))
        })?;
        if s == "~" {
            return dirs::home_dir()
                .ok_or_else(|| SyncError::Backend("could not resolve home directory".into()));
        }
        if let Some(rest) = s.strip_prefix("~/") {
            let home = dirs::home_dir()
                .ok_or_else(|| SyncError::Backend("could not resolve home directory".into()))?;
            return Ok(home.join(rest));
        }
        Ok(path.to_path_buf())
    }
}

#[async_trait]
impl EncryptedSyncBackend for LocalFolderBackend {
    fn name(&self) -> &'static str {
        "local_folder"
    }

    async fn push(&self, data: &[u8]) -> Result<()> {
        let folder = Self::expand_tilde(&self.folder)?;
        fs::create_dir_all(&folder).await?;

        let blob_path = folder.join(&self.blob_name);
        let tmp_path = blob_path.with_extension("enc.bin.tmp");

        // Write to a temp file in the same dir, then rename atomically.
        fs::write(&tmp_path, data).await?;
        fs::rename(&tmp_path, &blob_path).await?;

        tracing::debug!(
            folder = %folder.display(),
            blob = %self.blob_name,
            bytes = data.len(),
            "local_folder: pushed ciphertext blob"
        );
        Ok(())
    }

    async fn pull(&self) -> Result<Option<Vec<u8>>> {
        let folder = Self::expand_tilde(&self.folder)?;
        let blob_path = folder.join(&self.blob_name);

        if !blob_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&blob_path).await?;
        tracing::debug!(
            path = %blob_path.display(),
            bytes = bytes.len(),
            "local_folder: pulled ciphertext blob"
        );
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalFolderBackend::new(dir.path(), "blob.bin");

        // Pull from empty backend → None.
        assert!(backend.pull().await.unwrap().is_none());

        // Push then pull → same bytes.
        let payload = b"hello icloud";
        backend.push(payload).await.unwrap();
        let got = backend.pull().await.unwrap().unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn creates_missing_folder() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let backend = LocalFolderBackend::new(&nested, "blob.bin");

        backend.push(b"x").await.unwrap();
        assert!(nested.join("blob.bin").exists());
    }

    #[tokio::test]
    async fn overwrites_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let backend = LocalFolderBackend::new(dir.path(), "blob.bin");

        backend.push(b"first").await.unwrap();
        backend.push(b"second longer payload").await.unwrap();
        let got = backend.pull().await.unwrap().unwrap();
        assert_eq!(got, b"second longer payload");
    }
}
