use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::history::HistoryEntry;
use crate::schema::INIT_SQL;

#[derive(Clone)]
pub struct Database {
    conn: Arc<tokio::sync::Mutex<Connection>>,
}

impl Database {
    pub async fn open(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let path = path.unwrap_or_else(|| {
            let data_dir = dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("rusterm");
            std::fs::create_dir_all(&data_dir).ok();
            data_dir.join("rusterm.db")
        });

        let conn = Connection::open(&path).await?;
        let db = Self {
            conn: Arc::new(tokio::sync::Mutex::new(conn)),
        };

        db.init_schema().await?;
        Ok(db)
    }

    async fn init_schema(&self) -> anyhow::Result<()> {
        let sql = INIT_SQL.to_string();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                // The bundled SQLite is NOT compiled with
                // SQLITE_ENABLE_MATH_FUNCTIONS, so LN()/LOG() are unavailable
                // out of the box. The frecency ranking in search_history()
                // relies on LN(), so register it as a custom scalar function.
                conn.create_scalar_function(
                    "ln",
                    1,
                    rusqlite::functions::FunctionFlags::SQLITE_UTF8
                        | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                    |ctx| {
                        let x: f64 = ctx.get(0)?;
                        Ok(x.ln())
                    },
                )?;
                conn.execute_batch(&sql)?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Schema init error: {:?}", e))?;
        Ok(())
    }

    // --- Connection Config ---

