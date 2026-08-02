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

/// One command executed through the REST relay (`rusterm-relay`). Unlike
/// [`HistoryEntry`] (local terminal shell history), a relay entry always has
/// an API `account`, a target `host_id`, and records whether the command ran
/// elevated. `exit_code`/`duration_ms`/`timed_out` are `None` only when the
/// command never reached the executor (rejected by validation) — when the
/// executor ran but returned an error, we still persist the row with whatever
/// partial outcome we have so the user can see and re-run it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHistoryEntry {
    pub id: String,
    pub account: String,
    pub host_id: String,
    pub command: String,
    pub elevated: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub timed_out: bool,
    pub created_at: String,
}

/// Cursor for paginating [`RelayHistoryEntry`] in reverse-chronological order.
/// Matches the `(created_at, id)` sort key so entries sharing a timestamp
/// stay stable across page boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHistoryCursor {
    pub created_at: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHistoryPage {
    pub entries: Vec<RelayHistoryEntry>,
    pub next_cursor: Option<RelayHistoryCursor>,
}
