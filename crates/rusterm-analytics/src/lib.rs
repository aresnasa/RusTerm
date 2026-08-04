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
pub mod embed;
pub mod mirror;
pub mod sanitize;

pub use classify::{CommandCategory, classify_commands};
pub use embed::{DEFAULT_DIM, Embedder, HashEmbedder, cosine_similarity};
pub use mirror::mirror_from_sqlite;
pub use sanitize::{contains_sensitive_material, sanitize_command};

/// Aggregated success/failure counts for one full command line, used to
/// rank habits and to downgrade risky suggestions.
///
/// Executions whose exit code is unknown (NULL) are *observations* of the
/// command but count as neither success nor failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandRanking {
    pub command: String,
    pub successes: u64,
    pub failures: u64,
}

impl CommandRanking {
    /// Observations with a known outcome: successes + failures.
    pub fn total(&self) -> u64 {
        self.successes + self.failures
    }

    /// successes / (successes + failures); 0.0 when there are no
    /// observations with a known outcome.
    pub fn success_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            0.0
        } else {
            self.successes as f64 / total as f64
        }
    }
}

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

/// A locally observed failed-command → successful-command correction pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandCorrection {
    pub typo: String,
    pub correction: String,
    pub observations: u64,
    /// UTC timestamp of the most recent observation (RFC3339).
    pub last_seen: String,
}

/// One observed OneKey behavior event. This is metadata only: identifiers,
/// a SHA-256 prompt fingerprint produced by the UI, and an action label.
/// Credential values, OneKey display names, and raw prompt text are never
/// stored here (mirrors the privacy contract of `OneKeyPreference` in
/// settings.json).
///
/// `candidates_hash` is a digest of the sorted (onekey_id, step_id) pairs
/// that matched the prompt when the event happened. Comparing it against the
/// current match set detects "the user added/removed a OneKey affecting this
/// prompt" — the one situation where the chooser popup must reappear even
/// though a habit exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OneKeyUsageEvent {
    pub connection_id: String,
    /// SHA-256 of the normalized prompt (computed by the UI). Never raw text.
    pub prompt_fingerprint: String,
    /// Stable OneKey/step identifiers. Empty for events that don't target a
    /// specific candidate (e.g. `popup_shown`).
    pub onekey_id: String,
    pub step_id: String,
    /// One of `manual_select`, `auto_submit`, `rejected`, `popup_shown`.
    pub action: String,
    /// Digest of the sorted candidate identifiers at event time.
    pub candidates_hash: String,
    /// UTC timestamp (RFC3339).
    pub created_at: String,
}

/// Aggregated (hour_of_day, count) row from `usage_patterns_by_time_of_day()`.
/// `hour` is in [0, 23] UTC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HourlyUsage {
    pub hour: u32,
    pub count: u64,
}

/// Recency-weighted ("decayed") frequency ranking of a single command line.
///
/// Unlike [`CommandRanking`] (which counts raw successes/failures), the
/// `decayed_score` here is `Σ daily_decay ^ age_days` over every execution —
/// recent runs count ~1.0, old runs fade toward 0. This is the "usage
/// probability" signal the suggestion pipeline blends with semantic
/// similarity (see [`AnalyticsDB::suggest_by_context`]) to model user habits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HabitRanking {
    pub command: String,
    /// `Σ daily_decay ^ age_days`. Higher = used more, more recently.
    pub decayed_score: f64,
    pub total_count: u64,
    pub successes: u64,
    pub failures: u64,
    /// RFC3339 UTC timestamp of the most recent execution.
    pub last_seen: String,
}

impl HabitRanking {
    /// Successes / (successes + failures); 0.0 when no known-outcome
    /// observations exist. Mirrors `CommandRanking::success_rate`.
    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            0.0
        } else {
            self.successes as f64 / total as f64
        }
    }
}

/// A habit-memory suggestion: a command ranked by the blend of semantic
/// similarity to a query and recency-weighted usage frequency.
///
/// Produced by [`AnalyticsDB::suggest_by_context`]. `score` is the final
/// blended score in `[0, 1]` (higher = better); `similarity` and
/// `decayed_score` are exposed so the UI can show *why* a command was
/// suggested ("matches what you typed" vs "you run this a lot").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HabitSuggestion {
    pub command: String,
    /// `alpha * max(0, similarity) + (1 - alpha) * freq_norm`.
    pub score: f32,
    /// Cosine similarity between the query embedding and the command's cached
    /// embedding, in `[-1, 1]`. Clamped to `>= 0` in the blend.
    pub similarity: f32,
    /// Raw decayed frequency (same value as `HabitRanking::decayed_score`).
    pub decayed_score: f64,
    pub total_count: u64,
    pub success_rate: f32,
    pub last_seen: String,
}

/// Tuning knobs for [`AnalyticsDB::suggest_by_context`]. All fields have
/// sensible defaults; pass `SuggestOptions::default()` for the common case.
#[derive(Debug, Clone)]
pub struct SuggestOptions {
    /// Per-day decay multiplier applied to each past execution. `0.99` ≈ a
    /// 69-day half-life: recent habits dominate but old ones still register.
    pub daily_decay: f64,
    /// Blend weight in `[0, 1]` between semantic similarity and frequency.
    /// `0.0` = pure frequency, `1.0` = pure similarity, `0.5` = balanced.
    pub alpha: f32,
    /// Only suggest commands whose success rate is at least this (in
    /// `[0, 1]`). `0.0` = no filter. Set higher (e.g. `0.5`) to avoid
    /// surfacing commands the user usually fails.
    pub min_success_rate: f32,
    /// Only consider commands seen at least this many times. Filters out
    /// one-off typos that happen to embed similarly to the query.
    pub min_observations: u32,
    /// Maximum number of suggestions to return. `0` is treated as no limit.
    pub limit: u32,
}

impl Default for SuggestOptions {
    fn default() -> Self {
        Self {
            daily_decay: 0.99,
            alpha: 0.5,
            min_success_rate: 0.0,
            min_observations: 1,
            limit: 10,
        }
    }
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

            CREATE TABLE IF NOT EXISTS command_corrections (
                typo          VARCHAR NOT NULL,
                correction    VARCHAR NOT NULL,
                observations  BIGINT NOT NULL,
                last_seen     VARCHAR NOT NULL,
                PRIMARY KEY (typo, correction)
            );
            CREATE INDEX IF NOT EXISTS idx_command_corrections_typo
                ON command_corrections(typo);

            -- Habit-memory vector store: one cached embedding per unique
            -- command line. The embedding is stored as a JSON array string
            -- (e.g. [0.1,0.2,...]) rather than a DuckDB FLOAT[N] column so
            -- we avoid array-type binding quirks in duckdb-rs and keep the
            -- schema portable. Vector math (cosine similarity) is done in
            -- Rust at query time — cheap for <10k unique commands.
            --
            -- Populated lazily by `record_command_embedded` /
            -- `upsert_embedding`; the raw `commands` table remains the source
            -- of truth for frequency/decay, this is purely a semantic-similarity
            -- cache so the (deterministic) embedder isn't re-run on every query.
            CREATE TABLE IF NOT EXISTS command_embeddings (
                command   VARCHAR PRIMARY KEY,
                embedding VARCHAR NOT NULL
            );

