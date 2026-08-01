use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: String,
    pub command: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub created_at: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_cursor: Option<HistoryCursor>,
}

pub struct CommandHistory;

impl CommandHistory {
    pub fn new() -> Self {
        Self
    }
}