    pub async fn save_connection(
        &self,
        id: &str,
        name: &str,
        kind: &str,
        config: &str,
        group: Option<&str>,
        tags: &str,
        onekey: bool,
    ) -> anyhow::Result<()> {
        let id = id.to_string();
        let name = name.to_string();
        let kind = kind.to_string();
        let config = config.to_string();
        let group = group.map(String::from);
        let tags = tags.to_string();
        let onekey = onekey as i32;
        let now = chrono::Utc::now().to_rfc3339();

        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO connections (id, name, kind, config, group_name, tags, onekey, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![id, name, kind, config, group, tags, onekey, now, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Save error: {:?}", e))?;
        Ok(())
    }

    pub async fn list_connections(&self) -> anyhow::Result<Vec<(String, String, String, String)>> {
        self.conn
            .lock()
            .await
            .call::<_, Vec<(String, String, String, String)>, rusqlite::Error>(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT id, name, kind, config FROM connections ORDER BY name")?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(rows)
            })
            .await
            .map_err(|e| anyhow::anyhow!("List error: {:?}", e))
    }

    pub async fn delete_connection(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute("DELETE FROM connections WHERE id = ?1", params![id])?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Delete error: {:?}", e))?;
        Ok(())
    }

    // --- Command History ---

    pub async fn save_history(&self, entry: HistoryEntry) -> anyhow::Result<()> {
        let id = entry.id;
        let command = entry.command;
        let session_id = entry.session_id;
        let cwd = entry.cwd;
        let hostname = entry.hostname;
        let exit_code = entry.exit_code;
        let duration_ms = entry.duration_ms;
        let created_at = entry.created_at;

        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute(
                    "INSERT INTO history (id, command, session_id, cwd, hostname, exit_code, duration_ms, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![id, command, session_id, cwd, hostname, exit_code, duration_ms, created_at],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("History save error: {:?}", e))?;
        Ok(())
    }

    /// Delete a single history entry by id. Used to un-record a command that
    /// exited non-zero (OSC 133;D), so failed commands aren't suggested.
    pub async fn delete_history(&self, id: String) -> anyhow::Result<()> {
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute("DELETE FROM history WHERE id = ?1", params![id])?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("History delete error: {:?}", e))?;
        Ok(())
    }

    /// Bulk-insert many history entries in a single transaction.
    /// Used for importing remote shell history after an SSH connection.
    pub async fn save_history_batch(&self, entries: Vec<HistoryEntry>) -> anyhow::Result<()> {
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO history (id, command, session_id, cwd, hostname, exit_code, duration_ms, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )?;
                    for e in &entries {
                        stmt.execute(params![
                            e.id, e.command, e.session_id, e.cwd, e.hostname,
                            e.exit_code, e.duration_ms, e.created_at,
                        ])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Batch history save error: {:?}", e))?;
        Ok(())
    }

    /// Mark a command as failed by saving a row with the given non-zero
    /// `exit_code`. Replaces any existing rows for the same command (across
    /// all hosts) so the `HAVING` clause in `search_history` filters it out:
    ///   SUM(exit_code = 0) > 0  → false (no successful run recorded)
    ///   SUM(exit_code IS NOT NULL) = 0  → false (this row is NOT NULL)
    /// The command stays in the DB as a durable "known-failed" marker so that
    /// subsequent history imports (which read `~/.bash_history`) can skip it —
    /// `~/.bash_history` contains failed commands too, and without this
    /// marker the import would re-introduce them as `exit_code = NULL`, which
    /// the `HAVING` clause would keep ("unknown, assume success").
    ///
    /// If the user later runs the same command successfully, `save_history`
    /// with `exit_code = Some(0)` adds a successful row, and the `HAVING`
    /// clause will then keep the command (at least one success).
    pub async fn mark_command_failed(&self, command: &str, exit_code: i32) -> anyhow::Result<()> {
        let command = command.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                let tx = conn.transaction()?;
                // Remove every existing row for this command (success, failure,
                // or NULL-import) so there's a single authoritative record.
                // Without this, a prior NULL import would co-exist with the
                // failure marker and the HAVING clause would keep the command
                // ("all NULL" branch triggers).
                tx.execute(
                    "DELETE FROM history WHERE command = ?1",
                    params![command],
                )?;
                tx.execute(
                    "INSERT INTO history (id, command, session_id, cwd, hostname, exit_code, duration_ms, created_at) \
                     VALUES (?1, ?2, '', NULL, NULL, ?3, NULL, ?4)",
                    params![id, command, exit_code, now],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Mark failed error: {:?}", e))?;
        Ok(())
    }

    /// Return the set of commands currently marked as failed
    /// (`exit_code IS NOT NULL AND exit_code != 0`) with no successful
    /// run recorded. Used by history imports to skip known-failed commands
    /// that would otherwise be re-introduced from `~/.bash_history` as
    /// `exit_code = NULL` (which the HAVING clause keeps as "unknown").
    pub async fn known_failed_commands(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let rows: Vec<String> = self
            .conn
            .lock()
            .await
            .call::<_, Vec<String>, rusqlite::Error>(move |conn| {
                // A command is "known failed" iff every recorded row for it has
                // a non-zero exit_code (i.e. no NULL rows and no zero rows).
                // This matches the HAVING clause's negation: NOT (
                //   SUM(exit_code = 0) > 0
                //   OR SUM(exit_code IS NOT NULL) = 0
                // )
                let mut stmt = conn.prepare(
                    "SELECT command FROM history \
                     GROUP BY command \
                     HAVING SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) = 0 \
                        AND SUM(CASE WHEN exit_code IS NOT NULL THEN 1 ELSE 0 END) > 0",
                )?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(rows)
            })
            .await
            .map_err(|e| anyhow::anyhow!("Known failed query error: {:?}", e))?;
        Ok(rows.into_iter().collect())
    }

    /// Delete all history rows with the given command string.
    ///
    /// Kept for backwards compatibility and tests, but the runtime code now
    /// uses `mark_command_failed` instead of deleting — deletion would let
    /// a subsequent history import re-introduce the failed command from
    /// `~/.bash_history` as `exit_code = NULL`, which the HAVING clause
    /// would keep as "unknown, assume success".
    pub async fn delete_history_by_command(&self, command: &str) -> anyhow::Result<()> {
        let command = command.to_string();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute("DELETE FROM history WHERE command = ?1", params![command])?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Delete by command error: {:?}", e))?;
        Ok(())
    }

    /// Delete all history rows tagged with the given hostname.
    /// Used to refresh imported remote history without accumulating duplicates.
    pub async fn delete_history_by_hostname(&self, hostname: &str) -> anyhow::Result<()> {
        let hostname = hostname.to_string();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute("DELETE FROM history WHERE hostname = ?1", params![hostname])?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Delete by hostname error: {:?}", e))?;
        Ok(())
    }

    /// Return ALL history rows, unfiltered, without grouping or ranking.
    ///
    /// Used by the analytics mirror (`rusterm-analytics::mirror_from_sqlite`)
    /// to bulk-copy the OLTP store into DuckDB for OLAP queries. This is a
    /// full table scan — it does NOT apply the `HAVING` clause that
    /// `search_history` uses to filter failed commands, because analytics
    /// queries (success rate, known-failed count) need to see every row,
    /// including failures.
    ///
    /// For typical usage, this returns <100k rows and fits comfortably in
    /// memory. If history grows beyond that, callers should switch to
    /// streaming (cursor-based) reads.
    pub async fn all_history(&self) -> anyhow::Result<Vec<HistoryEntry>> {
        let rows: Vec<HistoryEntry> = self
            .conn
            .lock()
            .await
            .call::<_, Vec<HistoryEntry>, rusqlite::Error>(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, command, session_id, cwd, hostname, exit_code, \
                     duration_ms, created_at FROM history ORDER BY created_at ASC",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(HistoryEntry {
                            id: row.get(0)?,
                            command: row.get(1)?,
                            session_id: row.get(2)?,
                            cwd: row.get(3)?,
                            hostname: row.get(4)?,
                            exit_code: row.get(5)?,
                            duration_ms: row.get(6)?,
                            created_at: row.get(7)?,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(rows)
            })
            .await
            .map_err(|e| anyhow::anyhow!("All history query error: {:?}", e))?;
        Ok(rows)
    }

    /// Search command history with frecency ranking (frequency + recency + success).
    /// Groups by command, counts executions, and ranks by a combined
    /// score of frequency, recency, and success rate inspired by atuin.
    ///
    /// Only suggests commands that have succeeded at least once (exit_code = 0).
    /// Commands whose every recorded execution failed (exit_code != 0) are
    /// filtered out — the user doesn't want their typos and broken commands
    /// popping up as suggestions. Commands with NULL exit_code (local shell
    /// import, where shell integration couldn't capture the exit code) are
    /// kept — we can't know they failed, so assume success.
    pub async fn search_history(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<HistoryEntry>> {
        let query = query.to_string();
        let limit = limit;
        let now_secs = chrono::Utc::now().timestamp();

        // Frecency ranking formula (inspired by atuin's daemon mode):
        //   frequency_score = LN(count + 1) * 20.0    (diminishing returns)
        //   recency_score = tiered buckets by age
        //   success_score = successful_count / total_count * 10.0
        //   final_score = frequency_score + recency_score + success_score
        let order_expr = "(LN(COUNT(*) + 1) * 20.0) + \
         CASE WHEN (unixepoch(MAX(h.created_at)) - ?3) < 3600 THEN 90.0 \
              WHEN (unixepoch(MAX(h.created_at)) - ?3) < 86400 THEN 70.0 \
              WHEN (unixepoch(MAX(h.created_at)) - ?3) < 259200 THEN 50.0 \
              WHEN (unixepoch(MAX(h.created_at)) - ?3) < 604800 THEN 30.0 \
              WHEN (unixepoch(MAX(h.created_at)) - ?3) < 2592000 THEN 15.0 \
              ELSE 5.0 END + \
         (CAST(SUM(CASE WHEN h.exit_code = 0 THEN 1 ELSE 0 END) AS REAL) / COUNT(*) * 10.0) DESC";

        // HAVING clause: keep commands that have succeeded at least once, OR
        // whose exit_code was never recorded (NULL — local shell import).
        // This drops commands whose every execution failed (typos, broken
        // commands) — the user doesn't want those suggested.
        //
        //   SUM(CASE WHEN h.exit_code = 0 THEN 1 ELSE 0 END) > 0
        //     → at least one successful execution
        //   SUM(CASE WHEN h.exit_code IS NOT NULL THEN 1 ELSE 0 END) = 0
        //     → no recorded exit codes at all (all NULL) — keep (unknown)
        let having_expr = "HAVING \
         (SUM(CASE WHEN h.exit_code = 0 THEN 1 ELSE 0 END) > 0 \
          OR SUM(CASE WHEN h.exit_code IS NOT NULL THEN 1 ELSE 0 END) = 0)";

        self.conn
            .lock()
            .await
            .call::<_, Vec<HistoryEntry>, rusqlite::Error>(move |conn| {
                let sql = if query.is_empty() {
                    format!(
                        "SELECT h.id, h.command, h.session_id, h.cwd, h.hostname, h.exit_code, h.duration_ms, h.created_at
                         FROM history h
                         GROUP BY h.command
                         {having_expr}
                         ORDER BY {order_expr}
                         LIMIT ?2"
                    )
                } else {
                    // Use LIKE prefix matching instead of FTS5 MATCH.
                    // FTS5 tokenizes text, so MATCH 'l' won't match 'ls' —
                    // it only matches the exact token 'l'. LIKE 'l%' matches
                    // any command starting with 'l', which is what the
                    // suggestion popup needs. The idx_history_command_created
                    // index covers this query efficiently.
                    format!(
                        "SELECT h.id, h.command, h.session_id, h.cwd, h.hostname, h.exit_code, h.duration_ms, h.created_at
                         FROM history h
                         WHERE LOWER(h.command) LIKE LOWER(?1) || '%'
                         GROUP BY h.command
                         {having_expr}
                         ORDER BY {order_expr}
                         LIMIT ?2"
                    )
                };

                let mut stmt = conn.prepare(&sql)?;
                // Both branches bind params in the same order (?,?2,?3) so the
                // shared order_expr (which references ?3) resolves correctly.
                // In the empty-query branch ?1 is unused by the SQL, so binding
                // the (empty) query string there is harmless.
                let rows: Vec<HistoryEntry> = if query.is_empty() {
                    stmt.query_map(params![query, limit, now_secs], |row| {
                        Ok(HistoryEntry {
                            id: row.get(0)?,
                            command: row.get(1)?,
                            session_id: row.get(2)?,
                            cwd: row.get(3)?,
                            hostname: row.get(4)?,
                            exit_code: row.get(5)?,
                            duration_ms: row.get(6)?,
                            created_at: row.get(7)?,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect()
                } else {
                    stmt.query_map(params![query, limit, now_secs], |row| {
                        Ok(HistoryEntry {
                            id: row.get(0)?,
                            command: row.get(1)?,
                            session_id: row.get(2)?,
                            cwd: row.get(3)?,
                            hostname: row.get(4)?,
                            exit_code: row.get(5)?,
                            duration_ms: row.get(6)?,
                            created_at: row.get(7)?,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect()
                };
                Ok(rows)
            })
            .await
            .map_err(|e| anyhow::anyhow!("Search error: {:?}", e))
    }

    // --- Session Log ---

    pub async fn append_session_log(&self, session_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let session_id = session_id.to_string();
        let data = data.to_vec();
        let now = chrono::Utc::now().to_rfc3339();

        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute(
                    "INSERT INTO session_log (id, session_id, data, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![id, session_id, data, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("Log error: {:?}", e))?;
        Ok(())
    }

    // --- Configured Hosts (feature #7: SSH auto-configure to left side) ---
    //
    // Records the host of an SSH session whose terminal has been successfully
    // auto-positioned to the leftmost tab. Only the FINAL success is recorded —
    // intermediate debug steps are not. On subsequent SSH logins to a host that
    // is already in this table, the configuration step is skipped (idempotency:
    // avoid duplicate configuration).

    /// Returns true if the host has already been auto-configured (i.e., its
    /// terminal was successfully moved to the left side on a prior SSH login).
    /// Used to short-circuit the configuration step on repeat connections.
    pub async fn is_host_configured(&self, host: &str) -> anyhow::Result<bool> {
        let host = host.to_string();
        self.conn
            .lock()
            .await
            .call::<_, bool, rusqlite::Error>(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT 1 FROM configured_hosts WHERE host = ?1 LIMIT 1")?;
                let exists = stmt.exists(params![host])?;
                Ok(exists)
            })
            .await
            .map_err(|e| anyhow::anyhow!("is_host_configured error: {:?}", e))
    }

    /// Records that an SSH session to `host` has been successfully
    /// auto-configured (its terminal moved to the leftmost tab position).
    /// Inserted with `INSERT OR IGNORE` so repeat calls for the same host are
    /// a no-op — only the FIRST successful configuration is recorded.
    pub async fn mark_host_configured(&self, host: &str) -> anyhow::Result<()> {
        let host = host.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .lock()
            .await
            .call::<_, (), rusqlite::Error>(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO configured_hosts (host, configured_at) VALUES (?1, ?2)",
                    params![host, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| anyhow::anyhow!("mark_host_configured error: {:?}", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::open(Some(path)).await.unwrap();
        (db, dir)
    }

    #[tokio::test]
    async fn test_save_and_list_connections() {
        let (db, _dir) = test_db().await;

        db.save_connection("c1", "Server 1", "Ssh", "{}", None, "", false)
            .await
            .unwrap();
        db.save_connection("c2", "Server 2", "Telnet", "{}", Some("Group A"), "", true)
            .await
            .unwrap();

        let connections = db.list_connections().await.unwrap();
        assert_eq!(connections.len(), 2);

        // Sorted by name
        assert_eq!(connections[0].1, "Server 1");
        assert_eq!(connections[1].1, "Server 2");
    }

    #[tokio::test]
    async fn test_delete_connection() {
        let (db, _dir) = test_db().await;

        db.save_connection("c1", "Server 1", "Ssh", "{}", None, "", false)
            .await
            .unwrap();
        db.delete_connection("c1").await.unwrap();

        let connections = db.list_connections().await.unwrap();
        assert!(connections.is_empty());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_connection() {
        let (db, _dir) = test_db().await;
        // Should not error
        db.delete_connection("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_save_and_search_history() {
        let (db, _dir) = test_db().await;

        let entries = vec![
            HistoryEntry {
                id: "h1".to_string(),
                command: "kubectl get pods".to_string(),
                session_id: "s1".to_string(),
                cwd: Some("/home/user".to_string()),
                hostname: Some("server1".to_string()),
                exit_code: Some(0),
                duration_ms: Some(1500),
                created_at: "2025-01-01T00:00:00Z".to_string(),
            },
            HistoryEntry {
                id: "h2".to_string(),
                command: "docker ps -a".to_string(),
                session_id: "s1".to_string(),
                cwd: Some("/home/user".to_string()),
                hostname: Some("server1".to_string()),
                exit_code: Some(0),
                duration_ms: Some(500),
                created_at: "2025-01-01T00:01:00Z".to_string(),
            },
            HistoryEntry {
                id: "h3".to_string(),
                command: "git status".to_string(),
                session_id: "s2".to_string(),
                cwd: Some("/project".to_string()),
                hostname: Some("localhost".to_string()),
                exit_code: Some(0),
                duration_ms: Some(100),
                created_at: "2025-01-01T00:02:00Z".to_string(),
            },
        ];

        for entry in entries {
            db.save_history(entry).await.unwrap();
        }

        // Search for kubectl
        let results = db.search_history("kubectl", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].command, "kubectl get pods");

        // List all (empty query)
        let all = db.search_history("", 10).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn test_search_history_filters_out_only_failed_commands() {
        // The user's requirement: failed commands (typos, broken commands) should
        // NOT appear in the suggestion popup — only commands that have succeeded
        // at least once should be suggested.
        //
        // This test verifies the HAVING clause in search_history drops commands
        // whose every recorded execution failed (exit_code != 0), while keeping:
        // - commands that succeeded at least once (even if they also have failed runs)
        // - commands with NULL exit_code (local shell import — unknown, assume success)
        let (db, _dir) = test_db().await;

        // "ls -la" — succeeded every time. SHOULD be suggested.
        db.save_history(HistoryEntry {
            id: "ok1".to_string(),
            command: "ls -la".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // "git statsu" — a typo of "git status". Failed every time (exit_code 1).
        // SHOULD NOT be suggested.
        db.save_history(HistoryEntry {
            id: "fail1".to_string(),
            command: "git statsu".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(1),
            duration_ms: None,
            created_at: "2025-01-01T00:01:00Z".to_string(),
        })
        .await
        .unwrap();
        db.save_history(HistoryEntry {
            id: "fail2".to_string(),
            command: "git statsu".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(127),
            duration_ms: None,
            created_at: "2025-01-02T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // "git status" — succeeded once, failed once. SHOULD be suggested
        // (at least one success is enough).
        db.save_history(HistoryEntry {
            id: "mixed1".to_string(),
            command: "git status".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        db.save_history(HistoryEntry {
            id: "mixed2".to_string(),
            command: "git status".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(128),
            duration_ms: None,
            created_at: "2025-01-02T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // "cat /some/missing" — local shell import with NULL exit_code
        // (no shell integration, exit code unknown). SHOULD be suggested
        // (we can't know it failed, so assume success).
        db.save_history(HistoryEntry {
            id: "null1".to_string(),
            command: "cat /some/missing".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: None,
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // Search for all commands (empty query)
        let all = db.search_history("", 10).await.unwrap();
        let cmds: Vec<String> = all.iter().map(|e| e.command.clone()).collect();

        // "git statsu" (typo) must be filtered out — every run failed.
        assert!(
            !cmds.contains(&"git statsu".to_string()),
            "only-failed commands must not be suggested: got {:?}",
            cmds
        );

        // The other three should be present.
        assert!(
            cmds.contains(&"ls -la".to_string()),
            "always-succeeded command should be suggested"
        );
        assert!(
            cmds.contains(&"git status".to_string()),
            "command with at least one success should be suggested even if it also has failures"
        );
        assert!(
            cmds.contains(&"cat /some/missing".to_string()),
            "command with NULL exit_code (unknown) should be suggested"
        );

        // Verify the count: 3 commands kept, 1 filtered out.
        assert_eq!(
            all.len(),
            3,
            "expected 3 suggested commands, got {}: {:?}",
            all.len(),
            cmds
        );
    }

    #[tokio::test]
    async fn test_delete_history_by_command_purges_all_rows() {
        // Verifies that when a command fails at runtime, we can purge
        // every DB row with that command string — including rows that
        // were previously saved as successful (exit_code=0) or imported
        // from shell history (exit_code=NULL). This is the core of the
        // "failed commands must never appear in suggestions" fix: even
        // if a command was previously recorded (in any state), running
        // it and seeing rc != 0 wipes it from history so it stops being
        // suggested. If the user later runs it successfully, deferred
        // recording will save it again.
        let (db, _dir) = test_db().await;

        // "kubectl get pods" — previously succeeded. SHOULD be suggested
        // until the user runs it and it fails.
        db.save_history(HistoryEntry {
            id: "ok1".to_string(),
            command: "kubectl get pods".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some("prod".to_string()),
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        // Same command, different host, NULL exit_code (shell import).
        db.save_history(HistoryEntry {
            id: "imp1".to_string(),
            command: "kubectl get pods".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some("dev".to_string()),
            exit_code: None,
            duration_ms: None,
            created_at: "2025-01-02T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // "ls" — unrelated, must survive the delete.
        db.save_history(HistoryEntry {
            id: "ok2".to_string(),
            command: "ls".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // Sanity: both commands are suggested before the delete.
        let before = db.search_history("", 10).await.unwrap();
        let cmds_before: Vec<String> = before.iter().map(|e| e.command.clone()).collect();
        assert!(cmds_before.contains(&"kubectl get pods".to_string()));
        assert!(cmds_before.contains(&"ls".to_string()));

        // The user now runs "kubectl get pods" and it fails (rc != 0).
        // The app calls delete_history_by_command to purge it.
        db.delete_history_by_command("kubectl get pods")
            .await
            .unwrap();

        // After the delete: "kubectl get pods" must NOT be suggested,
        // and BOTH rows (ok1 and imp1) must be gone — even across hosts
        // and regardless of exit_code.
        let after = db.search_history("", 10).await.unwrap();
        let cmds_after: Vec<String> = after.iter().map(|e| e.command.clone()).collect();
        assert!(
            !cmds_after.contains(&"kubectl get pods".to_string()),
            "failed command must be purged from suggestions: got {:?}",
            cmds_after
        );
        assert!(
            cmds_after.contains(&"ls".to_string()),
            "unrelated successful command must survive: got {:?}",
            cmds_after
        );
        assert_eq!(
            after.len(),
            1,
            "expected only 'ls' to remain, got {:?}",
            cmds_after
        );
    }

    #[tokio::test]
    async fn test_session_log() {
        let (db, _dir) = test_db().await;

        db.append_session_log("session-1", b"output data here")
            .await
            .unwrap();
        db.append_session_log("session-1", b"more output")
            .await
            .unwrap();

        // Just verify it doesn't error - reading logs back would need a separate query
    }

    #[tokio::test]
    async fn test_mark_command_failed_replaces_prior_rows() {
        // The user's repro: type `pwdwd` (a typo), it fails with rc != 0.
        // Previously, the failed command would still appear in the suggestion
        // popup because:
        //   1. The remote `~/.bash_history` contains `pwdwd` from a prior
        //      session, and history import re-inserts it as `exit_code = NULL`,
        //      which the HAVING clause keeps ("unknown, assume success").
        //   2. The runtime `rc != 0` branch deleted the DB row, but the next
        //      reconnect re-imported it.
        //
        // The fix: on rc != 0, call `mark_command_failed`, which DELETEs any
        // existing rows for that command and INSERTs a single row with
        // `exit_code = <rc>`. The HAVING clause then filters it out (no
        // successful run, and at least one non-NULL exit_code).
        let (db, _dir) = test_db().await;

        // Simulate a prior NULL import (from ~/.bash_history).
        db.save_history(HistoryEntry {
            id: "imp1".to_string(),
            command: "pwdwd".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some("dev-vm01".to_string()),
            exit_code: None,
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        // And a prior successful run (defensive — shouldn't happen for a
        // typo, but tests the replace semantics).
        db.save_history(HistoryEntry {
            id: "ok1".to_string(),
            command: "pwdwd".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some("dev-vm01".to_string()),
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-02T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        // Sanity: pwdwd IS suggested before the failure marker.
        let before = db.search_history("pwd", 10).await.unwrap();
        assert!(
            before.iter().any(|e| e.command == "pwdwd"),
            "pwdwd should be suggested before the failure marker (prior success): {:?}",
            before.iter().map(|e| e.command.clone()).collect::<Vec<_>>()
        );

        // The user runs pwdwd, it fails with rc=127.
        db.mark_command_failed("pwdwd", 127).await.unwrap();

        // After marking: pwdwd must NOT be suggested — the HAVING clause
        // drops it (no successful run recorded, and a non-NULL exit_code
        // exists).
        let after = db.search_history("pwd", 10).await.unwrap();
        assert!(
            !after.iter().any(|e| e.command == "pwdwd"),
            "pwdwd must not be suggested after mark_command_failed: {:?}",
            after.iter().map(|e| e.command.clone()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_known_failed_commands_excludes_successful_and_null() {
        // `known_failed_commands` is the set used by history imports to skip
        // commands that would otherwise be re-introduced from
        // `~/.bash_history` as `exit_code = NULL`. It must include ONLY
        // commands whose every recorded execution failed (no NULL rows,
        // no zero rows).
        let (db, _dir) = test_db().await;

        // "pwdwd" — only-failed (should be in the set).
        db.mark_command_failed("pwdwd", 127).await.unwrap();
        // "ls" — only-success (should NOT be in the set).
        db.save_history(HistoryEntry {
            id: "ok1".to_string(),
            command: "ls".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        // "docker ps" — NULL import (should NOT be in the set — unknown).
        db.save_history(HistoryEntry {
            id: "imp1".to_string(),
            command: "docker ps".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some("dev".to_string()),
            exit_code: None,
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        // "git status" — mixed (one failure, one success → NOT in the set).
        db.save_history(HistoryEntry {
            id: "mix-ok".to_string(),
            command: "git status".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
        db.save_history(HistoryEntry {
            id: "mix-fail".to_string(),
            command: "git status".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(1),
            duration_ms: None,
            created_at: "2025-01-02T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        let failed = db.known_failed_commands().await.unwrap();
        assert!(
            failed.contains("pwdwd"),
            "only-failed command should be in the known-failed set: {:?}",
            failed
        );
        assert!(
            !failed.contains("ls"),
            "successful command should NOT be in the known-failed set: {:?}",
            failed
        );
        assert!(
            !failed.contains("docker ps"),
            "NULL-import command should NOT be in the known-failed set: {:?}",
            failed
        );
        assert!(
            !failed.contains("git status"),
            "mixed command (has at least one success) should NOT be in the known-failed set: {:?}",
            failed
        );
    }

    #[tokio::test]
    async fn test_failed_command_survives_reimport_scenario() {
        // This is the user's exact repro scenario, modeled at the DB level:
        //
        //   1. User previously typed `pwdwd` (a typo) on a remote host. It
        //      failed, but bash wrote it to `~/.bash_history`.
        //   2. User opens a new RusTerm session. History import reads
        //      `~/.bash_history` and inserts `pwdwd` with `exit_code = NULL`.
        //   3. `pwdwd` appears in the suggestion popup (HAVING keeps NULL).
        //   4. User runs `pwdwd`, it fails with rc=127. The app calls
        //      `mark_command_failed("pwdwd", 127)`.
        //   5. `pwdwd` should NO LONGER appear in suggestions.
        //   6. User disconnects and reconnects. The history import runs
        //      again, reads `~/.bash_history` (still has `pwdwd`), and
        //      — CRITICALLY — the import skips `pwdwd` because it's in
        //      `known_failed_commands`. `pwdwd` must NOT reappear.
        //
        // This test pins the entire flow so a future regression in either
        // `mark_command_failed` or `known_failed_commands` (or the import
        // filter that uses them) is caught.
        let (db, _dir) = test_db().await;

        // Step 1+2: simulate history import by saving `pwdwd` (and some
        // legitimate commands) with exit_code = NULL.
        db.save_history_batch(vec![
            HistoryEntry {
                id: "imp-pwdwd".to_string(),
                command: "pwdwd".to_string(),
                session_id: "s".to_string(),
                cwd: None,
                hostname: Some("dev-vm01".to_string()),
                exit_code: None,
                duration_ms: None,
                created_at: "2025-01-01T00:00:00Z".to_string(),
            },
            HistoryEntry {
                id: "imp-pwd".to_string(),
                command: "pwd".to_string(),
                session_id: "s".to_string(),
                cwd: None,
                hostname: Some("dev-vm01".to_string()),
                exit_code: None,
                duration_ms: None,
                created_at: "2025-01-01T00:01:00Z".to_string(),
            },
            HistoryEntry {
                id: "imp-ls".to_string(),
                command: "ls -la".to_string(),
                session_id: "s".to_string(),
                cwd: None,
                hostname: Some("dev-vm01".to_string()),
                exit_code: None,
                duration_ms: None,
                created_at: "2025-01-01T00:02:00Z".to_string(),
            },
        ])
        .await
        .unwrap();

        // Step 3: pwdwd IS suggested before the failure marker.
        let before = db.search_history("pw", 10).await.unwrap();
        let cmds_before: Vec<String> = before.iter().map(|e| e.command.clone()).collect();
        assert!(
            cmds_before.contains(&"pwdwd".to_string()),
            "pwdwd should be suggested before the failure marker (NULL import): {:?}",
            cmds_before
        );
        assert!(
            cmds_before.contains(&"pwd".to_string()),
            "pwd should be suggested: {:?}",
            cmds_before
        );

        // Step 4: the user runs pwdwd, it fails. App calls mark_command_failed.
        db.mark_command_failed("pwdwd", 127).await.unwrap();

        // Step 5: pwdwd must NOT be suggested after the failure marker.
        let after_mark = db.search_history("pw", 10).await.unwrap();
        let cmds_after_mark: Vec<String> = after_mark.iter().map(|e| e.command.clone()).collect();
        assert!(
            !cmds_after_mark.contains(&"pwdwd".to_string()),
            "pwdwd must NOT be suggested after mark_command_failed: {:?}",
            cmds_after_mark
        );
        assert!(
            cmds_after_mark.contains(&"pwd".to_string()),
            "pwd should still be suggested: {:?}",
            cmds_after_mark
        );

        // Step 6: simulate a reconnect. The import first fetches
        // known_failed_commands, then filters the would-be-imported list
        // against that set. Here we model that: `~/.bash_history` still
        // contains pwdwd, pwd, and ls, but the import should skip pwdwd.
        let failed_set = db.known_failed_commands().await.unwrap();
        assert!(
            failed_set.contains("pwdwd"),
            "known_failed_commands must report pwdwd so the import can skip it: {:?}",
            failed_set
        );

        let reimport_commands = vec!["pwdwd".to_string(), "pwd".to_string(), "ls -la".to_string()];
        let filtered: Vec<String> = reimport_commands
            .into_iter()
            .filter(|cmd| !failed_set.contains(cmd))
            .collect();

        // Now the import deletes the host's prior rows and inserts the
        // filtered list.
        db.delete_history_by_hostname("dev-vm01").await.unwrap();
        let entries: Vec<_> = filtered
            .iter()
            .map(|cmd| HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                command: cmd.clone(),
                session_id: "s".to_string(),
                cwd: None,
                hostname: Some("dev-vm01".to_string()),
                exit_code: None,
                duration_ms: None,
                created_at: chrono::Utc::now().to_rfc3339(),
            })
            .collect();
        db.save_history_batch(entries).await.unwrap();

        // After reconnect: pwdwd must STILL not be suggested.
        let after_reconnect = db.search_history("pw", 10).await.unwrap();
        let cmds_after_reconnect: Vec<String> =
            after_reconnect.iter().map(|e| e.command.clone()).collect();
        assert!(
            !cmds_after_reconnect.contains(&"pwdwd".to_string()),
            "pwdwd must NOT reappear after reconnect (import must skip known-failed): {:?}",
            cmds_after_reconnect
        );
        assert!(
            cmds_after_reconnect.contains(&"pwd".to_string()),
            "pwd should still be suggested after reconnect: {:?}",
            cmds_after_reconnect
        );
    }

    #[tokio::test]
    async fn test_mark_failed_then_success_re_enables_suggestion() {
        // If the user runs a previously-failed command successfully, the
        // command should re-appear in suggestions. `save_history` with
        // `exit_code = Some(0)` adds a successful row; `mark_command_failed`
        // had previously replaced all rows with a single failure row, so
        // after the successful save there's one failure + one success →
        // HAVING keeps it.
        let (db, _dir) = test_db().await;

        db.mark_command_failed("kubectl get pods", 1).await.unwrap();
        let after_fail = db.search_history("kube", 10).await.unwrap();
        assert!(!after_fail.iter().any(|e| e.command == "kubectl get pods"));

        db.save_history(HistoryEntry {
            id: "ok-after".to_string(),
            command: "kubectl get pods".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: None,
            exit_code: Some(0),
            duration_ms: None,
            created_at: "2025-01-03T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

        let after_ok = db.search_history("kube", 10).await.unwrap();
        assert!(
            after_ok.iter().any(|e| e.command == "kubectl get pods"),
            "command should be re-suggested after a successful run: {:?}",
            after_ok
                .iter()
                .map(|e| e.command.clone())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn test_upsert_connection() {
        let (db, _dir) = test_db().await;

        db.save_connection("c1", "Original", "Ssh", "{}", None, "", false)
            .await
            .unwrap();
        db.save_connection("c1", "Updated", "Ssh", "{}", None, "", false)
            .await
            .unwrap();

        let connections = db.list_connections().await.unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].1, "Updated");
    }

    // --- configured_hosts tests (feature #7: SSH auto-configure to left) ---

    #[tokio::test]
    async fn test_is_host_configured_returns_false_for_unknown_host() {
        // A freshly-opened DB has no configured hosts, so any host lookup
        // must return false. This is the short-circuit path the SSH
        // connect flow uses to decide whether to perform the auto-configure
        // step (move tab to leftmost + record success).
        let (db, _dir) = test_db().await;
        assert!(!db.is_host_configured("new-host.example").await.unwrap());
    }

    #[tokio::test]
    async fn test_mark_then_is_host_configured_roundtrip() {
        // After `mark_host_configured` succeeds for a host, a subsequent
        // `is_host_configured` lookup for that host must return true. This
        // is the contract the SSH connect flow relies on: once a host has
        // been successfully auto-configured, future SSH logins to the same
        // host skip the configuration step (idempotency).
        let (db, _dir) = test_db().await;

        assert!(!db.is_host_configured("prod-1").await.unwrap());
        db.mark_host_configured("prod-1").await.unwrap();
        assert!(db.is_host_configured("prod-1").await.unwrap());
    }

    #[tokio::test]
    async fn test_mark_host_configured_is_idempotent() {
        // Calling `mark_host_configured` twice for the same host must not
        // error and must not create a duplicate row. The SSH connect flow
        // can call this on every successful connect (the check-then-mark
        // sequence isn't atomic), and the second call is a no-op — this is
        // part of the "avoid duplicate configuration" requirement.
        let (db, _dir) = test_db().await;

        db.mark_host_configured("bastion").await.unwrap();
        db.mark_host_configured("bastion").await.unwrap();
        // Still only counts as configured once — `is_host_configured`
        // returns true (not the number of rows).
        assert!(db.is_host_configured("bastion").await.unwrap());

        // A different host is still unknown.
        assert!(!db.is_host_configured("other-host").await.unwrap());
    }

    #[tokio::test]
    async fn test_skip_configure_when_already_configured() {
        // This is the test the task summary called for: "skip if already
        // configured". The DB layer exposes the two primitives (check +
        // mark); the SSH connect flow composes them as:
        //   if !is_host_configured(h) { configure(); mark_host_configured(h); }
        // We simulate that flow here and assert that after the first
        // iteration, a second iteration does NOT need to call
        // `mark_host_configured` again (the host is already recorded).
        let (db, _dir) = test_db().await;
        let host = "already-known-host";

        // First connect: not configured → configure → mark.
        let needs_configure = !db.is_host_configured(host).await.unwrap();
        assert!(
            needs_configure,
            "first connect should require configuration"
        );
        if needs_configure {
            // (In the real flow, the tab is moved to leftmost here.)
            db.mark_host_configured(host).await.unwrap();
        }

        // Second connect: already configured → skip the configure step.
        let needs_configure_again = !db.is_host_configured(host).await.unwrap();
        assert!(
            !needs_configure_again,
            "second connect should NOT require configuration (already recorded)"
        );
        // Crucially, we do NOT call mark_host_configured again — the host
        // is already in the table, and the second configure step is
        // skipped ("avoid duplicate configuration").
    }
}