            -- OneKey behavior events: which saved credential entry the user
            -- (or the auto-submit path) used for which prompt on which
            -- connection, and whether the remote rejected it. Identifiers and
            -- hashes only — credential values, display names, and raw prompt
            -- text never reach this table. Powers habit-based OneKey
            -- switching (auto-submit without a popup once a stable habit is
            -- observed).
            CREATE TABLE IF NOT EXISTS onekey_events (
                connection_id      VARCHAR NOT NULL,
                prompt_fingerprint VARCHAR NOT NULL,
                onekey_id          VARCHAR NOT NULL,
                step_id            VARCHAR NOT NULL,
                action             VARCHAR NOT NULL,
                candidates_hash    VARCHAR NOT NULL,
                created_at         VARCHAR NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_onekey_events_scope
                ON onekey_events(connection_id, prompt_fingerprint);

            -- Session-recovery replay event log (time-series). One row per
            -- replay-recorder mutation, in user-input order: 'op' /
            -- 'context' append an input line, 'pop' removes the trailing
            -- one (exit after sudo -i), 'reset' clears the log (bastion
            -- menu reentry thawed the recorder). Ordering is
            -- (ts_micros, seq): the timestamp is captured synchronously at
            -- input time and seq is an in-process monotonic tiebreaker, so
            -- a fold replays rows in exact submission order even though
            -- the DB writes happen on spawned tasks that may land out of
            -- order. Keyed by connection id (session ids are fresh UUIDs
            -- on every reconnect, so they can't join across restarts).
            CREATE TABLE IF NOT EXISTS replay_events (
                connection_id VARCHAR NOT NULL,
                ts_micros     BIGINT NOT NULL,
                seq           BIGINT NOT NULL,
                event         VARCHAR NOT NULL,
                op            VARCHAR
            );
            CREATE INDEX IF NOT EXISTS idx_replay_events_conn
                ON replay_events(connection_id, ts_micros, seq);
            ",
        )?;
        Ok(())
    }

    /// Insert a single command execution. Used for incremental mirroring
    /// from the runtime path (each successful command is recorded here too,
    /// so the analytics DB stays current without a full re-mirror).
    pub fn record_command(&self, cmd: &AnalyticsCommand) -> Result<()> {
        // Defense in depth: never persist credential material. Lines
        // dominated by secrets (e.g. PEM private keys) are dropped silently.
        let Some(command) = sanitize::sanitize_command(&cmd.command) else {
            return Ok(());
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO commands (command, hostname, exit_code, created_at) VALUES (?, ?, ?, ?)",
            duckdb::params![command, cmd.hostname, cmd.exit_code, cmd.created_at],
        )?;
        Ok(())
    }

