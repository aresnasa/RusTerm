//! Built-in backend implementations.
//!
//! Each backend implements [`EncryptedSyncBackend`](crate::backend::EncryptedSyncBackend)
//! and stores the opaque ciphertext blob produced by [`SyncManager`](crate::manager::SyncManager).
//! No backend ever sees the master key or plaintext.

pub mod gist;
pub mod http;
pub mod local_folder;

pub use gist::GistBackend;
pub use http::HttpBackend;
pub use local_folder::LocalFolderBackend;
