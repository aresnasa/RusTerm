use chrono::{Duration, Utc};
use rusterm_db::{
    Database,
    history::{HistoryCursor, HistoryEntry},
};
use tempfile::TempDir;

async fn test_db() -> (Database, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(Some(dir.path().join("history.db")))
        .await
        .unwrap();
    (db, dir)
}

fn entry(id: &str, command: &str, session_id: &str, created_at: &str) -> HistoryEntry {
    HistoryEntry {
        id: id.to_owned(),
        command: command.to_owned(),
        session_id: session_id.to_owned(),
        cwd: None,
        hostname: None,
        exit_code: Some(0),
        duration_ms: None,
        created_at: created_at.to_owned(),
    }
}

#[tokio::test]
async fn paginates_stably_without_duplicates() {
    let (db, _dir) = test_db().await;
    db.save_history_batch(vec![
        entry("oldest", "cmd oldest", "s1", "2025-01-01T00:00:00Z"),
        entry("middle", "cmd middle", "s1", "2025-01-01T00:01:00Z"),
        entry("same-a", "cmd same a", "s1", "2025-01-01T00:02:00Z"),
        entry("same-c", "cmd same c", "s1", "2025-01-01T00:02:00Z"),
        entry("same-b", "cmd same b", "s1", "2025-01-01T00:02:00Z"),
        entry("newest", "cmd newest", "s1", "2025-01-01T00:03:00Z"),
    ])
    .await
    .unwrap();

    let expected = vec!["newest", "same-c", "same-b", "same-a", "middle", "oldest"];
    let mut actual = Vec::new();
    let mut before = None;

    loop {
        let page = db
            .list_history_page(None, None, before.as_ref(), 2)
            .await
            .unwrap();
        actual.extend(page.entries.iter().map(|item| item.id.as_str().to_owned()));

        match page.next_cursor {
            Some(cursor) => before = Some(cursor),
            None => break,
        }
    }

    assert_eq!(actual, expected);
    let mut deduplicated = actual.clone();
    deduplicated.sort();
    deduplicated.dedup();
    assert_eq!(deduplicated.len(), actual.len());

    let empty = db
        .list_history_page(
            None,
            None,
            Some(&HistoryCursor {
                created_at: "2025-01-01T00:00:00Z".to_owned(),
                id: "oldest".to_owned(),
            }),
            2,
        )
        .await
        .unwrap();
    assert!(empty.entries.is_empty());
    assert!(empty.next_cursor.is_none());
}

#[tokio::test]
async fn filters_by_case_insensitive_contains_and_session() {
    let (db, _dir) = test_db().await;
    db.save_history_batch(vec![
        entry("h1", "Git Status", "s1", "2025-01-01T00:04:00Z"),
        entry("h2", "kubectl DEPLOY api", "s1", "2025-01-01T00:03:00Z"),
        entry("h3", "deploy worker", "s2", "2025-01-01T00:02:00Z"),
        entry("h4", "echo hello", "s2", "2025-01-01T00:01:00Z"),
    ])
    .await
    .unwrap();

    let search = db
        .list_history_page(Some("pLoY"), None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        search
            .entries
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h2", "h3"]
    );
    assert!(search.next_cursor.is_none());

    let session_search = db
        .list_history_page(Some("deploy"), Some("s2"), None, 10)
        .await
        .unwrap();
    assert_eq!(session_search.entries.len(), 1);
    assert_eq!(session_search.entries[0].id, "h3");

    let session = db
        .list_history_page(None, Some("s1"), None, 10)
        .await
        .unwrap();
    assert_eq!(
        session
            .entries
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["h1", "h2"]
    );

    let empty = db
        .list_history_page(Some("does not exist"), None, None, 10)
        .await
        .unwrap();
    assert!(empty.entries.is_empty());
    assert!(empty.next_cursor.is_none());
}

#[tokio::test]
async fn search_history_favors_recent_commands() {
    let (db, _dir) = test_db().await;
    let now = Utc::now();
    let old = (now - Duration::days(60)).to_rfc3339();
    let recent = (now - Duration::minutes(5)).to_rfc3339();

    db.save_history_batch(vec![
        entry("old-1", "older frequent command", "s1", &old),
        entry("old-2", "older frequent command", "s2", &old),
        entry("recent", "recent command", "s1", &recent),
    ])
    .await
    .unwrap();

    let results = db.search_history("", 10).await.unwrap();
    assert_eq!(results[0].command, "recent command");
}
