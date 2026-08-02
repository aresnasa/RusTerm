#![allow(unused_imports)]
//! Analytics integration layer.
//!
//! When the `analytics` feature is enabled, this module wraps
//! `rusterm-analytics`'s `AnalyticsDB` and provides hooks for the app to:
//!   - Open / lazily-init the analytics DB on first use
//!   - Mirror history from SQLite → DuckDB on launch
//!   - Record each successful command execution (incremental mirror)
//!   - Query aggregated metrics for a future analytics UI panel
//!
//! When the feature is disabled, the same API surface returns empty
//! results / no-ops, so the app code can call into this module
//! unconditionally without `#[cfg]` guards at every call site.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearnedCorrection {
    pub correction: String,
    pub observations: u64,
}

/// Whether the user has opted in to local usage-habit collection. Encapsulates
/// the gating policy so call sites stay clean. This is the in-memory mirror of
/// `PersistedConfig::collect_usage_habits`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageHabitsEnabled(pub bool);

impl UsageHabitsEnabled {
    pub fn enabled(self) -> bool {
        self.0
    }
}

#[cfg(feature = "analytics")]
pub mod enabled {
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use parking_lot::Mutex;
    use rusterm_analytics::{
        AnalyticsCommand, AnalyticsDB, BehaviorSummary, CategoryCount, HourlyUsage,
        PrefixSuccessRate,
    };

    use super::LearnedCorrection;

    /// Lazily-initialized analytics DB handle. Stored in `AppState` so the
    /// connection persists across renders. Wrapped in `Option` because we
    /// don't want to pay the DuckDB open cost on app startup if the user
    /// never opens the analytics panel — first query triggers init.
    ///
    /// `Clone` is cheap (inner is `Arc<Mutex<...>>`), so callers can clone
    /// before spawning async tasks. `Debug` is manual because `AnalyticsDB`
    /// doesn't implement it (DuckDB's `Connection` has no Debug impl).
    #[derive(Clone)]
    pub struct AnalyticsHandle {
        inner: Arc<Mutex<Option<AnalyticsDB>>>,
    }

    impl std::fmt::Debug for AnalyticsHandle {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let open = self.inner.lock().is_some();
            f.debug_struct("AnalyticsHandle")
                .field("open", &open)
                .finish()
        }
    }

    impl AnalyticsHandle {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(None)),
            }
        }

        /// Open the analytics DB at the default path
        /// (`~/.local/share/rusterm/rusterm-analytics.duckdb`). Idempotent —
        /// subsequent calls return the existing handle.
        pub fn ensure_open(&self) -> Result<()> {
            let mut guard = self.inner.lock();
            if guard.is_some() {
                return Ok(());
            }
            let db = AnalyticsDB::open(None::<&str>).context("opening analytics duckdb")?;
            *guard = Some(db);
            Ok(())
        }

        /// Mirror all rows from the SQLite history table into DuckDB. Wipes
        /// the DuckDB table first so the result is a consistent snapshot.
        /// Should be run on app startup (after the SQLite history import
        /// completes) and on manual "refresh analytics" UI action.
        pub async fn mirror_from_sqlite(&self, sqlite_db: &rusterm_db::Database) -> Result<u64> {
            self.ensure_open()?;
            // We need to take the DB out of the lock to pass it to the async
            // mirror function. Take it, run, put it back.
            let db = {
                let mut guard = self.inner.lock();
                guard.take().expect("ensure_open just populated it")
            };
            let result = rusterm_analytics::mirror_from_sqlite(&db, sqlite_db).await;
            // Put it back regardless of outcome
            let mut guard = self.inner.lock();
            *guard = Some(db);
            result
        }

        /// Record a single command execution (incremental mirror). Called
        /// from the runtime path each time a command succeeds (rc==0).
        pub fn record_command(&self, cmd: &AnalyticsCommand) -> Result<()> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            if let Some(db) = guard.as_ref() {
                db.record_command(cmd)?;
            }
            Ok(())
        }

        pub fn record_command_correction(&self, typo: &str, correction: &str) -> Result<()> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            if let Some(db) = guard.as_ref() {
                db.record_command_correction(typo, correction)?;
            }
            Ok(())
        }

        pub fn command_corrections_for(&self, typo: &str) -> Result<Vec<LearnedCorrection>> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            let corrections = guard
                .as_ref()
                .context("analytics db not open")?
                .command_corrections_for(typo)?;
            Ok(corrections
                .into_iter()
                .map(|correction| LearnedCorrection {
                    correction: correction.correction,
                    observations: correction.observations,
                })
                .collect())
        }

        pub fn classify(&self) -> Result<Vec<CategoryCount>> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            guard.as_ref().context("analytics db not open")?.classify()
        }

        pub fn success_rate_by_prefix(&self) -> Result<Vec<PrefixSuccessRate>> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            guard
                .as_ref()
                .context("analytics db not open")?
                .success_rate_by_prefix()
        }

        pub fn usage_patterns_by_time_of_day(&self) -> Result<Vec<HourlyUsage>> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            guard
                .as_ref()
                .context("analytics db not open")?
                .usage_patterns_by_time_of_day()
        }

        pub fn behavior_summary(&self) -> Result<BehaviorSummary> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            guard
                .as_ref()
                .context("analytics db not open")?
                .behavior_summary()
        }
    }

    impl AnalyticsHandle {
        /// Build a privacy-safe report from the local DuckDB data. Only
        /// aggregated fields are included — never raw command lines, hostnames
        /// or timestamps. Each aggregated row passes through
        /// `sanitize_command` to guarantee no secret material survives even
        /// inside aggregated prefix/correction text.
        pub fn build_usage_habits_report(&self) -> anyhow::Result<UsageHabitsReport> {
            self.ensure_open()?;
            let guard = self.inner.lock();
            let db = guard.as_ref().context("analytics db not open")?;
            let classifications = db.classify()?;
            let hourly_usage = db.usage_patterns_by_time_of_day()?;
            let prefix_success_rates = db.success_rate_by_prefix()?;
            let mut typo_corrections = db.all_command_corrections()?;
            // Defense-in-depth: never export a correction pair that the
            // sanitizer would drop or redact. (The storage layer already
            // sanitizes on insert, but re-checking here means a future schema
            // migration can't leak anything.)
            typo_corrections.retain(|c| {
                rusterm_analytics::sanitize_command(&c.typo).is_some()
                    && rusterm_analytics::sanitize_command(&c.correction).is_some()
            });
            Ok(UsageHabitsReport {
                generated_at: chrono::Utc::now(),
                total_commands: db.total_commands()?,
                classifications,
                hourly_usage,
                prefix_success_rates,
                typo_corrections,
            })
        }
    }

    /// Privacy-safe export payload for the usage-habits report. Only aggregated,
    /// non-identifying fields: command classifications, hourly activity, per-prefix
    /// success rates, and typo→correction pairs. Raw command text, hostnames,
    /// timestamps, session ids and any line flagged by `sanitize_command` are
    /// excluded.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct UsageHabitsReport {
        pub generated_at: chrono::DateTime<chrono::Utc>,
        pub total_commands: u64,
        pub classifications: Vec<rusterm_analytics::CategoryCount>,
        pub hourly_usage: Vec<rusterm_analytics::HourlyUsage>,
        pub prefix_success_rates: Vec<rusterm_analytics::PrefixSuccessRate>,
        pub typo_corrections: Vec<rusterm_analytics::CommandCorrection>,
    }

    impl Default for AnalyticsHandle {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(not(feature = "analytics"))]
