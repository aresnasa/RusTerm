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
                let mut stmt = conn.prepare(
                    "SELECT id, name, kind, config FROM connections ORDER BY name",
                )?;
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

    /// Search command history with frecency ranking (frequency + recency + success).
    /// Groups by command, counts executions, and ranks by a combined
    /// score of frequency, recency, and success rate inspired by atuin.
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

        self.conn
            .lock()
            .await
            .call::<_, Vec<HistoryEntry>, rusqlite::Error>(move |conn| {
                let sql = if query.is_empty() {
                    format!(
                        "SELECT h.id, h.command, h.session_id, h.cwd, h.hostname, h.exit_code, h.duration_ms, h.created_at
                         FROM history h
                         GROUP BY h.command
                         HAVING MAX(h.created_at)
                         ORDER BY {order_expr}
                         LIMIT ?2"
                    )
                } else {
                    format!(
                        "SELECT h.id, h.command, h.session_id, h.cwd, h.hostname, h.exit_code, h.duration_ms, h.created_at
                         FROM history h
                         JOIN history_fts f ON h.rowid = f.rowid
                         WHERE history_fts MATCH ?1
                         GROUP BY h.command
                         HAVING MAX(h.created_at)
                         ORDER BY {order_expr}
                         LIMIT ?2"
                    )
                };

                let mut stmt = conn.prepare(&sql)?;
                let rows: Vec<HistoryEntry> = if query.is_empty() {
                    stmt.query_map(params![now_secs, limit], |row| {
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

    pub async fn append_session_log(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> anyhow::Result<()> {
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

        db.save_connection("c1", "Server 1", "Ssh", "{}", None, "", false).await.unwrap();
        db.save_connection("c2", "Server 2", "Telnet", "{}", Some("Group A"), "", true).await.unwrap();

        let connections = db.list_connections().await.unwrap();
        assert_eq!(connections.len(), 2);

        // Sorted by name
        assert_eq!(connections[0].1, "Server 1");
        assert_eq!(connections[1].1, "Server 2");
    }

    #[tokio::test]
    async fn test_delete_connection() {
        let (db, _dir) = test_db().await;

        db.save_connection("c1", "Server 1", "Ssh", "{}", None, "", false).await.unwrap();
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
    async fn test_session_log() {
        let (db, _dir) = test_db().await;

        db.append_session_log("session-1", b"output data here").await.unwrap();
        db.append_session_log("session-1", b"more output").await.unwrap();

        // Just verify it doesn't error - reading logs back would need a separate query
    }

    #[tokio::test]
    async fn test_upsert_connection() {
        let (db, _dir) = test_db().await;

        db.save_connection("c1", "Original", "Ssh", "{}", None, "", false).await.unwrap();
        db.save_connection("c1", "Updated", "Ssh", "{}", None, "", false).await.unwrap();

        let connections = db.list_connections().await.unwrap();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].1, "Updated");
    }
}
