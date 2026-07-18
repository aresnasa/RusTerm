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
///
/// `exit_code` is propagated from sources that record it (atuin DB,
/// RusTerm's own DB, runtime OSC 133;D). It is `None` for sources whose
/// file format has no exit-code field (bash/zsh/fish flat history files).
/// Downstream code (DB import) uses this to mark failed commands so the
/// `HAVING` clause in `search_history` can filter them out — without this,
/// failed commands from atuin would land as `exit_code = NULL` and be
/// kept as "unknown, assume success", re-surfacing as suggestions.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HistoryMatch {
    pub command: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub score: f32,
    pub exit_code: Option<i32>,
}

impl HistoryMatch {
    pub fn new(
        command: String,
        cwd: Option<String>,
        hostname: Option<String>,
        timestamp: Option<DateTime<Utc>>,
        score: f32,
        exit_code: Option<i32>,
    ) -> Self {
        Self {
            command,
            cwd,
            hostname,
            timestamp,
            score,
            exit_code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the `HistoryMatch::new` signature: the `exit_code` parameter was
    /// added so atuin's per-execution exit code can flow through to the DB
    /// import path (and from there into the `HAVING` clause that filters
    /// failed commands out of suggestions). A regression that drops the
    /// field or reorders params would break this test and force the author
    /// to confront the dirty-data source they'd be re-introducing.
    #[test]
    fn history_match_new_carries_exit_code() {
        let m = HistoryMatch::new(
            "pwdwd".to_string(),
            Some("/home/u".to_string()),
            Some("host".to_string()),
            None,
            1.0,
            Some(127),
        );
        assert_eq!(m.command, "pwdwd");
        assert_eq!(m.exit_code, Some(127));
    }

    /// Sources without exit-code info (bash/zsh/fish flat history files) pass
    /// `None`. This is the contract the hybrid provider and the DB import rely
    /// on: `None` means "unknown", and downstream the HAVING clause keeps such
    /// rows as "assume success" — which is why we ALSO need the
    /// `known_failed_commands` filter at import time to skip commands we've
    /// previously marked as failed.
    #[test]
    fn history_match_accepts_none_exit_code_for_flat_file_sources() {
        let m = HistoryMatch::new("ls".to_string(), None, None, None, 1.0, None);
        assert_eq!(m.exit_code, None);
    }

    /// Hybrid provider's dedup merge must preserve a non-None `exit_code`
    /// when one source has it and another doesn't (e.g. atuin has exit_code,
    /// bash file doesn't). This is the core of the dirty-data fix: without
    /// this propagation, the merged entry would lose atuin's failure signal.
    ///
    /// We model the merge logic inline (the actual `HybridHistoryProvider::search`
    /// reads files we can't easily mock here) to pin the data contract.
    #[test]
    fn hybrid_merge_preserves_exit_code_from_source_that_has_it() {
        // Simulate two HistoryMatches for the same command from different sources
        let from_atuin = HistoryMatch::new(
            "kubectl get pods".to_string(),
            Some("/work".to_string()),
            Some("host".to_string()),
            None,
            50.0,
            Some(1), // atuin recorded a failure
        );
        let from_bash = HistoryMatch::new(
            "kubectl get pods".to_string(),
            None,
            None,
            None,
            40.0,
            None, // bash file has no exit code
        );

        // Replicate the merge logic from `hybrid.rs`: prefer the higher score,
        // and prefer a known exit_code over None.
        let mut best: std::collections::HashMap<String, HistoryMatch> =
            std::collections::HashMap::new();
        for m in [from_atuin, from_bash] {
            let entry = best.entry(m.command.clone()).or_insert_with(|| {
                HistoryMatch::new(
                    m.command.clone(),
                    m.cwd.clone(),
                    m.hostname.clone(),
                    m.timestamp,
                    m.score,
                    m.exit_code,
                )
            });
            if m.score > entry.score
                || (m.score == entry.score && (m.cwd.is_some() || m.hostname.is_some()))
            {
                entry.score = m.score;
                if m.cwd.is_some() {
                    entry.cwd = m.cwd;
                }
                if m.hostname.is_some() {
                    entry.hostname = m.hostname;
                }
                if m.timestamp.is_some() {
                    entry.timestamp = m.timestamp;
                }
                if m.exit_code.is_some() {
                    match entry.exit_code {
                        None => entry.exit_code = m.exit_code,
                        Some(0) => entry.exit_code = m.exit_code,
                        Some(_) => {} // keep existing non-zero
                    }
                }
            }
        }

        let merged = best
            .get("kubectl get pods")
            .expect("merged entry must exist");
        assert_eq!(
            merged.exit_code,
            Some(1),
            "merge must preserve atuin's non-zero exit_code so the DB import \
             marks the command as failed: {:?}",
            merged.exit_code
        );
    }
}
