use thiserror::Error;

/// Errors returned by the sync subsystem.
///
/// Deliberately does **not** expose ciphertext, keys, or backend credentials
/// in its `Display` output, so it is safe to log and surface to the UI.
#[derive(Debug, Error)]
pub enum SyncError {
    /// The local master key could not be obtained from `ConfigManager`.
    /// Usually means the user has not unlocked the config yet.
    #[error("master key unavailable — unlock the config first")]
    NoMasterKey,

    /// The remote blob exists but is not a RusTerm sync blob (wrong magic).
    /// Indicates either a fresh remote (nothing to pull) or a corrupted blob.
    #[error("remote blob is not a RusTerm sync blob (bad magic)")]
    BadMagic,

    /// The remote blob's format version is newer than this client understands.
    #[error("unsupported blob version: {0}")]
    UnsupportedVersion(u8),

    /// Decryption failed. Usually means the master password used locally does
    /// not match the one used to push the blob (different machine, rotated
    /// password, or wrong user).
    #[error("decryption failed — master password mismatch or corrupted blob")]
    DecryptFailed,

    /// The local config file (`settings.json`) does not exist, so there is
    /// nothing to push yet.
    #[error("local config file not found at {0}")]
    LocalConfigMissing(String),

    /// A backend-specific error (HTTP failure, filesystem error, etc.).
    /// The inner message is the backend's own description.
    #[error("backend error: {0}")]
    Backend(String),

    /// An IO error from a local-folder backend.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A serialization/deserialization error on the sync config or blob header.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// An HTTP-layer error from `reqwest`.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// A non-success HTTP response from a backend.
    #[error("unexpected http status {status} from {url}: {body}")]
    HttpStatus {
        status: u16,
        url: String,
        body: String,
    },
}

pub type Result<T> = std::result::Result<T, SyncError>;