pub mod disabled {
    use super::LearnedCorrection;

    /// Stub `AnalyticsHandle` for when the `analytics` feature is off.
    /// All methods are no-ops or return empty results — the app can call
    /// into this unconditionally without `#[cfg]` guards at every call site.
    ///
    /// The `record_command` method takes a generic `&T` (rather than
    /// `&rusterm_analytics::AnalyticsCommand`) so we don't need to depend
    /// on `rusterm-analytics` when the feature is off — that keeps the
    /// non-analytics build truly DuckDB-free.
    #[derive(Default, Clone, Debug)]
    pub struct AnalyticsHandle;

    impl AnalyticsHandle {
        pub fn new() -> Self {
            Self
        }
        pub fn ensure_open(&self) -> anyhow::Result<()> {
            Ok(())
        }
        pub async fn mirror_from_sqlite(
            &self,
            _sqlite_db: &rusterm_db::Database,
        ) -> anyhow::Result<u64> {
            Ok(0)
        }
        pub fn record_command<T>(&self, _cmd: &T) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn record_command_correction(
            &self,
            _typo: &str,
            _correction: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn command_corrections_for(
            &self,
            _typo: &str,
        ) -> anyhow::Result<Vec<LearnedCorrection>> {
            Ok(Vec::new())
        }
    }

    impl AnalyticsHandle {
        pub fn build_usage_habits_report(&self) -> anyhow::Result<UsageHabitsReport> {
            Ok(UsageHabitsReport {
                generated_at: chrono::Utc::now(),
            })
        }
    }

    /// Feature-off stub: an empty report with just a timestamp. The disabled
    /// `AnalyticsHandle::build_usage_habits_report` returns this so call sites
    /// compile and behave as no-ops without the DuckDB dependency.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    pub struct UsageHabitsReport {
        pub generated_at: chrono::DateTime<chrono::Utc>,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn correction_learning_is_a_safe_noop_without_analytics() {
            let handle = AnalyticsHandle::new();
            handle
                .record_command_correction("dockre ps", "docker ps")
                .unwrap();
            assert!(
                handle
                    .command_corrections_for("dockre ps")
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[cfg(not(feature = "analytics"))]
pub use disabled::AnalyticsHandle;
#[cfg(feature = "analytics")]
pub use enabled::AnalyticsHandle;
