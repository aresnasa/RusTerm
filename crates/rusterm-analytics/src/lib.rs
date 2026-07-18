//! # rusterm-analytics: Local-only OLAP layer for command history
//!
//! **SECURITY POLICY:** Mirrors the `rusterm-history` policy — all data is
//! strictly local. DuckDB runs embedded (in-process, no server), and the
//! backing file lives under `~/.local/share/rusterm/rusterm-analytics.duckdb`.
//! No data ever leaves the machine.
//!
//! ## Why DuckDB alongside SQLite?
//!
//! SQLite (in `rusterm-db`) is the OLTP store: per-keystroke suggestion
//! queries, individual history inserts, atomic failure markers. It's tuned
//! for low-latency point reads and writes.
//!
//! DuckDB is the OLAP store: aggregations, group-by-classification, time-
//! bucketed usage patterns, success-rate-by-prefix. These queries scan
//! large portions of the history table and benefit from DuckDB's vectorized
//! columnar execution engine — typically 10-100x faster than SQLite for
//! the same GROUP BY queries on >10k rows.
//!
//! ## Data flow
//!
//! ```text
//!   ~/.bash_history ──┐
//!   ~/.zsh_history ───┤── rusterm-history ──► rusterm-db (SQLite, OLTP)
//!   ~/.atuin/history ─┘                            │
//!                                                  ▼
//!                                       rusterm-analytics (DuckDB, OLAP)
//!                                       - classify_commands()
//!                                       - success_rate_by_prefix()
//!                                       - usage_patterns_by_time_of_day()
//!                                       - behavior_summary()
//! ```
//!
//! The mirror from SQLite → DuckDB happens:
//!   - On `AnalyticsDB::open()` (full re-mirror)
//!   - On `mirror_from_sqlite()` (manual refresh)
//!   - Incremental via `record_command()` on each successful command
//!
//! ## Concurrency
//!
//! DuckDB's Rust crate (`duckdb::Connection`) is `Send` but NOT `Sync`.
//! We wrap it in a `Mutex<Connection>` so the `AnalyticsDB` can be shared
//! across tasks. All public methods take the lock synchronously — analytics
//! queries are fast enough (single-digit ms) that we don't need an async
//! channel like `tokio-rusqlite` uses for SQLite.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use duckdb::Connection;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

pub mod classify;
pub mod mirror;

pub use classify::{CommandCategory, classify_commands};
pub use mirror::mirror_from_sqlite;

/// One row in the analytics-optimized `commands` table.
///
/// Mirrors a subset of `rusterm_db::HistoryEntry` — only the columns analytics
/// queries actually read. Skipping cwd/session_id/duration keeps the DuckDB
/// file smaller and the scans faster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsCommand {
    pub command: String,
    pub hostname: Option<String>,
    pub exit_code: Option<i32>,
    /// UTC timestamp of the command execution (RFC3339).
    pub created_at: String,
}

/// Aggregated (category, count) row from `classify_commands()`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CategoryCount {
    pub category: CommandCategory,
    pub count: u64,
}

/// Aggregated (prefix, success_rate) row from `success_rate_by_prefix()`.
/// `success_rate` is in [0.0, 1.0]; commands with NULL exit_code are treated
/// as "unknown" and excluded from the denominator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrefixSuccessRate {
    pub prefix: String,
    pub total_attempts: u64,
    pub successful: u64,
    pub failed: u64,
    pub success_rate: f32,
}

/// Aggregated (hour_of_day, count) row from `usage_patterns_by_time_of_day()`.
/// `hour` is in [0, 23] UTC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HourlyUsage {
    pub hour: u32,
    pub count: u64,
}

/// High-level behavior summary shown in the analytics panel.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BehaviorSummary {
    pub total_commands: u64,
    pub unique_commands: u64,
    pub known_failed_commands: u64,
    pub success_rate: f32,
    pub most_used_category: Option<CommandCategory>,
    pub most_used_command: Option<String>,
    /// Busiest hour of day (UTC), 0-23. None if no data.
    pub busiest_hour: Option<u32>,
    /// Distinct hosts the user has run commands on.
    pub distinct_hosts: u64,
}