    /// Record a command execution *and* cache its embedding for habit-memory
    /// search. This is the preferred recording path for the live runtime:
    /// it keeps `command_embeddings` warm so [`Self::suggest_by_context`]
    /// can rank by semantic similarity without re-running the embedder.
    ///
    /// Embeddings are computed from the **sanitized** command (so cached
    /// vectors never contain secret-derived signal) and upserted idempotently
    /// — the same command always maps to the same vector, so repeated
    /// executions only touch the `commands` table, not `command_embeddings`.
    pub fn record_command_embedded(
        &self,
        cmd: &AnalyticsCommand,
        embedder: &dyn embed::Embedder,
    ) -> Result<()> {
        let Some(command) = sanitize::sanitize_command(&cmd.command) else {
            return Ok(());
        };
        {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO commands (command, hostname, exit_code, created_at) \
                 VALUES (?, ?, ?, ?)",
                duckdb::params![command, cmd.hostname, cmd.exit_code, cmd.created_at],
            )?;
        }
        // Compute and cache the embedding outside the DB lock — hashing is
        // CPU-only and stateless, so there's no contention benefit to holding
        // the lock, and re-acquiring it for the upsert is a single fast op.
        let embedding = embedder.embed(&command);
        self.upsert_embedding(&command, &embedding)
    }

    /// Insert (or replace) the cached embedding for a command. The embedding
    /// is stored as a JSON array string. Public so a backfill job can populate
    /// `command_embeddings` for commands mirrored via the bulk `mirror_from_sqlite`
    /// path (which doesn't embed).
    pub fn upsert_embedding(&self, command: &str, embedding: &[f32]) -> Result<()> {
        let json = serde_json::to_string(embedding)
            .context("serializing embedding to JSON for command_embeddings")?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO command_embeddings (command, embedding) VALUES (?, ?) \
             ON CONFLICT(command) DO UPDATE SET embedding = excluded.embedding",
            duckdb::params![command, json],
        )?;
        Ok(())
    }

    /// Backfill `command_embeddings` for every distinct command that doesn't
    /// yet have a cached vector. Intended to run once after a bulk
    /// `mirror_from_sqlite` (which inserts raw rows but no embeddings) so the
    /// habit-memory layer has full coverage. Returns the number of embeddings
    /// inserted. No-op for commands that already have an embedding.
    pub fn backfill_embeddings(&self, embedder: &dyn embed::Embedder) -> Result<u64> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT c.command \
             FROM commands c \
             LEFT JOIN command_embeddings e ON e.command = c.command \
             WHERE e.embedding IS NULL",
        )?;
        let missing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut inserted: u64 = 0;
        for command in missing {
            let embedding = embedder.embed(&command);
            let json = serde_json::to_string(&embedding)
                .context("serializing embedding during backfill")?;
            // Re-acquire the lock per row — backfill is a batch job, not a
            // hot path, so per-row locking is fine and keeps lock hold time
            // short (each upsert is a single indexed write).
            conn.execute(
                "INSERT INTO command_embeddings (command, embedding) VALUES (?, ?) \
                 ON CONFLICT(command) DO UPDATE SET embedding = excluded.embedding",
                duckdb::params![command, json],
            )?;
            inserted += 1;
        }
        Ok(inserted)
    }

    /// Increment a locally learned correction pair. Pairing is decided by the
    /// UI, which has session boundaries and exit-code ordering; DuckDB only
    /// persists aggregate observations.
    pub fn record_command_correction(&self, typo: &str, correction: &str) -> Result<()> {
        // Defense in depth: never persist credential material, in either
        // side of the learned pair.
        let (Some(typo), Some(correction)) = (
            sanitize::sanitize_command(typo),
            sanitize::sanitize_command(correction),
        ) else {
            return Ok(());
        };
        let conn = self.conn.lock();
        let last_seen = Utc::now().to_rfc3339();
        let updated = conn.execute(
            "UPDATE command_corrections
             SET observations = observations + 1, last_seen = ?
             WHERE typo = ? AND correction = ?",
            duckdb::params![last_seen, typo, correction],
        )?;
        if updated == 0 {
            conn.execute(
                "INSERT INTO command_corrections
                    (typo, correction, observations, last_seen)
                 VALUES (?, ?, 1, ?)",
                duckdb::params![typo, correction, last_seen],
            )?;
        }
        Ok(())
    }

    /// Return learned corrections for an exact failed command, ranked by
    /// repeated local observations and recency.
    pub fn command_corrections_for(&self, typo: &str) -> Result<Vec<CommandCorrection>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT typo, correction, observations, last_seen
             FROM command_corrections
             WHERE typo = ?
             ORDER BY observations DESC, last_seen DESC, correction ASC",
        )?;
        let rows = statement
            .query_map(duckdb::params![typo], |row| {
                Ok(CommandCorrection {
                    typo: row.get(0)?,
                    correction: row.get(1)?,
                    observations: row.get::<_, i64>(2)? as u64,
                    last_seen: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every recorded typo → correction pair, sorted by observation count
    /// (most observed first) and then by recency. Used by the privacy-safe
    /// export path (`UsageHabitsReport`). Rows are passed through the
    /// sanitizer at insert time as well, so this should never contain secret
    /// material; the export layer re-checks defensively.
    pub fn all_command_corrections(&self) -> Result<Vec<CommandCorrection>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT typo, correction, observations, last_seen
             FROM command_corrections
             ORDER BY observations DESC, last_seen DESC, typo ASC, correction ASC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(CommandCorrection {
                    typo: row.get(0)?,
                    correction: row.get(1)?,
                    observations: row.get::<_, i64>(2)? as u64,
                    last_seen: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append one OneKey behavior event. `created_at` is stamped here (UTC
    /// RFC3339) when the caller leaves it empty, so callers only describe
    /// *what* happened. Inputs are identifiers/hashes produced by the UI —
    /// no sanitization pass is needed because no free-form user text enters
    /// this table.
    pub fn record_onekey_event(&self, event: &OneKeyUsageEvent) -> Result<()> {
        let conn = self.conn.lock();
        let created_at = if event.created_at.is_empty() {
            Utc::now().to_rfc3339()
        } else {
            event.created_at.clone()
        };
        conn.execute(
            "INSERT INTO onekey_events
                (connection_id, prompt_fingerprint, onekey_id, step_id,
                 action, candidates_hash, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            duckdb::params![
                event.connection_id,
                event.prompt_fingerprint,
                event.onekey_id,
                event.step_id,
                event.action,
                event.candidates_hash,
                created_at,
            ],
        )?;
        Ok(())
    }

    /// The most recent OneKey behavior events across all connections,
    /// returned oldest-first so callers can replay them into an append-only
    /// in-memory cache. `limit` caps the window (newest `limit` rows are
    /// selected, then re-ordered ascending).
    pub fn recent_onekey_events(&self, limit: u32) -> Result<Vec<OneKeyUsageEvent>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT connection_id, prompt_fingerprint, onekey_id, step_id,
                    action, candidates_hash, created_at
             FROM (
                 SELECT *, row_number() OVER (ORDER BY created_at DESC) AS rn
                 FROM onekey_events
             )
             WHERE rn <= ?
             ORDER BY created_at ASC",
        )?;
        let rows = statement
            .query_map(duckdb::params![limit], |row| {
                Ok(OneKeyUsageEvent {
                    connection_id: row.get(0)?,
                    prompt_fingerprint: row.get(1)?,
                    onekey_id: row.get(2)?,
                    step_id: row.get(3)?,
                    action: row.get(4)?,
                    candidates_hash: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Append one session-recovery replay event (see the `replay_events`
    /// schema comment for the event grammar). `ts_micros` and `seq` are
    /// captured synchronously by the caller at input time — they define the
    /// fold order, so this insert may safely run on a spawned task that
    /// lands out of order. Free-form op text goes through the credential
    /// sanitizer; secret-looking lines drop the whole event (defense in
    /// depth — the UI already refuses to record credential-prompt input).
    pub fn record_replay_event(
        &self,
        connection_id: &str,
        ts_micros: i64,
        seq: u64,
        event: &str,
        op: Option<&str>,
    ) -> Result<()> {
        let op = match op {
            Some(raw) => match sanitize::sanitize_command(raw) {
                Some(clean) => Some(clean),
                // Secret-looking payload: dropping just the op would replay
                // a hole in the sequence — drop the event entirely.
                None => return Ok(()),
            },
            None => None,
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO replay_events (connection_id, ts_micros, seq, event, op)\n             VALUES (?, ?, ?, ?, ?)",
            duckdb::params![connection_id, ts_micros, seq as i64, event, op],
        )?;
        Ok(())
    }

    /// The current replayable establishment ops for a connection: all its
    /// replay events folded in (ts_micros, seq) order. This reproduces the
    /// in-memory recorder's final state, surviving app restarts and the
    /// snapshot debounce (each event row is written as the input happens).
    pub fn latest_replay_ops(&self, connection_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut statement = conn.prepare(
            "SELECT event, op FROM replay_events\n             WHERE connection_id = ?\n             ORDER BY ts_micros ASC, seq ASC",
        )?;
        let rows = statement
            .query_map(duckdb::params![connection_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(fold_replay_events(rows))
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

    /// Per-full-command success/failure counts, ranking most-used first.
    /// Commands with only NULL exit codes count as unknown (excluded from
    /// both success and failure). Limit 0 means no limit.
    pub fn command_rankings(&self, limit: u32) -> Result<Vec<CommandRanking>> {
        let conn = self.conn.lock();
        let limit_clause = if limit == 0 {
            String::new()
        } else {
            format!("LIMIT {}", u64::from(limit))
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT
                command,
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) AS ok,
                SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) AS fail
             FROM commands
             GROUP BY command
             ORDER BY
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END)
                    + SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) DESC,
                command ASC
             {limit_clause}"
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CommandRanking {
                    command: row.get(0)?,
                    successes: row.get::<_, i64>(1)? as u64,
                    failures: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Prefix-filtered command rankings: the user's most-used commands whose
    /// text starts with `prefix` (case-insensitive), ordered by total
    /// observation count (successes + failures) descending. Used to drive
    /// history-based completion in the Send panel and to strengthen terminal
    /// suggestions with pure-frequency DuckDB data.
    ///
    /// A command is included even if some of its executions failed — the
    /// caller (suggestion pipeline) decides whether to surface it, and
    /// grouping by `command` means a single typo won't dominate the list.
    /// `limit == 0` means no limit.
    ///
    /// The `starts_with` comparison is case-insensitive via `LOWER()`. The
    /// `idx_commands_command` index doesn't help a `LOWER()`-wrapped filter,
    /// but the analytics table is small (<100k rows) and DuckDB's vectorized
    /// scan handles this in well under a millisecond, so no functional index
    /// is warranted.
    pub fn command_rankings_by_prefix(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<CommandRanking>> {
        let conn = self.conn.lock();
        let limit_clause = if limit == 0 {
            String::new()
        } else {
            format!("LIMIT {}", u64::from(limit))
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT
                command,
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) AS ok,
                SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) AS fail
             FROM commands
             WHERE LOWER(command) LIKE LOWER(?) || '%'
             GROUP BY command
             ORDER BY
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END)
                    + SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) DESC,
                command ASC
             {limit_clause}"
        ))?;
        let rows = stmt
            .query_map(duckdb::params![prefix], |row| {
                Ok(CommandRanking {
                    command: row.get(0)?,
                    successes: row.get::<_, i64>(1)? as u64,
                    failures: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Recency-weighted ("decayed") frequency ranking of the user's commands.
    ///
    /// Each command's `decayed_score` is `Σ daily_decay ^ age_days` over all
    /// its executions, where `age_days` is the integer-day age of each
    /// execution relative to `now()`. A `daily_decay` of `0.99` gives a
    /// ~69-day half-life: a command run today contributes ~1.0, one run a
    /// week ago ~0.93, one run a year ago ~0.025. This is the "usage
    /// probability" signal — it ranks *habits* (frequent AND recent) above
    /// raw frequency alone.
    ///
    /// Unlike [`Self::command_rankings`] (pure count), this answers "what is
    /// the user currently in the habit of doing?". Commands with NULL exit
    /// codes count as observations but neither success nor failure. `limit`
    /// of `0` means no limit.
    ///
    /// Scans the `commands` table once (vectorised by DuckDB, <1ms for <100k
    /// rows) — no precomputed decay column to drift out of sync.
    pub fn habit_rankings(&self, daily_decay: f64, limit: u32) -> Result<Vec<HabitRanking>> {
        let limit_clause = if limit == 0 {
            String::new()
        } else {
            format!("LIMIT {}", u64::from(limit))
        };
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT
                command,
                SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) AS ok,
                SUM(CASE WHEN exit_code IS NOT NULL AND exit_code != 0 THEN 1 ELSE 0 END) AS fail,
                COUNT(*) AS total,
                SUM(POWER(
                    CAST(? AS DOUBLE),
                    GREATEST(0, DATE_DIFF('day',
                        TRY_CAST(created_at AS TIMESTAMPTZ),
                        CAST(NOW() AS TIMESTAMPTZ)))
                )) AS decayed,
                MAX(created_at) AS last_seen
             FROM commands
             GROUP BY command
             ORDER BY decayed DESC, command ASC
             {limit_clause}"
        ))?;
        let rows = stmt
            .query_map(duckdb::params![daily_decay], |row| {
                let ok: i64 = row.get(1)?;
                let fail: i64 = row.get(2)?;
                let total: i64 = row.get(3)?;
                let decayed: Option<f64> = row.get(4)?;
                Ok(HabitRanking {
                    command: row.get(0)?,
                    decayed_score: decayed.unwrap_or(0.0),
                    total_count: total as u64,
                    successes: ok as u64,
                    failures: fail as u64,
                    last_seen: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Suggest commands the user is likely to run next, ranked by a blend of
    /// **semantic similarity** to `query_embedding` and **decayed usage
    /// frequency**. This is the core "habit memory" query: it answers
    /// "what does the user usually do that looks like *this*?".
    ///
    /// Scoring (per candidate command):
    /// ```text
    ///   freq_norm   = decayed_score / max_decayed_score   // ∈ [0, 1]
    ///   similarity  = cosine(query, cached_embedding)      // ∈ [-1, 1]
    ///   score       = alpha * max(0, similarity) + (1 - alpha) * freq_norm
    /// ```
    /// Negative similarity (semantically unrelated) contributes `0`, so a
    /// command only ranks on frequency unless it's genuinely similar to the
    /// query. Candidates with `total_count < min_observations` or
    /// `success_rate < min_success_rate` are filtered out first.
    ///
    /// Vector math is done in Rust (cosine over ≤10k cached embeddings is
    /// <10ms); the decayed score + counts come from a single DuckDB scan.
    /// Commands without a cached embedding are skipped (call
    /// [`Self::backfill_embeddings`] to populate them after a bulk mirror).
    pub fn suggest_by_context(
        &self,
        query_embedding: &[f32],
        opts: &SuggestOptions,
    ) -> Result<Vec<HabitSuggestion>> {
        let conn = self.conn.lock();
        // One round-trip: per-command decayed score, counts, last_seen, and
        // the cached embedding JSON. LEFT JOIN so commands without a cached
        // embedding are still scored on frequency (filtered out below).
        let mut stmt = conn.prepare(
            "SELECT
                c.command,
                SUM(CASE WHEN c.exit_code = 0 THEN 1 ELSE 0 END) AS ok,
                SUM(CASE WHEN c.exit_code IS NOT NULL AND c.exit_code != 0 THEN 1 ELSE 0 END) AS fail,
                COUNT(*) AS total,
                SUM(POWER(
                    CAST(? AS DOUBLE),
                    GREATEST(0, DATE_DIFF('day',
                        TRY_CAST(c.created_at AS TIMESTAMPTZ),
                        CAST(NOW() AS TIMESTAMPTZ)))
                )) AS decayed,
                MAX(c.created_at) AS last_seen,
                e.embedding
             FROM commands c
             LEFT JOIN command_embeddings e ON e.command = c.command
             GROUP BY c.command, e.embedding",
        )?;
        struct Row {
            command: String,
            ok: u64,
            fail: u64,
            total: u64,
            decayed: f64,
            last_seen: String,
            embedding_json: Option<String>,
        }
        let rows: Vec<Row> = stmt
            .query_map(duckdb::params![opts.daily_decay], |r| {
                let ok: i64 = r.get(1)?;
                let fail: i64 = r.get(2)?;
                let total: i64 = r.get(3)?;
                let decayed: Option<f64> = r.get(4)?;
                Ok(Row {
                    command: r.get(0)?,
                    ok: ok as u64,
                    fail: fail as u64,
                    total: total as u64,
                    decayed: decayed.unwrap_or(0.0),
                    last_seen: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    embedding_json: r.get::<_, Option<String>>(6)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        // Release the DB lock before the (CPU-only) vector math.
        drop(conn);

        let max_decayed = rows.iter().map(|r| r.decayed).fold(0.0f64, f64::max);
        let alpha = opts.alpha;
        let min_obs = u64::from(opts.min_observations.max(1));

        let mut suggestions: Vec<HabitSuggestion> = rows
            .into_iter()
            .filter(|r| r.total >= min_obs)
            .filter_map(|r| {
                // Skip commands without a cached embedding — can't score
                // similarity. (Frequency-only candidates would dominate
                // unrelated queries; the caller can use habit_rankings for
                // pure-frequency suggestions.)
                let json = r.embedding_json?;
                let emb: Vec<f32> = serde_json::from_str(&json).ok()?;
                let success_rate = if r.ok + r.fail == 0 {
                    0.0
                } else {
                    r.ok as f32 / (r.ok + r.fail) as f32
                };
                if success_rate < opts.min_success_rate {
                    return None;
                }
                let similarity = embed::cosine_similarity(query_embedding, &emb);
                let freq_norm = if max_decayed > 0.0 {
                    (r.decayed / max_decayed) as f32
                } else {
                    0.0
                };
                let score = alpha * similarity.max(0.0) + (1.0 - alpha) * freq_norm;
                Some(HabitSuggestion {
                    command: r.command,
                    score,
                    similarity,
                    decayed_score: r.decayed,
                    total_count: r.total,
                    success_rate,
                    last_seen: r.last_seen,
                })
            })
            .filter(|s| s.score > 0.0)
            .collect();

        // Sort by blended score desc; ties broken by raw decayed frequency
        // (prefer stronger habits) then command asc for determinism.
        suggestions.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.decayed_score
                        .partial_cmp(&a.decayed_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.command.cmp(&b.command))
        });
        if opts.limit > 0 {
            suggestions.truncate(opts.limit as usize);
        }
        Ok(suggestions)
    }

    /// Commands the user frequently fails: total observations >=
    /// min_observations AND failure share >= (1.0 - max_success_rate),
    /// sorted worst (lowest success rate, then most failures) first.
    /// Used to downgrade risky suggestions. Limit 0 means no limit.
    pub fn high_failure_commands(
        &self,
        max_success_rate: f64,
        min_observations: u64,
        limit: u32,
    ) -> Result<Vec<CommandRanking>> {
        let mut risky: Vec<CommandRanking> = self
            .command_rankings(0)?
            .into_iter()
            .filter(|r| r.total() >= min_observations && r.success_rate() <= max_success_rate)
            .collect();
        risky.sort_by(|a, b| {
            a.success_rate()
                .partial_cmp(&b.success_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.failures.cmp(&a.failures))
        });
        if limit > 0 {
            risky.truncate(limit as usize);
        }
        Ok(risky)
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
        conn.execute_batch("DELETE FROM commands; DELETE FROM command_corrections; DELETE FROM command_embeddings; DELETE FROM onekey_events; DELETE FROM replay_events;")?;
        Ok(())
    }
}

/// Folds an ordered replay-event stream into the current replayable ops —
/// the pure core of [`AnalyticsDB::latest_replay_ops`]. Event grammar:
/// `op` / `context` push their op line, `pop` removes the trailing line
/// (the user exited a `sudo -i`-style context), `reset` clears everything
/// (bastion menu reentry re-records from scratch). Unknown event kinds are
/// ignored so older binaries keep working against a newer log.
pub fn fold_replay_events(events: Vec<(String, Option<String>)>) -> Vec<String> {
    let mut ops = Vec::new();
    for (event, op) in events {
        match event.as_str() {
            "op" | "context" => {
                if let Some(op) = op {
                    ops.push(op);
                }
            }
            "pop" => {
                ops.pop();
            }
            "reset" => ops.clear(),
            _ => {}
        }
    }
    ops
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

    #[test]
    fn correction_pairs_are_upserted_and_ranked_locally() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command_correction("dockre ps", "docker ps")
            .unwrap();
        db.record_command_correction("dockre ps", "docker ps")
            .unwrap();
        db.record_command_correction("dockre ps", "dockerd ps")
            .unwrap();

        let corrections = db.command_corrections_for("dockre ps").unwrap();
        assert_eq!(corrections.len(), 2);
        assert_eq!(corrections[0].correction, "docker ps");
        assert_eq!(corrections[0].observations, 2);
        assert_eq!(corrections[1].observations, 1);
    }

    /// Seed the ranking scenario: `git status` 3 ok, `dockre ps` 2 fail,
    /// `docker ps` 5 ok + 1 fail.
    fn seed_ranking_history(db: &AnalyticsDB) {
        for c in [
            cmd("git status", Some(0), "2026-07-18T10:00:00Z"),
            cmd("git status", Some(0), "2026-07-18T10:01:00Z"),
            cmd("git status", Some(0), "2026-07-18T10:02:00Z"),
            cmd("dockre ps", Some(127), "2026-07-18T10:03:00Z"),
            cmd("dockre ps", Some(127), "2026-07-18T10:04:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:05:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:06:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:07:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:08:00Z"),
            cmd("docker ps", Some(0), "2026-07-18T10:09:00Z"),
            cmd("docker ps", Some(1), "2026-07-18T10:10:00Z"),
        ] {
            db.record_command(&c).unwrap();
        }
    }

    /// `command_rankings` must rank per-full-command success/failure counts,
    /// most-used first.
    #[test]
    fn command_rankings_orders_most_used_first() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        seed_ranking_history(&db);

        let rankings = db.command_rankings(10).unwrap();
        assert_eq!(rankings.len(), 3);

        assert_eq!(rankings[0].command, "docker ps");
        assert_eq!(rankings[0].successes, 5);
        assert_eq!(rankings[0].failures, 1);
        assert_eq!(rankings[0].total(), 6);
        assert!(
            (rankings[0].success_rate() - 5.0 / 6.0).abs() < 1e-9,
            "docker ps success_rate must be 5/6, got {}",
            rankings[0].success_rate()
        );

        assert_eq!(rankings[1].command, "git status");
        assert_eq!(rankings[1].successes, 3);
        assert_eq!(rankings[1].failures, 0);
        assert_eq!(rankings[1].total(), 3);
        assert_eq!(rankings[1].success_rate(), 1.0);

        assert_eq!(rankings[2].command, "dockre ps");
        assert_eq!(rankings[2].successes, 0);
        assert_eq!(rankings[2].failures, 2);
        assert_eq!(rankings[2].total(), 2);
        assert_eq!(rankings[2].success_rate(), 0.0);
    }

    /// Limit 0 means no limit; otherwise at most `limit` rows.
    #[test]
    fn command_rankings_limit_zero_means_no_limit() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        seed_ranking_history(&db);
        assert_eq!(db.command_rankings(0).unwrap().len(), 3);
        let top_two = db.command_rankings(2).unwrap();
        assert_eq!(top_two.len(), 2);
        assert_eq!(top_two[0].command, "docker ps");
        assert_eq!(top_two[1].command, "git status");
    }

    /// Prefix-filtered rankings: only commands starting with the prefix are
    /// returned, still ordered by total observation count. This is the query
    /// the Send panel and the strengthened terminal suggestions rely on.
    #[test]
    fn command_rankings_by_prefix_filters_and_orders_by_usage() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        seed_ranking_history(&db);

        // "d" matches both docker variants; docker ps (6 obs) beats dockre ps (2).
        let d = db.command_rankings_by_prefix("d", 0).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].command, "docker ps");
        assert_eq!(d[0].total(), 6);
        assert_eq!(d[1].command, "dockre ps");
        assert_eq!(d[1].total(), 2);

        // "docker" narrows to just docker ps.
        let docker = db.command_rankings_by_prefix("docker", 0).unwrap();
        assert_eq!(docker.len(), 1);
        assert_eq!(docker[0].command, "docker ps");

        // Case-insensitive: "GIT" matches "git status".
        let git = db.command_rankings_by_prefix("GIT", 0).unwrap();
        assert_eq!(git.len(), 1);
        assert_eq!(git[0].command, "git status");

        // No match.
        assert!(
            db.command_rankings_by_prefix("nonexistent", 0)
                .unwrap()
                .is_empty()
        );

        // Limit caps the result count.
        assert_eq!(db.command_rankings_by_prefix("d", 1).unwrap().len(), 1);
    }

    /// `high_failure_commands` must surface frequently-failed commands and
    /// exclude well-behaved ones, sorted worst-first.
    #[test]
    fn high_failure_commands_returns_frequent_failures_worst_first() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        seed_ranking_history(&db);
        // A second risky command: 1 ok, 3 fail -> success rate 0.25 <= 0.3.
        for c in [
            cmd("gti stauts", Some(0), "2026-07-18T11:00:00Z"),
            cmd("gti stauts", Some(1), "2026-07-18T11:01:00Z"),
            cmd("gti stauts", Some(1), "2026-07-18T11:02:00Z"),
            cmd("gti stauts", Some(1), "2026-07-18T11:03:00Z"),
        ] {
            db.record_command(&c).unwrap();
        }

        let risky = db.high_failure_commands(0.3, 2, 10).unwrap();
        let commands: Vec<&str> = risky.iter().map(|r| r.command.as_str()).collect();
        assert!(
            commands.contains(&"dockre ps"),
            "dockre ps (0% success, 2 obs) must be flagged, got {commands:?}"
        );
        assert!(
            commands.contains(&"gti stauts"),
            "gti stauts (25% success, 4 obs) must be flagged, got {commands:?}"
        );
        assert!(
            !commands.contains(&"docker ps"),
            "docker ps (83% success) must NOT be flagged, got {commands:?}"
        );
        assert!(
            !commands.contains(&"git status"),
            "git status (100% success) must NOT be flagged, got {commands:?}"
        );
        // Worst (lowest success rate) first: dockre ps (0.0) before gti stauts (0.25).
        assert_eq!(risky[0].command, "dockre ps");
        assert_eq!(risky[1].command, "gti stauts");
    }

    /// `high_failure_commands` must respect the observation floor — a command
    /// that failed once but has too few observations is not yet "frequently
    /// failing".
    #[test]
    fn high_failure_commands_respects_min_observations() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("dockre ps", Some(127), "2026-07-18T10:00:00Z"))
            .unwrap();
        let risky = db.high_failure_commands(0.3, 2, 10).unwrap();
        assert!(
            risky.is_empty(),
            "1 observation < min_observations=2 must not be flagged"
        );
    }

    /// Rows with NULL exit codes are unknown: they must not count as success
    /// or failure, and must not inflate a command's observation count.
    #[test]
    fn rankings_ignore_null_exit_codes() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        seed_ranking_history(&db);
        db.record_command(&cmd("mystery", None, "2026-07-18T12:00:00Z"))
            .unwrap();
        db.record_command(&cmd("git status", None, "2026-07-18T12:01:00Z"))
            .unwrap();

        let rankings = db.command_rankings(0).unwrap();
        let git = rankings.iter().find(|r| r.command == "git status").unwrap();
        assert_eq!(git.successes, 3, "NULL exit code must not count as success");
        assert_eq!(git.failures, 0, "NULL exit code must not count as failure");
        assert_eq!(git.total(), 3);

        let mystery = rankings.iter().find(|r| r.command == "mystery").unwrap();
        assert_eq!(mystery.successes, 0);
        assert_eq!(mystery.failures, 0);
        assert_eq!(mystery.total(), 0);
        assert_eq!(mystery.success_rate(), 0.0);

        // A NULL-only command has zero observations — it can't be flagged as
        // frequently failing even though its success_rate() is 0.0.
        let risky = db.high_failure_commands(0.3, 1, 10).unwrap();
        assert!(
            !risky.iter().any(|r| r.command == "mystery"),
            "NULL-only command must be excluded from high_failure_commands"
        );
    }

    /// Defense in depth: `record_command` must redact secret values before
    /// they ever reach the DuckDB file.
    #[test]
    fn record_command_redacts_glued_mysql_password() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("mysql -psecret db", Some(0), "2026-07-18T10:00:00Z"))
            .unwrap();
        let summary = db.behavior_summary().unwrap();
        assert_eq!(
            summary.most_used_command.as_deref(),
            Some("mysql -p*** db"),
            "stored command must have the password redacted"
        );
    }

    /// Defense in depth: a command line carrying PEM private key material
    /// must be dropped entirely — never persisted.
    #[test]
    fn record_command_drops_pem_private_key() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("ls", Some(0), "2026-07-18T10:00:00Z"))
            .unwrap();
        let before = db.total_commands().unwrap();

        let pem = "cat -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg==\n-----END PRIVATE KEY-----";
        db.record_command(&cmd(pem, Some(0), "2026-07-18T10:01:00Z"))
            .unwrap();
        assert_eq!(
            db.total_commands().unwrap(),
            before,
            "PEM-containing command must not be stored"
        );
    }

    /// Correction pairs pass through the sanitizer too: secrets in either
    /// side must be redacted, and PEM material must drop the pair.
    #[test]
    fn record_command_correction_is_sanitized() {
        let db = AnalyticsDB::open_in_memory().unwrap();

        // Redactable secret values are stored redacted.
        db.record_command_correction("mysql -psecret db", "mysql -p*** db")
            .unwrap();
        let corrections = db.command_corrections_for("mysql -p*** db").unwrap();
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].typo, "mysql -p*** db");
        assert_eq!(corrections[0].correction, "mysql -p*** db");
        assert!(
            db.command_corrections_for("mysql -psecret db")
                .unwrap()
                .is_empty(),
            "the raw secret must never be persisted as a typo key"
        );

        // PEM material in either side drops the whole pair.
        let pem = "-----BEGIN PRIVATE KEY-----";
        db.record_command_correction(pem, "docker ps").unwrap();
        db.record_command_correction("dockre ps", pem).unwrap();
        assert!(db.command_corrections_for("dockre ps").unwrap().is_empty());
    }

    /// `clear` must wipe all rows. Used by tests and by a future "reset
    /// analytics" UI action.
    #[test]
    fn clear_wipes_all_rows() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("ls", Some(0), "2026-07-18T10:00:00Z"))
            .unwrap();
        assert_eq!(db.total_commands().unwrap(), 1);
        db.record_command_correction("gti status", "git status")
            .unwrap();
        db.clear().unwrap();
        assert_eq!(db.total_commands().unwrap(), 0);
        assert!(db.command_corrections_for("gti status").unwrap().is_empty());
    }

    // ── OneKey behavior-event tests ─────────────────────────────────

    fn onekey_event(
        onekey_id: &str,
        action: &str,
        candidates_hash: &str,
        created_at: &str,
    ) -> OneKeyUsageEvent {
        OneKeyUsageEvent {
            connection_id: "conn-1".to_string(),
            prompt_fingerprint: "fp-1".to_string(),
            onekey_id: onekey_id.to_string(),
            step_id: format!("{onekey_id}-step"),
            action: action.to_string(),
            candidates_hash: candidates_hash.to_string(),
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn onekey_events_roundtrip_oldest_first() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_onekey_event(&onekey_event(
            "ok-a",
            "manual_select",
            "hash-1",
            "2026-08-01T10:00:00Z",
        ))
        .unwrap();
        db.record_onekey_event(&onekey_event(
            "ok-a",
            "auto_submit",
            "hash-1",
            "2026-08-02T10:00:00Z",
        ))
        .unwrap();
        db.record_onekey_event(&onekey_event(
            "ok-b",
            "rejected",
            "hash-2",
            "2026-08-03T10:00:00Z",
        ))
        .unwrap();

        let events = db.recent_onekey_events(100).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].action, "manual_select");
        assert_eq!(events[1].action, "auto_submit");
        assert_eq!(events[2].action, "rejected");
        assert_eq!(events[2].onekey_id, "ok-b");
        assert_eq!(events[2].candidates_hash, "hash-2");
    }

    #[test]
    fn recent_onekey_events_limit_keeps_the_newest_rows() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        for day in 1..=5 {
            db.record_onekey_event(&onekey_event(
                "ok-a",
                "manual_select",
                "hash-1",
                &format!("2026-08-0{day}T10:00:00Z"),
            ))
            .unwrap();
        }
        let events = db.recent_onekey_events(2).unwrap();
        assert_eq!(events.len(), 2);
        // Newest two, still ordered oldest-first.
        assert_eq!(events[0].created_at, "2026-08-04T10:00:00Z");
        assert!(events[0].created_at < events[1].created_at);
    }

    #[test]
    fn record_onekey_event_stamps_missing_created_at() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_onekey_event(&onekey_event("ok-a", "popup_shown", "hash-1", ""))
            .unwrap();
        let events = db.recent_onekey_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert!(!events[0].created_at.is_empty());
    }

    #[test]
    fn clear_wipes_onekey_events() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_onekey_event(&onekey_event(
            "ok-a",
            "manual_select",
            "hash-1",
            "2026-08-01T10:00:00Z",
        ))
        .unwrap();
        db.clear().unwrap();
        assert!(db.recent_onekey_events(10).unwrap().is_empty());
    }

    // ── session-recovery replay-event tests ─────────────────────────

    /// The full jumpserver + sudo round trip: menu ops and the context
    /// command fold back in exact (ts, seq) submission order, and a `pop`
    /// (exit after sudo -i) removes only the escalation.
    #[test]
    fn replay_events_fold_in_submission_order() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_replay_event("conn-1", 1_000, 1, "op", Some("/q"))
            .unwrap();
        db.record_replay_event("conn-1", 2_000, 2, "op", Some("2"))
            .unwrap();
        db.record_replay_event("conn-1", 3_000, 3, "context", Some("sudo -i"))
            .unwrap();
        assert_eq!(
            db.latest_replay_ops("conn-1").unwrap(),
            vec!["/q", "2", "sudo -i"]
        );
        db.record_replay_event("conn-1", 4_000, 4, "pop", None)
            .unwrap();
        assert_eq!(db.latest_replay_ops("conn-1").unwrap(), vec!["/q", "2"]);
        // Other connections are unaffected.
        assert!(db.latest_replay_ops("conn-2").unwrap().is_empty());
    }

    /// Rows may be INSERTed out of order (spawned tasks) — the fold orders
    /// by (ts_micros, seq), so the result is still the user's input order.
    #[test]
    fn replay_events_out_of_order_inserts_still_fold_by_timestamp() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_replay_event("conn-1", 2_000, 2, "op", Some("11"))
            .unwrap();
        db.record_replay_event("conn-1", 1_000, 1, "op", Some("/cao"))
            .unwrap();
        db.record_replay_event("conn-1", 3_000, 3, "op", Some("2"))
            .unwrap();
        assert_eq!(
            db.latest_replay_ops("conn-1").unwrap(),
            vec!["/cao", "11", "2"]
        );
    }

    /// A `reset` event (bastion menu reentry thawed the recorder) discards
    /// everything before it — only the navigation recorded afterwards
    /// replays, matching the in-memory "last selection wins" semantics.
    #[test]
    fn replay_events_reset_starts_a_fresh_recording() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_replay_event("conn-1", 1_000, 1, "op", Some("/q"))
            .unwrap();
        db.record_replay_event("conn-1", 2_000, 2, "op", Some("2"))
            .unwrap();
        db.record_replay_event("conn-1", 3_000, 3, "reset", None)
            .unwrap();
        db.record_replay_event("conn-1", 4_000, 4, "op", Some("/w"))
            .unwrap();
        db.record_replay_event("conn-1", 5_000, 5, "op", Some("3"))
            .unwrap();
        assert_eq!(db.latest_replay_ops("conn-1").unwrap(), vec!["/w", "3"]);
    }

    /// Secret-looking op payloads drop the whole event (never a hole in the
    /// sequence), unknown event kinds are ignored by the fold, and `clear`
    /// wipes the table.
    #[test]
    fn replay_events_are_sanitized_and_clearable() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_replay_event("conn-1", 1_000, 1, "op", Some("web-01"))
            .unwrap();
        // A PEM header is the sanitizer's canonical "secret" — dropped.
        db.record_replay_event(
            "conn-1",
            2_000,
            2,
            "op",
            Some("-----BEGIN RSA PRIVATE KEY-----"),
        )
        .unwrap();
        assert_eq!(db.latest_replay_ops("conn-1").unwrap(), vec!["web-01"]);
        assert_eq!(
            fold_replay_events(vec![
                ("op".to_string(), Some("a".to_string())),
                ("future-kind".to_string(), Some("b".to_string())),
                ("pop".to_string(), None),
                // pop on empty is a no-op, not a panic.
                ("pop".to_string(), None),
            ]),
            Vec::<String>::new()
        );
        db.clear().unwrap();
        assert!(db.latest_replay_ops("conn-1").unwrap().is_empty());
    }

    // ── habit-memory / vector-storage tests ─────────────────────────────

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn days_ago_rfc3339(days: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339()
    }

    #[test]
    fn record_command_embedded_caches_embedding() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        db.record_command_embedded(&cmd("git status", Some(0), &now_rfc3339()), &emb)
            .unwrap();
        // The cached embedding must equal what the embedder produces.
        let conn = db.conn.lock();
        let json: String = conn
            .query_row(
                "SELECT embedding FROM command_embeddings WHERE command = 'git status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let cached: Vec<f32> = serde_json::from_str(&json).unwrap();
        let expected = emb.embed("git status");
        assert_eq!(cached.len(), expected.len());
        for (a, b) in cached.iter().zip(expected.iter()) {
            assert!(
                (a - b).abs() < 1e-6,
                "cached embedding must match embedder output"
            );
        }
    }

    #[test]
    fn record_command_embedded_drops_secret_commands() {
        // A PEM-key command is dropped by the sanitizer — it must NOT be
        // recorded in commands NOR get a cached embedding.
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        let pem = "cat -----BEGIN PRIVATE KEY-----\nMIIEvQ==";
        db.record_command_embedded(&cmd(pem, Some(0), &now_rfc3339()), &emb)
            .unwrap();
        assert_eq!(db.total_commands().unwrap(), 0);
        let conn = db.conn.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM command_embeddings", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(n, 0, "no embedding should be cached for a dropped command");
    }

    #[test]
    fn upsert_embedding_is_idempotent() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        let v = vec![0.1, 0.2, 0.3];
        db.upsert_embedding("ls", &v).unwrap();
        db.upsert_embedding("ls", &v).unwrap();
        let conn = db.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM command_embeddings WHERE command = 'ls'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(n, 1, "upsert must not duplicate rows");
    }

    #[test]
    fn backfill_embeddings_fills_missing() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        // Record via the non-embedding path (simulates bulk mirror).
        db.record_command(&cmd("git status", Some(0), &now_rfc3339()))
            .unwrap();
        db.record_command(&cmd("docker ps", Some(0), &now_rfc3339()))
            .unwrap();
        let emb = HashEmbedder::new();
        let filled = db.backfill_embeddings(&emb).unwrap();
        assert_eq!(filled, 2);
        // Second backfill is a no-op.
        let again = db.backfill_embeddings(&emb).unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn habit_rankings_decays_by_recency() {
        // "ls" was run 1000 days ago; "pwd" was run today. With a 0.99 daily
        // decay, the 1000-day-old command contributes ~0.99^1000 ≈ 4e-5,
        // effectively 0 — so "pwd" must rank above "ls" even though both have
        // one execution.
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("ls", Some(0), &days_ago_rfc3339(1000)))
            .unwrap();
        db.record_command(&cmd("pwd", Some(0), &now_rfc3339()))
            .unwrap();
        let rankings = db.habit_rankings(0.99, 0).unwrap();
        assert_eq!(rankings.len(), 2);
        assert_eq!(rankings[0].command, "pwd", "recent command must rank first");
        assert!(
            rankings[0].decayed_score > rankings[1].decayed_score,
            "recent decayed score {} must beat old {}",
            rankings[0].decayed_score,
            rankings[1].decayed_score
        );
        // Today's command contributes ~1.0; 1000-day-old ~0.
        assert!(
            (rankings[0].decayed_score - 1.0).abs() < 0.01,
            "today's decayed score ≈ 1.0, got {}",
            rankings[0].decayed_score
        );
        assert!(
            rankings[1].decayed_score < 0.001,
            "1000-day-old decayed score ≈ 0, got {}",
            rankings[1].decayed_score
        );
    }

    #[test]
    fn habit_rankings_counts_total_including_null_exit_codes() {
        // Two executions of "ls": one ok, one NULL exit. total must be 2,
        // successes 1, failures 0 (NULL is neither).
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("ls", Some(0), &now_rfc3339()))
            .unwrap();
        db.record_command(&cmd("ls", None, &now_rfc3339())).unwrap();
        let r = db.habit_rankings(0.99, 0).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].total_count, 2);
        assert_eq!(r[0].successes, 1);
        assert_eq!(r[0].failures, 0);
    }

    #[test]
    fn suggest_by_context_ranks_similar_higher() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        // Seed two habits: a git one and a docker one.
        for _ in 0..5 {
            db.record_command_embedded(&cmd("git status", Some(0), &now_rfc3339()), &emb)
                .unwrap();
        }
        for _ in 0..5 {
            db.record_command_embedded(&cmd("docker ps", Some(0), &now_rfc3339()), &emb)
                .unwrap();
        }
        // Query with something git-like — "git status" must outrank "docker ps".
        let q = emb.embed("git log");
        let sugg = db
            .suggest_by_context(&q, &SuggestOptions::default())
            .unwrap();
        assert!(!sugg.is_empty());
        assert_eq!(sugg[0].command, "git status");
        assert!(sugg[0].similarity > 0.0);
        // docker ps may or may not appear (its similarity could be ≤0 → filtered);
        // but if it does, git status must be above it.
        if let Some(docker) = sugg.iter().find(|s| s.command == "docker ps") {
            assert!(
                sugg[0].score >= docker.score,
                "git status must outrank docker ps for a git-like query"
            );
        }
    }

    #[test]
    fn suggest_by_context_frequency_boosts_habits() {
        // With alpha=0 (pure frequency), the more-frequent command wins
        // regardless of similarity.
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        for _ in 0..10 {
            db.record_command_embedded(&cmd("ls", Some(0), &now_rfc3339()), &emb)
                .unwrap();
        }
        for _ in 0..1 {
            db.record_command_embedded(&cmd("pwd", Some(0), &now_rfc3339()), &emb)
                .unwrap();
        }
        let q = emb.embed("pwd"); // semantically biased toward pwd
        let pure_freq = SuggestOptions {
            alpha: 0.0,
            ..SuggestOptions::default()
        };
        let sugg = db.suggest_by_context(&q, &pure_freq).unwrap();
        assert!(!sugg.is_empty());
        // "ls" has 10x the decayed frequency, so with alpha=0 it must win.
        assert_eq!(sugg[0].command, "ls");
    }

    #[test]
    fn suggest_by_context_filters_low_success_rate() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        // "badcmd" fails every time.
        for _ in 0..5 {
            db.record_command_embedded(&cmd("badcmd", Some(1), &now_rfc3339()), &emb)
                .unwrap();
        }
        let q = emb.embed("badcmd");
        let strict = SuggestOptions {
            min_success_rate: 0.5,
            ..SuggestOptions::default()
        };
        let sugg = db.suggest_by_context(&q, &strict).unwrap();
        assert!(
            sugg.iter().all(|s| s.command != "badcmd"),
            "0% success-rate command must be filtered out"
        );
    }

    #[test]
    fn suggest_by_context_skips_commands_without_embedding() {
        // Record via the non-embedding path, so command_embeddings is empty.
        let db = AnalyticsDB::open_in_memory().unwrap();
        db.record_command(&cmd("git status", Some(0), &now_rfc3339()))
            .unwrap();
        let q = HashEmbedder::new().embed("git");
        let sugg = db
            .suggest_by_context(&q, &SuggestOptions::default())
            .unwrap();
        assert!(
            sugg.is_empty(),
            "commands without a cached embedding must be skipped"
        );
    }

    #[test]
    fn clear_wipes_embeddings() {
        let db = AnalyticsDB::open_in_memory().unwrap();
        let emb = HashEmbedder::new();
        db.record_command_embedded(&cmd("ls", Some(0), &now_rfc3339()), &emb)
            .unwrap();
        db.clear().unwrap();
        let conn = db.conn.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM command_embeddings", [], |r| r.get(0))
            .unwrap();
        drop(conn);
        assert_eq!(n, 0, "clear() must wipe command_embeddings too");
    }
}
