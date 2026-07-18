//! User-facing sync configuration.
//!
//! A `SyncConfig` describes *where* to sync and *how to authenticate* to the
//! chosen backend. It does **not** contain the master key — the master key is
//! always supplied at runtime by the [`ConfigManager`] after the user unlocks
//! their config with their master password.
//!
//! `SyncConfig` is intended to be serialized alongside (or inside) the user's
//! existing `settings.json`. It carries no secrets beyond a GitHub PAT and an
//! optional HTTP bearer token; both should be stored in the OS keyring (see
//! [`rusterm_crypto::KeyringStore`]) in production, with only the *name* of
//! the keyring entry held here. To keep the MVP simple, we allow the tokens to
//! be inlined in the config, but this should be migrated to keyring references
//! before shipping.

use serde::{Deserialize, Serialize};

/// Top-level sync configuration. Optional — if absent, sync is disabled.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    /// Which backend to use. If `None`, sync is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SyncTarget>,

    /// Name of the blob as seen by the backend. For Gist this is the filename
    /// inside the gist; for LocalFolder this is the file name; for Http this
    /// is appended to the URL path. Defaults to `"rusterm-config.enc.bin"`.
    #[serde(default = "default_blob_name")]
    pub blob_name: String,
}

fn default_blob_name() -> String {
    "rusterm-config.enc.bin".to_string()
}

impl SyncConfig {
    /// True if sync is configured (i.e. a target is set).
    pub fn is_enabled(&self) -> bool {
        self.target.is_some()
    }
}

/// Tagged union of supported backends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncTarget {
    /// GitHub Gist via the REST API. Requires a PAT with gist scope.
    Gist(GistConfig),

    /// A local folder. On macOS, point this at `~/Library/Mobile
    /// Documents/com~apple~CloudDocs/...` to sync via iCloud Drive. On other
    /// platforms, any local folder (including ones backed by Syncthing,
    /// Dropbox, etc.) works the same way.
    LocalFolder(LocalFolderConfig),

    /// A generic HTTP endpoint that accepts `PUT` (to upload) and `GET` (to
    /// download) the raw ciphertext bytes. Useful for self-hosted cloud
    /// storage that exposes a single file via HTTP.
    Http(HttpConfig),
}

/// GitHub Gist configuration.
///
/// The PAT is used as a Bearer token. It needs the `gist` scope (classic PAT)
/// or read/write access to gists (fine-grained PAT).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GistConfig {
    /// Gist ID (the hash in the gist URL). If the gist does not exist yet,
    /// leave this as `None` and the first `push` will create it; the created
    /// ID will be returned and the caller is responsible for persisting it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gist_id: Option<String>,
    /// GitHub Personal Access Token with gist scope. For production use,
    /// prefer storing this in the OS keyring and referencing it by name.
    pub token: String,
    /// Optional `description` field for the gist. Defaults to "RusTerm Config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// If true, the gist is created as a secret gist (not listed publicly).
    /// Has no effect after creation — gists cannot be toggled between public
    /// and secret via the API.
    #[serde(default = "default_true")]
    pub secret: bool,
}

fn default_true() -> bool {
    true
}

/// Local folder configuration. The folder is created on push if missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFolderConfig {
    /// Absolute path to the folder. On macOS, an iCloud-backed path looks
    /// like `~/Library/Mobile Documents/com~apple~CloudDocs/rusterm`.
    pub path: String,
}

/// Generic HTTP endpoint configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Base URL. The blob name is appended as the last path segment on push
    /// and pull, so the URL should not normally end with `/`. The backend
    /// will issue `PUT {base}/{blob_name}` and `GET {base}/{blob_name}`.
    pub url: String,
    /// Optional bearer token. Sent as `Authorization: Bearer {token}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}