/// Embedded DuckDB analytics database.
///
/// Wraps a `duckdb::Connection` in a `Mutex` (DuckDB's Connection is `Send`
/// but not `Sync`). All methods are sync — analytics queries are fast (the
/// vectorized engine makes short work of <100k rows) and we don't want the
/// per-call overhead of an async channel for this.
pub struct AnalyticsDB {
    conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl AnalyticsDB {
    /// Open (or create) the analytics DB at the given path. Runs the schema
    /// migration on every open (DuckDB's `CREATE TABLE IF NOT EXISTS` is
    /// idempotent and cheap).
    pub fn open(path: Option<impl AsRef<Path>>) -> Result<Self> {
        let db_path = path
            .as_ref()
            .map(|p| p.as_ref().to_path_buf())
            .unwrap_or_else(|| {
                dirs::data_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("rusterm")
                    .join("rusterm-analytics.duckdb")
            });
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating analytics db parent dir: {}", parent.display())
            })?;
        }
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening analytics duckdb at {}", db_path.display()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path,
        })
    }

    /// Open an in-memory DuckDB. Used by tests and for ephemeral analytics
    /// sessions that don't need persistence.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory duckdb")?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::new(),
        })
    }

    /// Create the `commands` table if it doesn't exist. The schema mirrors a
    /// subset of `rusterm_db::history::HistoryEntry` — we deliberately omit
    /// `id`, `session_id`, `cwd`, and `duration_ms` because no analytics query
    /// reads them. This keeps the DuckDB file small and scans fast.
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS commands (
                command      VARCHAR NOT NULL,
                hostname     VARCHAR,
                exit_code    INTEGER,
                created_at   VARCHAR NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_commands_command ON commands(command);
            CREATE INDEX IF NOT EXISTS idx_commands_hostname ON commands(hostname);
            ",
        )?;
        Ok(())
    }

    /// Insert a single command execution. Used for incremental mirroring
    /// from the runtime path (each successful command is recorded here too,
    /// so the analytics DB stays current without a full re-mirror).
    pub fn record_command(&self, cmd: &AnalyticsCommand) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO commands (command, hostname, exit_code, created_at) VALUES (?, ?, ?, ?)",
            duckdb::params![cmd.command, cmd.hostname, cmd.exit_code, cmd.created_at],
        )?;
        Ok(())
    }

    /// Total row count in the `commands` table. Used by tests and the
    /// behavior summary.
    pub fn total_commands(&self) -> Result<u64> {
        let conn = self.conn.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM commands", [], |row| row.get(0))
            .context("counting commands")?;
        Ok(count as u64)
    }

    /// Classify all commands by category (git, docker, kubectl, file ops,
    /// etc.). Returns counts per category, sorted descending. See
    /// `classify::classify_commands` for the prefix-matching rules.
    pub fn classify(&self) -> Result<Vec<CategoryCount>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT command FROM commands")?;
        let commands: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let counts = classify_commands(&commands);
        Ok(counts)
    }

    /// Compute success rate per command prefix. A "prefix" here is the first
    /// whitespace-delimited token of the command (e.g. `git`, `kubectl`,
    /// `cargo`, `ls`). Commands with NULL exit_code are excluded from the
    /// denominator (we can't tell if they succeeded).
    ///
    /// Useful for surfacing typos: a prefix with 0% success rate across many
    /// attempts is almost certainly a typo the user keeps making (e.g.
    /// `gut` instead of `git`).
    pub fn success_rate_by_prefix(&self) -> Result<Vec<PrefixSuccessRate>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH prefixes AS (
                SELECT
                    CASE
                        WHEN position(' ' in command) > 0
                            THEN substring(command FROM 1 FOR position(' ' in command) - 1)
                        ELSE command
                    END AS prefix,
                    exit_code
                FROM commands
                WHERE exit_code IS NOT NULL
            )
            SELECT
                prefix,
                COUNT(*) AS total,
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) AS ok,
                SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) AS fail
            FROM prefixes
            GROUP BY prefix
            ORDER BY total DESC",
        )?;
        let rows: Vec<(String, i64, i64, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows
            .into_iter()
            .map(|(prefix, total, ok, fail)| {
                let total_u = total as u64;
                let ok_u = ok as u64;
                let fail_u = fail as u64;
                let rate = if total_u == 0 {
                    0.0
                } else {
                    ok_u as f32 / total_u as f32
                };
                PrefixSuccessRate {
                    prefix,
                    total_attempts: total_u,
                    successful: ok_u,
                    failed: fail_u,
                    success_rate: rate,
                }
            })
            .collect())
    }

    /// Bucket all command executions by hour-of-day (UTC, 0-23). Returns 24
    /// rows (one per hour) — hours with no executions have count 0. Useful
    /// for visualizing when the user is most active.
    ///
    /// The query uses DuckDB's `strftime(..., '%H')` to extract the UTC hour
    /// from each command's RFC3339 timestamp. We cast to `TIMESTAMPTZ` first
    /// (so DuckDB recognizes the `Z` suffix as UTC) and then format with
    /// `%H` — `strftime` on a `TIMESTAMPTZ` returns the hour in UTC, not in
    /// the host's local timezone (verified empirically against DuckDB 1.10504).
    ///
    /// Earlier attempts used `EXTRACT(HOUR FROM <ts> AT TIME ZONE 'UTC')`
    /// but DuckDB's binder rejects that syntax. `strftime` is the canonical
    /// way to extract a UTC hour from a TIMESTAMPTZ in DuckDB.
    pub fn usage_patterns_by_time_of_day(&self) -> Result<Vec<HourlyUsage>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH hours AS (
                SELECT
                    CAST(strftime(TRY_CAST(created_at AS TIMESTAMPTZ), '%H') AS INTEGER) AS hour,
                    COUNT(*) AS cnt
                FROM commands
                WHERE TRY_CAST(created_at AS TIMESTAMPTZ) IS NOT NULL
                GROUP BY 1
            )
            SELECT g.hour, COALESCE(h.cnt, 0) AS cnt
            FROM generate_series(0, 23) AS g(hour)
            LEFT JOIN hours h ON h.hour = g.hour
            ORDER BY g.hour",
        )?;
        let rows: Vec<(i64, i64)> = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows
            .into_iter()
            .map(|(hour, count)| HourlyUsage {
                hour: hour as u32,
                count: count as u64,
            })
            .collect())
    }

    /// High-level behavior summary. Aggregates several metrics in one call
    /// so the UI panel can render with a single round-trip.
    pub fn behavior_summary(&self) -> Result<BehaviorSummary> {
        let conn = self.conn.lock();

        // total + unique + known-failed in one query
        let (total, unique, known_failed): (i64, i64, i64) = conn
            .query_row(
                "SELECT
                    COUNT(*) AS total,
                    COUNT(DISTINCT command) AS unique_cmds,
                    COUNT(DISTINCT CASE WHEN exit_code IS NOT NULL AND exit_code != 0 THEN command END) AS failed
                 FROM commands",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("behavior_summary: total/unique/failed")?;

        // success rate (excluding NULL exit codes)
        let (ok, total_with_exit): (i64, i64) = conn
            .query_row(
                "SELECT
                    SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN exit_code IS NOT NULL THEN 1 ELSE 0 END)
                 FROM commands",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("behavior_summary: success rate")?;
        let success_rate = if total_with_exit == 0 {
            0.0
        } else {
            ok as f32 / total_with_exit as f32
        };

        // most-used command
        let most_used_command: Option<String> = conn
            .query_row(
                "SELECT command FROM commands GROUP BY command ORDER BY COUNT(*) DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();

        // distinct hosts
        let distinct_hosts: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT hostname) FROM commands WHERE hostname IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .context("behavior_summary: distinct hosts")?;

        // busiest hour of day (UTC) — reuses the strftime bucketing logic.
        let mut stmt = conn.prepare(
            "SELECT
                CAST(strftime(TRY_CAST(created_at AS TIMESTAMPTZ), '%H') AS INTEGER) AS hour,
                COUNT(*) AS cnt
             FROM commands
             WHERE TRY_CAST(created_at AS TIMESTAMPTZ) IS NOT NULL
             GROUP BY 1
             ORDER BY cnt DESC
             LIMIT 1",
        )?;
        let busiest_hour: Option<i64> = stmt.query_row([], |row| row.get(0)).ok();

        // most-used category — we classify the commands in-process (no
        // SQL for this; the prefix-matching rules live in `classify`).
        drop(stmt);
        let mut stmt2 = conn.prepare("SELECT command FROM commands")?;
        let all_commands: Vec<String> = stmt2
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let most_used_category = classify_commands(&all_commands)
            .into_iter()
            .next()
            .map(|c| c.category);

        Ok(BehaviorSummary {
            total_commands: total as u64,
            unique_commands: unique as u64,
            known_failed_commands: known_failed as u64,
            success_rate,
            most_used_category,
            most_used_command,
            busiest_hour: busiest_hour.map(|h| h as u32),
            distinct_hosts: distinct_hosts as u64,
        })
    }

    /// Wipe all analytics data. Used by tests and by a future "reset
    /// analytics" UI action.
    pub fn clear(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM commands", [])?;
        Ok(())
    }
}

