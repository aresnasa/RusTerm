//! User-facing sync configuration.
//!
//! A `SyncConfig` describes *where* to sync, *how to authenticate* to the
//! chosen backend, and *which AEAD cipher* to wrap the blob in. It does
//! **not** contain the master key — the master key is always supplied at
//! runtime by the [`ConfigManager`] after the user unlocks their config with
//! their master password.
//!
//! ## Token storage
//!
//! Backend tokens (GitHub PATs, HTTP bearer tokens) are referenced via the
//! [`TokenSource`] enum. The recommended form is [`TokenSource::Keychain`],
//! which stores the secret in the OS keyring (macOS Keychain by default; see
//! [`rusterm_crypto::KeyringStore`]) and references it by `service` + `account`.
//! This keeps the secret out of `settings.json` and out of any cloud backup.
//!
//! For local development or tests, [`TokenSource::Inline`] embeds the secret
//! directly — this is **discouraged** for production because the token lands
//! in `settings.json` and would be synced to the cloud (still encrypted at
//! rest, but a token leak if the master password is ever compromised).
//!
//! For backward compatibility, an inline string in the JSON `"token"` field
//! still parses as `TokenSource::Inline`. Existing configs require no
//! migration.
//!
//! ## Cipher selection
//!
//! The `cipher` field selects the AEAD used to wrap the entire `settings.json`
//! before upload. The default is AES-256-GCM (the historical cipher, so old
//! blobs decode transparently). ChaCha20-Poly1305 is available as an
//! alternative — both take the same 32-byte master key, so the cipher choice
//! does not weaken (or strengthen) the key. The cipher actually used to
//! *decrypt* a pulled blob is read from the blob header (byte 9), so changing
//! this field only affects future pushes; old blobs always decrypt correctly.

use serde::{Deserialize, Serialize};

use rusterm_crypto::CipherSpec;

use crate::error::{Result, SyncError};

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

    /// AEAD cipher used to wrap the blob on push. Defaults to AES-256-GCM.
    ///
    /// Note: this only controls *pushes*. Pulled blobs are decrypted with
    /// whatever cipher they were originally pushed with (read from the blob
    /// header), so changing this field does not break reads of existing
    /// remote blobs.
    #[serde(default)]
    pub cipher: CipherSpec,
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
    /// GitHub PAT with gist scope. Defaults to a keychain reference under
    /// the `rusterm.sync.gist` service — see [`TokenSource`] for how to
    /// override with an inline token for local development.
    #[serde(default)]
    pub token: TokenSource,
    /// Optional `description` field for the gist. Defaults to "RusTerm Config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// If true, the gist is created as a secret gist (not listed publicly).
    /// Has no effect after creation — gists cannot be toggled between public
    /// and secret via the API.
    #[serde(default = "default_true")]
    pub secret: bool,
}

impl Default for GistConfig {
    fn default() -> Self {
        Self {
            gist_id: None,
            token: TokenSource::keychain(GIST_TOKEN_SERVICE, DEFAULT_ACCOUNT),
            description: None,
            secret: true,
        }
    }
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
    /// Defaults to `None` (no auth). When set, prefer the keychain form
    /// ([`TokenSource::Keychain`]) over inline for production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<TokenSource>,
}

/// Where a backend token comes from.
///
/// The default and recommended form is [`TokenSource::Keychain`]: the secret
/// lives in the OS keyring (macOS Keychain by default; see
/// [`rusterm_crypto::KeyringStore`]) and is referenced here by `service` +
/// `account`. This keeps the token out of `settings.json` and out of any
/// cloud backup.
///
/// [`TokenSource::Inline`] embeds the secret directly. This is convenient for
/// tests and quick local development, but is discouraged for production —
/// if the master password is ever compromised, the inline tokens are exposed
/// along with everything else in `settings.json`. With keychain storage, the
/// token stays opaque even to a user who unlocks the master password.
///
/// ## Serialization
///
/// The enum is `#[serde(untagged)]` so that an existing JSON value of
/// `"token": "ghp_xxx"` (a plain string) still parses as
/// `TokenSource::Inline("ghp_xxx")` without migration. A keychain reference
/// is written as `{"service": "...", "account": "..."}`.
///
/// ## Security
///
/// Resolving a keychain token calls into the OS keyring. If the keyring is
/// locked or the entry does not exist, [`resolve`](Self::resolve) returns an
/// error and the sync operation aborts before any network call is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenSource {
    /// Token is stored inline in the config. Discouraged for production.
    Inline(String),
    /// Token is stored in the OS keyring under `(service, account)`. The
    /// sync layer looks it up at sync time via
    /// [`rusterm_crypto::KeyringStore::get_credential_with`].
    Keychain(KeychainRef),
}

