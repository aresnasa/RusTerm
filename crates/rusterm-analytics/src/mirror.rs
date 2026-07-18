//! Mirror history data from SQLite (OLTP) → DuckDB (OLAP).
//!
//! The mirror is a one-way bulk copy: read all rows from `rusterm_db`'s
//! `history` table and insert them into DuckDB's `commands` table, replacing
//! any prior data. Used on app launch (to bring the analytics DB up to date)
//! and on manual "refresh analytics" UI action.
//!
//! Incremental updates (single-row inserts as new commands are recorded)
//! go through `AnalyticsDB::record_command` directly — this function is
//! for the bulk refresh path only.

use anyhow::{Context, Result};

use crate::{AnalyticsCommand, AnalyticsDB};

/// Bulk-mirror all rows from the given SQLite `rusterm-db` Database into
/// the given DuckDB `AnalyticsDB`. Wipes the DuckDB `commands` table first
/// so the result is a consistent snapshot of SQLite's current state.
///
/// This is a synchronous function — it blocks on both the SQLite query
/// (async) and the DuckDB bulk insert (sync). Callers should run it in a
/// `spawn_blocking` or a dedicated task so it doesn't block the UI thread.
///
/// Returns the number of rows mirrored.
pub async fn mirror_from_sqlite(
    analytics: &AnalyticsDB,
    sqlite_db: &rusterm_db::Database,
) -> Result<u64> {
    // Fetch all rows from SQLite. We deliberately don't stream — the typical
    // history table is <100k rows, which fits comfortably in memory.
    let entries = sqlite_db
        .all_history()
        .await
        .context("fetching all history from sqlite for mirror")?;

    // Wipe the DuckDB table first so the result is a consistent snapshot.
    analytics.clear().context("clearing analytics table before mirror")?;

    // Bulk-insert into DuckDB. We don't use DuckDB's Appender API here
    // because it's harder to make panic-safe across the iteration; the
    // per-row insert is fast enough for our row counts (<100k).
    let mut inserted: u64 = 0;
    for entry in entries {
        let cmd = AnalyticsCommand {
            command: entry.command,
            hostname: entry.hostname,
            exit_code: entry.exit_code,
            created_at: entry.created_at,
        };
        if let Err(e) = analytics.record_command(&cmd) {
            tracing::warn!(
                "mirror_from_sqlite: failed to insert row into duckdb (skipping): {}",
                e
            );
            continue;
        }
        inserted += 1;
    }

    tracing::info!(
        "mirror_from_sqlite: mirrored {} commands from sqlite to duckdb",
        inserted
    );
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mirror_copies_all_rows_from_sqlite() {
        // Set up a temporary SQLite DB with a few rows
        let tmp = tempfile::tempdir().unwrap();
        let sqlite_path = tmp.path().join("test.db");
        let sqlite_db = rusterm_db::Database::open(Some(sqlite_path))
            .await
            .unwrap();

        let entries = vec![
            rusterm_db::HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                command: "git status".to_string(),
                session_id: "test".to_string(),
                cwd: None,
                hostname: Some("local".to_string()),
                exit_code: Some(0),
                duration_ms: None,
                created_at: "2026-07-18T10:00:00Z".to_string(),
            },
            rusterm_db::HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                command: "pwdwd".to_string(),
                session_id: "test".to_string(),
                cwd: None,
                hostname: Some("local".to_string()),
                exit_code: Some(127), // failed
                duration_ms: None,
                created_at: "2026-07-18T10:01:00Z".to_string(),
            },
        ];
        sqlite_db.save_history_batch(entries).await.unwrap();

        // Mirror into an in-memory DuckDB
        let analytics = AnalyticsDB::open_in_memory().unwrap();
        let count = mirror_from_sqlite(&analytics, &sqlite_db).await.unwrap();
        assert_eq!(count, 2);
        assert_eq!(analytics.total_commands().unwrap(), 2);

        // The behavior summary should reflect the mirror
        let summary = analytics.behavior_summary().unwrap();
        assert_eq!(summary.total_commands, 2);
        assert_eq!(summary.known_failed_commands, 1);
        assert_eq!(summary.most_used_category, Some(crate::CommandCategory::Git));
    }

    #[tokio::test]
    async fn mirror_replaces_prior_data() {
        // First mirror with one row
        let tmp = tempfile::tempdir().unwrap();
        let sqlite_path = tmp.path().join("test.db");
        let sqlite_db = rusterm_db::Database::open(Some(sqlite_path))
            .await
            .unwrap();
        sqlite_db
            .save_history_batch(vec![rusterm_db::HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                command: "ls".to_string(),
                session_id: "test".to_string(),
                cwd: None,
                hostname: Some("local".to_string()),
                exit_code: Some(0),
                duration_ms: None,
                created_at: "2026-07-18T10:00:00Z".to_string(),
            }])
            .await
            .unwrap();
        let analytics = AnalyticsDB::open_in_memory().unwrap();
        mirror_from_sqlite(&analytics, &sqlite_db).await.unwrap();
        assert_eq!(analytics.total_commands().unwrap(), 1);

        // Second mirror — the SQLite DB now has 3 rows. The DuckDB table
        // must be wiped and re-populated, NOT appended to.
        sqlite_db
            .save_history_batch(vec![
                rusterm_db::HistoryEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    command: "git status".to_string(),
                    session_id: "test".to_string(),
                    cwd: None,
                    hostname: Some("local".to_string()),
                    exit_code: Some(0),
                    duration_ms: None,
                    created_at: "2026-07-18T10:01:00Z".to_string(),
                },
                rusterm_db::HistoryEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    command: "pwd".to_string(),
                    session_id: "test".to_string(),
                    cwd: None,
                    hostname: Some("local".to_string()),
                    exit_code: Some(0),
                    duration_ms: None,
                    created_at: "2026-07-18T10:02:00Z".to_string(),
                },
            ])
            .await
            .unwrap();
        mirror_from_sqlite(&analytics, &sqlite_db).await.unwrap();
        // 3 rows total (the first `ls` + 2 new), NOT 1+3=4
        assert_eq!(
            analytics.total_commands().unwrap(),
            3,
            "mirror must replace prior data, not append"
        );
    }
}