/// Convenience helper: parse an RFC3339 timestamp string into a `DateTime<Utc>`.
/// Public so tests can construct `AnalyticsCommand` values without repeating
/// the parse logic.
pub fn parse_created_at(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(command: &str, exit_code: Option<i32>, created_at: &str) -> AnalyticsCommand {
        AnalyticsCommand {
            command: command.to_string(),
            hostname: Some("local".to_string()),
            exit_code,
            created_at: created_at.to_string(),
        }
    }

    /// Smoke test: open an in-memory DB, insert one row, count it.
    /// Pins the schema + record_command + total_commands contract.
    #[test]
    fn open_in_memory_and_record_command() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        assert_eq!(db.total_commands().unwrap(), 0);
        db.record_command(&cmd("ls", Some(0), "2026-07-18T10:00:00Z"))
            .unwrap();
        assert_eq!(db.total_commands().unwrap(), 1);
    }

    /// `classify_commands` must bucket commands by their leading token into
    /// the right category. Pins the git/docker/kubectl/etc. classification
    /// rules so a future regression in the prefix matcher is caught.
    #[test]
    fn classify_groups_by_category() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        for c in [
            cmd("git status", Some(0), "2026-07-18T10:00:00Z"),
            cmd("git log", Some(0), "2026-07-18T10:01:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:02:00Z"),
            cmd("docker run -it alpine", Some(0), "2026-07-18T10:03:00Z"),
            cmd("kubectl get pods", Some(0), "2026-07-18T10:04:00Z"),
            cmd("ls -la", Some(0), "2026-07-18T10:05:00Z"),
            cmd("pwd", Some(0), "2026-07-18T10:06:00Z"),
        ] {
            db.record_command(&c).unwrap();
        }
        let counts = db.classify().unwrap();
        // Should be sorted descending by count; ties broken by label alpha.
        // Git and Docker both have count 2 → Docker (alpha < Git) comes first.
        assert!(counts[0].count >= counts[counts.len() - 1].count);
        assert_eq!(counts[0].category, CommandCategory::Docker);
        assert_eq!(counts[0].count, 2);
        assert_eq!(counts[1].category, CommandCategory::Git);
        assert_eq!(counts[1].count, 2);
    }

    /// `success_rate_by_prefix` must exclude NULL exit codes from the
    /// denominator. This is the contract the typo-detection UI relies on:
    /// "0% success rate across N attempts" should only count attempts where
    /// we actually saw a non-zero exit code.
    #[test]
    fn success_rate_excludes_null_exit_codes() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        // 3 successful git commands, 1 failed git command, 1 NULL git command.
        // success_rate = 3/4 = 0.75 (NULL excluded from denominator).
        for c in [
            cmd("git status", Some(0), "2026-07-18T10:00:00Z"),
            cmd("git log", Some(0), "2026-07-18T10:01:00Z"),
            cmd("git diff", Some(0), "2026-07-18T10:02:00Z"),
            cmd("git push", Some(1), "2026-07-18T10:03:00Z"),
            cmd("git checkout", None, "2026-07-18T10:04:00Z"), // NULL — excluded
        ] {
            db.record_command(&c).unwrap();
        }
        let rates = db.success_rate_by_prefix().unwrap();
        let git = rates
            .iter()
            .find(|r| r.prefix == "git")
            .expect("git prefix must exist");
        assert_eq!(git.total_attempts, 4, "NULL exit codes must be excluded");
        assert_eq!(git.successful, 3);
        assert_eq!(git.failed, 1);
        assert!(
            (git.success_rate - 0.75).abs() < 0.001,
            "success_rate must be 0.75, got {}",
            git.success_rate
        );
    }

    /// `usage_patterns_by_time_of_day` must return 24 rows (one per hour),
    /// even for hours with no executions. This pins the `generate_series`
    /// join behavior — a regression that drops the LEFT JOIN would return
    /// only hours with data.
    #[test]
    fn usage_patterns_returns_all_24_hours() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        // Two commands at 10:xx UTC, one at 14:xx UTC
        for c in [
            cmd("ls", Some(0), "2026-07-18T10:00:00Z"),
            cmd("pwd", Some(0), "2026-07-18T10:30:00Z"),
            cmd("git status", Some(0), "2026-07-18T14:15:00Z"),
        ] {
            db.record_command(&c).unwrap();
        }
        let buckets = db.usage_patterns_by_time_of_day().unwrap();
        assert_eq!(buckets.len(), 24, "must return exactly 24 hour buckets");
        assert_eq!(buckets[10].hour, 10);
        assert_eq!(buckets[10].count, 2, "10:xx UTC must have 2 commands");
        assert_eq!(buckets[14].hour, 14);
        assert_eq!(buckets[14].count, 1, "14:xx UTC must have 1 command");
        assert_eq!(buckets[0].count, 0, "midnight bucket must be 0");
    }

    /// `behavior_summary` aggregates several metrics. Pin the contract: it
    /// must compute total, unique, known_failed, success_rate, most-used
    /// command, busiest hour, distinct hosts, and most-used category.
    #[test]
    fn behavior_summary_aggregates_metrics() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        for c in [
            cmd("git status", Some(0), "2026-07-18T10:00:00Z"),
            cmd("git log", Some(0), "2026-07-18T10:05:00Z"),
            cmd("pwdwd", Some(127), "2026-07-18T10:10:00Z"), // typo — failed
            cmd("ls", Some(0), "2026-07-18T11:00:00Z"),
            cmd("docker ps", Some(0), "2026-07-19T09:00:00Z"),
        ] {
            db.record_command(&c).unwrap();
        }
        let summary = db.behavior_summary().unwrap();
        assert_eq!(summary.total_commands, 5);
        assert_eq!(summary.unique_commands, 5);
        assert_eq!(summary.known_failed_commands, 1);
        // 4 successful, 1 failed → 4/5 = 0.8
        assert!(
            (summary.success_rate - 0.8).abs() < 0.001,
            "success_rate must be 0.8, got {}",
            summary.success_rate
        );
        assert_eq!(summary.distinct_hosts, 1);
        // busiest hour is 10 UTC (two commands: git status + git log)
        assert_eq!(summary.busiest_hour, Some(10));
        // most-used category is Git (2 commands) — beats Docker (1) on count
        assert_eq!(summary.most_used_category, Some(CommandCategory::Git));
        // most-used command — all commands appear once, so it's whatever
        // GROUP BY ... ORDER BY COUNT(*) DESC picks first. We can't assert
        // the exact value, only that it's Some.
        assert!(summary.most_used_command.is_some());
    }

    /// `clear` must wipe all rows. Used by tests and by a future "reset
    /// analytics" UI action.
    #[test]
    fn clear_wipes_all_rows() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("ls", Some(0), "2026-07-18T10:00:00Z"))
            .unwrap();
        assert_eq!(db.total_commands().unwrap(), 1);
        db.clear().unwrap();
        assert_eq!(db.total_commands().unwrap(), 0);
    }
}