/// Reference to a keyring entry: a `(service, account)` pair. See
/// [`TokenSource::Keychain`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeychainRef {
    /// Keyring service name. Sync tokens conventionally live under
    /// `"rusterm.sync.<backend>"` (e.g. `"rusterm.sync.gist"`).
    #[serde(default = "default_gist_service")]
    pub service: String,
    /// Keyring account name. Defaults to `"default"`. Use distinct accounts
    /// (e.g. `"work"`, `"personal"`) to keep multiple tokens for the same
    /// backend in the same keyring.
    #[serde(default = "default_account")]
    pub account: String,
}

/// Default keychain service for Gist tokens. Used by `KeychainRef`'s serde
/// default and by [`SyncManager::store_token`](crate::SyncManager::store_token).
pub const GIST_TOKEN_SERVICE: &str = "rusterm.sync.gist";
/// Default keychain service for HTTP backend tokens.
pub const HTTP_TOKEN_SERVICE: &str = "rusterm.sync.http";
/// Default keychain account name.
pub const DEFAULT_ACCOUNT: &str = "default";

fn default_gist_service() -> String {
    GIST_TOKEN_SERVICE.to_string()
}

fn default_account() -> String {
    DEFAULT_ACCOUNT.to_string()
}

fn default_true() -> bool {
    true
}

impl TokenSource {
    /// Build a keychain token reference. Convenience constructor.
    pub fn keychain(service: impl Into<String>, account: impl Into<String>) -> Self {
        TokenSource::Keychain(KeychainRef {
            service: service.into(),
            account: account.into(),
        })
    }

    /// Build an inline token. Convenience constructor. Discouraged for
    /// production use — see the type-level docs.
    pub fn inline(token: impl Into<String>) -> Self {
        TokenSource::Inline(token.into())
    }

    /// Resolve to a concrete secret string.
    ///
    /// - For [`TokenSource::Inline`], returns the stored string directly.
    /// - For [`TokenSource::Keychain`], looks the entry up in the OS
    ///   keyring via [`rusterm_crypto::KeyringStore::get_credential_with`].
    ///   Returns an error if the keyring is locked, the entry does not
    ///   exist, or the platform keyring is unavailable.
    ///
    /// The returned string is held in plain `String` (not `Zeroizing`) for
    /// simplicity; callers that care about zeroization should re-wrap it
    /// themselves.
    pub fn resolve(&self) -> Result<String> {
        match self {
            TokenSource::Inline(s) => Ok(s.clone()),
            TokenSource::Keychain(kr) => {
                let secret =
                    rusterm_crypto::KeyringStore::get_credential_with(&kr.service, &kr.account)
                        .map_err(|e| {
                            SyncError::Backend(format!(
                                "keychain lookup failed for service={} account={}: {e}",
                                kr.service, kr.account
                            ))
                        })?;
                Ok(secret)
            }
        }
    }

    /// Return the inner string if this is an [`TokenSource::Inline`], else
    /// `None`. Useful for backends that want to assert the token has already
    /// been resolved (by [`BackendKind::from_config`]) without panicking.
    pub fn as_inline(&self) -> Option<&str> {
        match self {
            TokenSource::Inline(s) => Some(s.as_str()),
            TokenSource::Keychain(_) => None,
        }
    }

    /// Return the inner string, panicking if this is a [`TokenSource::Keychain`].
    ///
    /// This is intended for backends constructed via
    /// [`BackendKind::from_config`](crate::backend::BackendKind::from_config),
    /// which resolves keychain references to inline strings before building
    /// the backend. A panic here means a backend was constructed without
    /// going through `from_config` — that's a programmer error, not a
    /// runtime condition.
    pub fn expect_inline(&self) -> &str {
        self.as_inline().expect(
            "TokenSource was not resolved before backend construction; \
             use BackendKind::from_config to resolve keychain references",
        )
    }
}

