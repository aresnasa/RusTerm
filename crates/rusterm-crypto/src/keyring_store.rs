//! OS keyring wrapper.
//!
//! [`KeyringStore`] is a thin facade over the `keyring` crate that namespaces
//! RusTerm's credentials. Two APIs are provided:
//!
//! - The original single-name API (`save_credential`, `get_credential`,
//!   `delete_credential`) uses a fixed service of `"RusTerm"` and an
//!   arbitrary name. This is what `ConfigManager` uses for the master key
//!   (`"rusterm-master-key"`).
//! - The `with_service` API takes an explicit `service` parameter so callers
//!   like the sync layer can namespace their tokens (e.g. `"rusterm.sync.gist"`
//!   vs `"rusterm.sync.http"`) without colliding with the master key entry.
//!
//! Both APIs use the same underlying keyring; only the service/account
//! partitioning differs.
//!
//! ## Testing
//!
//! The `keyring` crate's built-in mock backend uses `EntryOnly` persistence,
//! which means each `Entry::new` call returns a fresh credential with no
//! shared state across calls. That makes it unsuitable for round-trip
//! testing of the `save → get` pattern this module exposes (where `save`
//! and `get` are independent calls that each construct their own `Entry`).
//! As a result, this module's tests are limited to invariants that don't
//! require cross-call persistence; the actual keychain round-trip is
//! validated manually on macOS during release testing.

use keyring::Entry;

/// Default service name for credentials stored via the legacy single-name API.
/// Sync tokens use their own service (see `with_service`).
pub const DEFAULT_SERVICE: &str = "RusTerm";

pub struct KeyringStore;

impl KeyringStore {
    /// Save a credential under the default service `"RusTerm"` and the given
    /// name. Overwrites any existing entry with the same name.
    pub fn save_credential(name: &str, secret: &str) -> anyhow::Result<()> {
        Self::save_credential_with(DEFAULT_SERVICE, name, secret)
    }

    /// Look up a credential previously saved via [`save_credential`].
    pub fn get_credential(name: &str) -> anyhow::Result<String> {
        Self::get_credential_with(DEFAULT_SERVICE, name)
    }

    /// Delete a credential previously saved via [`save_credential`]. Returns
    /// Ok if the credential was deleted, or an error if it did not exist or
    /// the keyring is unavailable.
    pub fn delete_credential(name: &str) -> anyhow::Result<()> {
        Self::delete_credential_with(DEFAULT_SERVICE, name)
    }

    /// Save a credential under an explicit `service` and `account` pair.
    /// Use this when you want to namespace credentials away from the default
    /// `"RusTerm"` service — e.g. sync tokens live under `"rusterm.sync.*"`.
    pub fn save_credential_with(service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        let entry = Entry::new(service, account)?;
        entry.set_password(secret)?;
        Ok(())
    }

    /// Look up a credential previously saved via [`save_credential_with`].
    pub fn get_credential_with(service: &str, account: &str) -> anyhow::Result<String> {
        let entry = Entry::new(service, account)?;
        let password = entry.get_password()?;
        Ok(password)
    }

    /// Delete a credential previously saved via [`save_credential_with`].
    pub fn delete_credential_with(service: &str, account: &str) -> anyhow::Result<()> {
        let entry = Entry::new(service, account)?;
        entry.delete_credential()?;
        Ok(())
    }
}
