//! Encrypted configuration synchronization for RusTerm.
//!
//! ## Security model
//!
//! This module synchronizes the user's [`ConfigManager`] config file
//! (`settings.json`) to cloud-backed encrypted stores while guaranteeing:
//!
//! - **Remote stores only ciphertext.** The entire `settings.json` blob is
//!   wrapped in a second layer of AES-256-GCM before being uploaded. Remotes
//!   never see hostnames, connection names, tags, expect regexes, or any other
//!   envelope metadata — only an opaque `nonce ‖ ciphertext ‖ tag` blob.
//! - **The decryption key never leaves the local machine.** The key is the
//!   user's master key, derived locally from the master password via Argon2id
//!   (see [`rusterm_crypto::derive_key`]). It is never serialized, never
//!   uploaded, and never placed in any backend.
//! - **Equivalent security across backends.** Gist, iCloud, and arbitrary HTTP
//!   endpoints all receive byte-identical ciphertext produced by the same
//!   primitive, so the choice of backend does not change the threat model.
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
pub use config::{GistConfig, HttpConfig, LocalFolderConfig, SyncConfig, SyncTarget};
pub use error::SyncError;
pub use manager::SyncManager;

/// Magic prefix written to every synced blob so we can evolve the format
/// without ambiguity. Currently always `RSTERM01` (8 ASCII bytes).
pub(crate) const BLOB_MAGIC: &[u8; 8] = b"RSTERM01";

/// On-wire format version. Bumped only when the header layout changes.
pub(crate) const BLOB_VERSION: u8 = 1;