impl Default for TokenSource {
    fn default() -> Self {
        TokenSource::keychain(GIST_TOKEN_SERVICE, DEFAULT_ACCOUNT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_source_inline_serde_round_trip() {
        let src = TokenSource::inline("ghp_abc");
        let json = serde_json::to_string(&src).unwrap();
        // Inline form serializes as a plain JSON string — backward compatible
        // with the historical `"token": "ghp_..."` config field.
        assert_eq!(json, "\"ghp_abc\"");
        let back: TokenSource = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TokenSource::Inline(s) if s == "ghp_abc"));
    }

    #[test]
    fn token_source_keychain_serde_round_trip() {
        let src = TokenSource::keychain("rusterm.sync.gist", "default");
        let json = serde_json::to_string(&src).unwrap();
        let back: TokenSource = serde_json::from_str(&json).unwrap();
        match back {
            TokenSource::Keychain(kr) => {
                assert_eq!(kr.service, "rusterm.sync.gist");
                assert_eq!(kr.account, "default");
            }
            _ => panic!("expected Keychain variant"),
        }
    }

    /// Old configs with `"token": "ghp_xxx"` (a plain string) must still
    /// parse as `TokenSource::Inline`. This is the key backward-compat
    /// guarantee — no migration needed when upgrading.
    #[test]
    fn legacy_inline_string_token_still_parses() {
        let json = "\"ghp_legacy_token\"";
        let src: TokenSource = serde_json::from_str(json).unwrap();
        assert!(matches!(src, TokenSource::Inline(s) if s == "ghp_legacy_token"));
    }

    /// A keychain reference without `service` / `account` fields still
    /// parses and uses the documented defaults.
    #[test]
    fn keychain_ref_uses_defaults_when_fields_absent() {
        let json = "{}";
        let kr: KeychainRef = serde_json::from_str(json).unwrap();
        assert_eq!(kr.service, GIST_TOKEN_SERVICE);
        assert_eq!(kr.account, DEFAULT_ACCOUNT);
    }

    /// `TokenSource::default()` is a keychain reference, never an inline
    /// string. This guarantees that newly-constructed configs never
    /// accidentally contain a plaintext token.
    #[test]
    fn default_is_keychain_not_inline() {
        let src = TokenSource::default();
        assert!(matches!(src, TokenSource::Keychain(_)));
    }

    #[test]
    fn gist_config_default_uses_keychain() {
        let cfg = GistConfig::default();
        assert!(matches!(cfg.token, TokenSource::Keychain(_)));
        assert!(cfg.secret);
        assert!(cfg.gist_id.is_none());
    }

    /// Full `SyncConfig` JSON round-trip with the new `cipher` field and a
    /// keychain token reference. Verifies the schema is stable.
    #[test]
    fn sync_config_serde_round_trip_with_cipher_and_keychain() {
        let cfg = SyncConfig {
            target: Some(SyncTarget::Gist(GistConfig {
                gist_id: Some("abc123".into()),
                token: TokenSource::keychain("rusterm.sync.gist", "default"),
                description: Some("RusTerm Config".into()),
                secret: true,
            })),
            blob_name: "rusterm-config.enc.bin".into(),
            cipher: CipherSpec::ChaCha20Poly1305,
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: SyncConfig = serde_json::from_str(&json).unwrap();
        assert!(back.is_enabled());
        assert_eq!(back.blob_name, cfg.blob_name);
        assert_eq!(back.cipher, CipherSpec::ChaCha20Poly1305);
        match back.target {
            Some(SyncTarget::Gist(g)) => {
                assert_eq!(g.gist_id.as_deref(), Some("abc123"));
                assert!(matches!(g.token, TokenSource::Keychain(_)));
            }
            _ => panic!("expected Gist target"),
        }
    }

    /// A `SyncConfig` with no `cipher` field still parses and defaults to
    /// AES-256-GCM. This is the backward-compat path for configs written
    /// before cipher selection existed.
    #[test]
    fn sync_config_without_cipher_defaults_to_aes() {
        let json = r#"{
            "target": {
                "kind": "local_folder",
                "path": "/tmp/rusterm"
            },
            "blob_name": "blob.enc"
        }"#;
        let cfg: SyncConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.cipher, CipherSpec::Aes256Gcm);
    }
}
