use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::HistoryMatch;

pub struct AtuinDbProvider {
    db_path: PathBuf,
}

impl AtuinDbProvider {
    pub fn new() -> Option<Self> {
        let path = dirs::data_dir()?.join("atuin").join("history.db");
        if path.exists() {
            Some(Self { db_path: path })
        } else {
            None
        }
    }

    /// Search with frecency ranking: frequency + recency combined.
    /// Groups by command, counts executions, ranks by a combined score.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
        let conn = Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;

        let now_secs = Utc::now().timestamp();
        let pattern = format!("{}%", query);
        let mut stmt = conn.prepare(
            "SELECT command, cwd, hostname, MAX(timestamp) as ts, COUNT(*) as cnt
             FROM history
             WHERE command LIKE ?1 AND deleted_at IS NULL
             GROUP BY command
             ORDER BY (LN(COUNT(*) + 1) * 20.0) +
                      CASE WHEN (timestamp / 1000000000 - ?3) < 3600 THEN 90.0
                           WHEN (timestamp / 1000000000 - ?3) < 86400 THEN 70.0
                           WHEN (timestamp / 1000000000 - ?3) < 259200 THEN 50.0
                           WHEN (timestamp / 1000000000 - ?3) < 604800 THEN 30.0
                           WHEN (timestamp / 1000000000 - ?3) < 2592000 THEN 15.0
                           ELSE 5.0 END DESC
             LIMIT ?2",
        )?;

        let matches: Vec<HistoryMatch> = stmt
            .query_map(rusqlite::params![pattern, limit, now_secs], |row| {
                let command: String = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                let hostname: Option<String> = row.get(2)?;
                let ts_ns: i64 = row.get(3)?;
                let count: i64 = row.get(4)?;
                Ok((command, cwd, hostname, ts_ns, count))
            })?
            .filter_map(|r| r.ok())
            .map(|(command, cwd, hostname, ts_ns, count)| {
                let timestamp = DateTime::from_timestamp_nanos(ts_ns);
                let age_hours = (Utc::now() - timestamp).num_hours().max(1) as f32;
                let recency_score = 1.0 / (1.0 + age_hours / 24.0);
                let frequency_score = (count as f32).ln() * 20.0;
                HistoryMatch::new(
                    command,
                    cwd,
                    hostname,
                    Some(timestamp),
                    recency_score + frequency_score,
                )
            })
            .collect();

        Ok(matches)
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryMatch>> {
        let conn = Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;

        let mut stmt = conn.prepare(
            "SELECT command, cwd, hostname, timestamp FROM history
             WHERE deleted_at IS NULL
             ORDER BY timestamp DESC LIMIT ?1",
        )?;

        let matches: Vec<HistoryMatch> = stmt
            .query_map(rusqlite::params![limit], |row| {
                let command: String = row.get(0)?;
                let cwd: Option<String> = row.get(1)?;
                let hostname: Option<String> = row.get(2)?;
                let ts_ns: i64 = row.get(3)?;
                Ok((command, cwd, hostname, ts_ns))
            })?
            .filter_map(|r| r.ok())
            .map(|(command, cwd, hostname, ts_ns)| {
                let timestamp = DateTime::from_timestamp_nanos(ts_ns);
                HistoryMatch::new(
                    command,
                    cwd,
                    hostname,
                    Some(timestamp),
                    1.0,
                )
            })
            .collect();

        Ok(matches)
    }
}
