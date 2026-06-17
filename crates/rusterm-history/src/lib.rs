//! # rusterm-history: Local-only command history providers
//!
//! **SECURITY POLICY:** All data accessed by this crate is strictly local.
//! No history data is ever transmitted over any network connection.
//!
//! The `network` feature flag exists as a compile-time guard — any future
//! functionality that would transmit history data MUST be gated behind
//! `#[cfg(feature = "network")]`. The default build (without this feature)
//! guarantees zero network I/O for history data.

pub mod atuin_db;
pub mod bash_history;
pub mod fish_history;
pub mod hybrid;
pub mod zsh_history;

pub use atuin_db::AtuinDbProvider;
pub use bash_history::BashHistoryProvider;
pub use fish_history::FishHistoryProvider;
pub use hybrid::HybridHistoryProvider;
pub use zsh_history::ZshHistoryProvider;

use chrono::{DateTime, Utc};

/// A match from command history search.
///
/// This struct is `#[non_exhaustive]` to prevent external code from
/// constructing it in ways that might bypass the local-only policy.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HistoryMatch {
    pub command: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub score: f32,
}

impl HistoryMatch {
    pub fn new(
        command: String,
        cwd: Option<String>,
        hostname: Option<String>,
        timestamp: Option<DateTime<Utc>>,
        score: f32,
    ) -> Self {
        Self { command, cwd, hostname, timestamp, score }
    }
}
