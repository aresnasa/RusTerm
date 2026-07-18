//! The `EncryptedSyncBackend` trait and the `BackendKind` dispatch enum.
//!
//! A backend is anything that can store and retrieve a single opaque blob of
//! bytes addressed by a user-chosen name (the "blob key"). Backends must not
//! interpret the bytes — they are always ciphertext produced by the
//! [`SyncManager`](crate::manager::SyncManager).
//!
//! ## Trait contract
//!
//! - `push`: atomically replace the remote blob with `data`. If the backend
//!   does not support atomic replace, it should at least guarantee that a
//!   concurrent `pull` returns either the old or the new blob, never a
//!   truncated mixture.
//! - `pull`: return the current blob. If no blob exists yet (fresh remote),
//!   return `Ok(None)` so the caller can distinguish "empty remote" from
//!   "backend error".
//!
//! All backends receive identical ciphertext from the `SyncManager`, which
//! gives equivalent security regardless of backend.

use async_trait::async_trait;

use crate::error::Result;

/// A cloud (or local-folder) store that holds a single opaque ciphertext blob.
///
/// Implementors must not interpret the bytes — they are always
/// `magic ‖ version ‖ nonce ‖ ciphertext ‖ tag` produced by the sync layer.
#[async_trait]
pub trait EncryptedSyncBackend: Send + Sync {
    /// Human-readable backend name for logging (e.g. `"gist"`, `"icloud"`).
    /// Must not contain credentials.
    fn name(&self) -> &'static str;

    /// Replace the remote blob with `data`. Should be idempotent and atomic
    /// from the perspective of a concurrent `pull`.
    async fn push(&self, data: &[u8]) -> Result<()>;

    /// Fetch the current remote blob. Returns `Ok(None)` if the remote is
    /// empty (i.e. no push has happened yet).
    async fn pull(&self) -> Result<Option<Vec<u8>>>;
}

/// Tagged dispatch over the built-in backends.
///
/// Constructed from a user-supplied [`SyncConfig`](crate::config::SyncConfig)
/// via [`BackendKind::from_config`]. Each variant owns its own backend
/// implementation, so callers can match once and dispatch without generics.
pub enum BackendKind {
    Gist(GistBackend),
    LocalFolder(LocalFolderBackend),
    Http(HttpBackend),
}

// `BackendKind` itself implements `EncryptedSyncBackend` by delegating to the
// active variant. This lets callers hold a single `Box<dyn
// EncryptedSyncBackend>` without caring which backend was chosen.
use crate::backends::{GistBackend, HttpBackend, LocalFolderBackend};

#[async_trait]
impl EncryptedSyncBackend for BackendKind {
    fn name(&self) -> &'static str {
        match self {
            BackendKind::Gist(b) => b.name(),
            BackendKind::LocalFolder(b) => b.name(),
            BackendKind::Http(b) => b.name(),
        }
    }

    async fn push(&self, data: &[u8]) -> Result<()> {
        match self {
            BackendKind::Gist(b) => b.push(data).await,
            BackendKind::LocalFolder(b) => b.push(data).await,
            BackendKind::Http(b) => b.push(data).await,
        }
    }

    async fn pull(&self) -> Result<Option<Vec<u8>>> {
        match self {
            BackendKind::Gist(b) => b.pull().await,
            BackendKind::LocalFolder(b) => b.pull().await,
            BackendKind::Http(b) => b.pull().await,
        }
    }
}
