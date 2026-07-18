//! Encrypted configuration synchronization for RusTerm.
//!
//! ## Security model
//!
//! This module synchronizes the user's [`ConfigManager`] config file
//! (`settings.json`) to cloud-backed encrypted stores while guaranteeing:
//!
//! - **Remote stores only ciphertext.** The entire `settings.json` blob is
//!   wrapped in a second AEAD envelope before being uploaded. Remotes never
//!   see hostnames, connection names, tags, expect regexes, or any other
//!   envelope metadata — only an opaque `magic | version | cipher_id |
//!   reserved | nonce ‖ ciphertext ‖ tag` blob.
//! - **The decryption key never leaves the local machine.** The key is the
//!   user's master key, derived locally from the master password via Argon2id
//!   (see [`rusterm_crypto::derive_key`]). It is never serialized, never
//!   uploaded, and never placed in any backend.
//! - **Equivalent security across backends.** Gist, iCloud, and arbitrary HTTP
//!   endpoints all receive byte-identical ciphertext produced by the same
//!   primitive (selected via [`CipherSpec`]), so the choice of backend does
//!   not change the threat model.
//! - **Only the local master password decrypts the cloud config.** The master
//!   key is Argon2id-derived from the master password; without it, the cloud
//!   blob is indistinguishable from random bytes. The choice of cipher
//!   (AES-256-GCM vs ChaCha20-Poly1305) does not change this — both consume
//!   the same 32-byte master key, and there is no escrow, no recovery path,
//!   and no way to weaken the encryption without weakening every backend.
//! - **Backend tokens never travel with the config.** By default, GitHub PATs
//!   and HTTP bearer tokens live in the OS keyring (macOS Keychain by default;
//!   see [`rusterm_crypto::KeyringStore`]) and are referenced from
//!   [`SyncConfig`] by `(service, account)`. An inline form exists for tests
//!   but is discouraged — see [`TokenSource`].
//!
//! ## Why two layers of encryption?
//!
//! `settings.json` already encrypts *sensitive fields* (passwords, key
//! passphrases, OneKey `send` values) with field-level AES-256-GCM. That layer
//! protects secrets if the local file is exfiltrated from disk next to the
//! binary.
//!
//! The sync layer adds a second, *transport* encryption around the **entire**
//! file. This protects envelope metadata (which hosts the user connects to,
//! connection group/tag taxonomy, expect regexes that may themselves encode
//! business context) from the cloud provider. The two layers serve different
//! threat models and compose without interfering.

pub mod backend;
pub mod backends;
pub mod config;
pub mod error;
pub mod manager;

pub use backend::{BackendKind, EncryptedSyncBackend};
pub use backends::{GistBackend, HttpBackend, LocalFolderBackend};
pub use config::{
    DEFAULT_ACCOUNT, GIST_TOKEN_SERVICE, GistConfig, HTTP_TOKEN_SERVICE, HttpConfig, KeychainRef,
    LocalFolderConfig, SyncConfig, SyncTarget, TokenSource,
};
pub use error::SyncError;
pub use manager::{SyncManager, TokenSlot};

// Re-export `CipherSpec` so callers can `use rusterm_sync::CipherSpec`
// without taking a direct dependency on `rusterm-crypto`.
pub use rusterm_crypto::CipherSpec;

/// Magic prefix written to every synced blob so we can evolve the format
/// without ambiguity. Currently always `RSTERM01` (8 ASCII bytes).
pub(crate) const BLOB_MAGIC: &[u8; 8] = b"RSTERM01";

/// On-wire format version. Bumped only when the header *layout* changes.
/// Repurposing a previously-zero byte (e.g. byte 9 became the cipher id) does
/// not bump the version — old blobs with `0x00` there decode transparently as
/// AES-256-GCM.
pub(crate) const BLOB_VERSION: u8 = 1;
