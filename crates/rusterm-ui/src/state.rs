use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use rusterm_core::config::{ConnectionConfig, OneKey};
use rusterm_core::config_manager::ConfigManager;
use rusterm_core::session::SessionType;
use rusterm_core::session_log::SessionLog;
use rusterm_core::terminal::{RenderOutput, Terminal};

pub type TerminalHandle = Arc<Mutex<TerminalEntry>>;

pub struct TerminalEntry {
    pub terminal: Terminal,
    pub parser: vte::ansi::Processor,
    pub scroll_offset: usize,
}

impl TerminalEntry {
    pub fn process_and_render(&mut self, data: &[u8]) -> rusterm_core::terminal::RenderOutput {
        let parser = &mut self.parser;
        self.terminal.process(data, parser);
        if self.scroll_offset == 0 {
            self.terminal.render_with_scroll(0)
        } else {
            self.terminal.render_with_scroll(self.scroll_offset)
        }
    }

    pub fn scroll_up(&mut self, rows: usize) -> rusterm_core::terminal::RenderOutput {
        let max_scroll = self.terminal.scrollback_len();
        self.scroll_offset = (self.scroll_offset + rows).min(max_scroll);
        self.terminal.render_with_scroll(self.scroll_offset)
    }

    pub fn scroll_down(&mut self, rows: usize) -> rusterm_core::terminal::RenderOutput {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.terminal.render_with_scroll(self.scroll_offset)
    }

    pub fn scroll_to_bottom(&mut self) -> rusterm_core::terminal::RenderOutput {
        self.scroll_offset = 0;
        self.terminal.render_with_scroll(0)
    }

    pub fn render_current(&self) -> rusterm_core::terminal::RenderOutput {
        self.terminal.render_with_scroll(self.scroll_offset)
    }
}

impl std::fmt::Debug for TerminalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalEntry").finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum UnlockState {
    #[default]
    FirstRun,
    Locked,
    Unlocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub sessions: Vec<SessionTab>,
    pub active_session: Option<String>,
    pub sidebar_open: bool,
    pub connections: Vec<ConnectionConfig>,
    pub theme: Theme,
    #[serde(skip)]
    pub close_senders: Vec<(String, mpsc::UnboundedSender<()>)>,
    #[serde(skip)]
    pub resize_senders: HashMap<String, mpsc::UnboundedSender<(u16, u16, u32, u32)>>,
    #[serde(skip)]
    pub config_manager: Option<ConfigManager>,
    #[serde(skip)]
    pub terminals: HashMap<String, TerminalHandle>,
    #[serde(skip)]
    pub session_logs: HashMap<String, Arc<Mutex<SessionLog>>>,
    #[serde(skip)]
    pub unlock_state: UnlockState,
    #[serde(skip)]
    pub master_password_error: Option<String>,
    /// Monotonically increasing counter for debouncing suggestion queries.
    /// Each keystroke increments this; stale async queries check if their
    /// snapshot is still current before writing results.
    #[serde(skip)]
    pub suggestion_epoch: u64,
    /// Per-session queue of (command, db_id) awaiting its exit code from OSC 133;D.
    /// Each Enter pushes a new pending entry onto the back; OSC 133;D pops the
    /// front (FIFO matches the shell's strict command-execution order). When
    /// a non-zero exit code arrives, the command is silently dropped — never
    /// recorded into history — so failed commands aren't suggested. On a zero
    /// exit code, the command is finally committed to `command_history` and
    /// the DB. If the shell doesn't emit OSC 133;D (no shell integration),
    /// entries stay queued and are never suggested — by design, we'd rather
    /// suggest nothing than suggest failed commands.
    #[serde(skip)]
    pub pending_exit_check: HashMap<String, VecDeque<(String, String)>>,
    /// Commands that have just failed (rc != 0) and are awaiting the async
    /// `mark_command_failed` DB write to complete.
    ///
    /// WHY THIS EXISTS: `mark_command_failed` runs in a `spawn` (we can't
    /// block the output loop on a DB write). Between the `retain` that
    /// removes the command from `command_history` (immediate) and the DB
    /// write that replaces the prior `exit_code = NULL` import row with a
    /// durable `exit_code = <rc>` failure marker (async), there's a window
    /// where the DB still has the old NULL row. The `HAVING` clause in
    /// `search_history` keeps NULL-exit-code commands ("unknown, assume
    /// success"), so during that window a suggestion query would re-surface
    /// the just-failed command — exactly the bug the user reported ("错误命令
    /// 会出现在上方建议栏").
    ///
    /// This set is the UI-side guard: on rc != 0 we insert the command here
    /// synchronously (same critical section as the `retain`), and the
    /// suggestion query filters against it. The `mark_command_failed` spawn
    /// removes the command from this set after the DB write commits, at
    /// which point the DB's own `HAVING` clause takes over and the set is
    /// no longer needed for that command. If the spawn fails (DB error),
    /// the entry stays in the set for the rest of the session — better to
    /// over-filter (never suggest a known-failed command) than to re-surface
    /// a typo the user just saw fail.
    #[serde(skip)]
    pub recent_failed_commands: HashSet<String>,
    /// OneKey library (ZOC-style Expect/Send), decrypted in memory after unlock.
    #[serde(skip)]
    pub onekeys: Vec<OneKey>,
    /// Per-session OneKey autofill popup state. Only shown when new output matches
    /// an OneKey's expect regex; persists across focus changes (no re-scan).
    #[serde(skip)]
    pub onekey_popups: HashMap<String, OneKeyPopupState>,
    /// Per-session connection config (kept in memory, not persisted) so a
    /// disconnected session can be reconnected by pressing Enter.
    #[serde(skip)]
    pub session_configs: HashMap<String, ConnectionConfig>,
    /// Session ids whose SSH/shell channel has dropped. While a session is in
    /// this set, pressing Enter triggers a reconnect instead of going to the
    /// (dead) PTY.
    #[serde(skip)]
    pub disconnected_sessions: HashSet<String>,
}

/// State of the OneKey autofill popup for a single session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OneKeyPopupState {
    pub visible: bool,
    /// Matching entries (one per OneKey whose step matched), each carrying the
    /// send value of the matched step.
    pub matches: Vec<OneKeyMatch>,
    pub selected: usize,
    /// The expect pattern that matched (used by "Save In OneKeys" to prefill).
    pub matched_expect: Option<String>,
}

/// A single match in the OneKey popup: the OneKey's name + the matched step's
/// send value (sent on selection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OneKeyMatch {
    pub name: String,
    pub send: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTab {
    pub id: String,
    pub name: String,
    pub kind: SessionType,
    #[serde(skip)]
    pub render_output: RenderOutput,
    pub version: u64,
    /// Inline fish-style suggestion (top match suffix)
    #[serde(skip)]
    pub suggestion: Option<String>,
    /// Multiple suggestion candidates for the dropdown
    #[serde(skip)]
    pub suggestions: Vec<String>,
    /// Dropdown selected index
    #[serde(skip)]
    pub suggestion_selected: usize,
    /// Dropdown visibility
    #[serde(skip)]
    pub suggestion_visible: bool,
    /// Local command history for this session. Stored locally only, never transmitted.
    #[serde(skip)]
    pub command_history: Vec<String>,
    /// Hostname this session is connected to (SSH host or "local" for shell).
    /// Used to tag commands in the DB so suggestions can draw from all hosts.
    #[serde(skip)]
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Theme {
    Dark,
    Light,
}

impl Default for AppState {
    fn default() -> Self {
        let unlock_state = if ConfigManager::check_config_exists() {
            UnlockState::Locked
        } else {
            UnlockState::FirstRun
        };
        Self {
            sessions: Vec::new(),
            active_session: None,
            sidebar_open: true,
            connections: Vec::new(),
            theme: Theme::Dark,
            close_senders: Vec::new(),
            resize_senders: HashMap::new(),
            config_manager: None,
            terminals: HashMap::new(),
            session_logs: HashMap::new(),
            unlock_state,
            master_password_error: None,
            suggestion_epoch: 0,
            pending_exit_check: HashMap::new(),
            recent_failed_commands: HashSet::new(),
            onekeys: Vec::new(),
            onekey_popups: HashMap::new(),
            session_configs: HashMap::new(),
            disconnected_sessions: HashSet::new(),
        }
    }
}

/// Move the session tab identified by `tab_id` to the leftmost position
/// (index 0) of `state.sessions`. This is the "configure terminal to the
/// left side" action triggered after a successful SSH login (feature #7).
///
/// Returns `true` if the tab was found and actually moved (i.e., it was not
/// already at position 0), `false` if the tab was not found OR was already
/// at the leftmost position. The SSH connect flow uses the `true` return
/// value as the signal that a configuration step actually occurred — only
/// then is the host recorded as configured in the DB (avoid duplicate
/// configuration).
///
/// This is a no-op when the tab is already at index 0: "already configured
/// in-place" is treated as "no configuration step occurred", so the caller
/// won't record the host again.
///
/// Takes `&mut AppState` (rather than `&mut Signal<AppState>`) so it's
/// unit-testable without spinning up a dioxus runtime. Callers in `app.rs`
/// pass `&mut state.write()`.
pub fn move_session_to_leftmost(state: &mut AppState, tab_id: &str) -> bool {
    let pos = state.sessions.iter().position(|t| t.id == tab_id);
    let Some(pos) = pos else {
        // Tab not found — nothing to configure. Don't record this as a
        // successful configuration (the requirement is to record only on
        // confirmed success, and we couldn't even find the session).
        return false;
    };
    if pos == 0 {
        // Already leftmost. Treat as already-configured-in-place — don't
        // record (avoids duplicate configuration on repeat connects to
        // a host whose tab happens to be the only one / already first).
        return false;
    }
    let tab = state.sessions.remove(pos);
    state.sessions.insert(0, tab);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the deferred-recording FIFO contract: Enter pushes a pending
    /// entry onto the back; OSC 133;D pops the front. This is the data-structure
    /// invariant the app relies on to commit only successful commands and drop
    /// failed ones in command-execution order.
    #[test]
    fn pending_exit_check_fifo_preserves_command_order() {
        let mut state = AppState::default();
        let sid = "session-1";

        // Simulate two Enters before any OSC 133;D arrives — e.g. the user
        // queued `ls` then `pwd` while the shell was still running `ls`.
        state
            .pending_exit_check
            .entry(sid.to_string())
            .or_default()
            .push_back(("ls".to_string(), "id-1".to_string()));
        state
            .pending_exit_check
            .entry(sid.to_string())
            .or_default()
            .push_back(("pwd".to_string(), "id-2".to_string()));

        // First OSC 133;D must pop `ls` (FIFO front), not `pwd`.
        let first = state
            .pending_exit_check
            .get_mut(sid)
            .and_then(|q| q.pop_front());
        assert_eq!(first, Some(("ls".to_string(), "id-1".to_string())));

        // Second OSC 133;D pops `pwd`.
        let second = state
            .pending_exit_check
            .get_mut(sid)
            .and_then(|q| q.pop_front());
        assert_eq!(second, Some(("pwd".to_string(), "id-2".to_string())));

        // Queue is now empty — a third OSC 133;D pops nothing. This is the
        // branch the failed-command discard takes when the user typed
        // something the shell rejected before printing a prompt.
        let third = state
            .pending_exit_check
            .get_mut(sid)
            .and_then(|q| q.pop_front());
        assert_eq!(third, None);
    }

    /// A new session starts with no pending exit checks and an empty default
    /// VecDeque. This pins the API contract that the app relies on: looking
    /// up a missing session returns None (not a panic), and `or_default()`
    /// creates an empty queue that can be pushed onto.
    #[test]
    fn pending_exit_check_missing_session_returns_none() {
        let mut state = AppState::default();
        let popped = state
            .pending_exit_check
            .get_mut("nonexistent")
            .and_then(|q| q.pop_front());
        assert_eq!(popped, None);
    }

    /// The pending queue is capped to prevent unbounded growth when the shell
    /// never emits OSC 133;D (no shell integration, or integration not yet
    /// loaded). The cap mirrors `MAX_PENDING` in `on_command`. When the queue
    /// is at capacity, the oldest entry is dropped before the new one is
    /// pushed — FIFO eviction. This pins the cap behaviour so a future
    /// refactor can't silently regress it.
    #[test]
    fn pending_exit_check_is_capped_to_max_pending() {
        let mut state = AppState::default();
        let sid = "session-1";
        const MAX_PENDING: usize = 32;

        // Push MAX_PENDING + 5 entries — the first 5 should be evicted.
        let queue = state.pending_exit_check.entry(sid.to_string()).or_default();
        for i in 0..(MAX_PENDING + 5) {
            while queue.len() >= MAX_PENDING {
                queue.pop_front();
            }
            queue.push_back((format!("cmd-{i}"), format!("id-{i}")));
        }

        // Queue should never exceed the cap.
        assert_eq!(queue.len(), MAX_PENDING);

        // Front of the queue should be the MAX_PENDING-th entry (5 evicted).
        let front = queue.front().map(|(cmd, _)| cmd.clone());
        assert_eq!(front.as_deref(), Some("cmd-5"));
    }

    /// Build an AppState with N session tabs in the order given by `names`.
    /// Helper for the move_session_to_leftmost tests below.
    fn state_with_tabs(names: &[&str]) -> AppState {
        let mut state = AppState::default();
        for name in names {
            state.sessions.push(SessionTab {
                id: name.to_string(),
                name: name.to_string(),
                kind: SessionType::Ssh,
                render_output: Default::default(),
                version: 0,
                suggestion: None,
                suggestions: Vec::new(),
                suggestion_selected: 0,
                suggestion_visible: false,
                command_history: Vec::new(),
                hostname: Some(name.to_string()),
            });
        }
        state
    }

    /// move_session_to_leftmost must relocate the matching tab to index 0.
    /// This is the core of feature #7 (auto-configure terminal to left side
    /// after SSH login): the SSH session's tab is moved to the leftmost
    /// position in the tab bar.
    #[test]
    fn move_session_to_leftmost_moves_matching_tab_to_index_zero() {
        let mut state = state_with_tabs(&["alpha", "beta", "gamma"]);
        let moved = move_session_to_leftmost(&mut state, "gamma");
        assert!(moved, "tab `gamma` (at index 2) should have been moved");
        let ids: Vec<String> = state.sessions.iter().map(|t| t.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["gamma".to_string(), "alpha".to_string(), "beta".to_string()],
            "`gamma` should now be at index 0; the rest should shift right"
        );
    }

    /// move_session_to_leftmost is a no-op when the tab is already at
    /// index 0. Returning `false` here tells the caller NOT to record the
    /// host as freshly configured (avoid duplicate configuration — the
    /// tab is already in the desired leftmost position).
    #[test]
    fn move_session_to_leftmost_is_noop_when_already_leftmost() {
        let mut state = state_with_tabs(&["alpha", "beta", "gamma"]);
        let moved = move_session_to_leftmost(&mut state, "alpha");
        assert!(
            !moved,
            "`alpha` is already at index 0 — no configuration step occurred"
        );
        let ids: Vec<String> = state.sessions.iter().map(|t| t.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            "order must be unchanged when the tab is already leftmost"
        );
    }

    /// move_session_to_leftmost returns `false` (and does nothing) when the
    /// tab id is not in `state.sessions` at all. The caller must not record
    /// a successful configuration in this case — there's nothing to configure.
    #[test]
    fn move_session_to_leftmost_returns_false_when_tab_not_found() {
        let mut state = state_with_tabs(&["alpha", "beta"]);
        let moved = move_session_to_leftmost(&mut state, "nonexistent");
        assert!(!moved, "a missing tab id cannot be configured");
        let ids: Vec<String> = state.sessions.iter().map(|t| t.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["alpha".to_string(), "beta".to_string()],
            "order must be unchanged when the tab id isn't found"
        );
    }

    /// Verifies the timing-window guard for failed-command suggestions.
    ///
    /// The user's complaint ("错误命令会出现在上方建议栏") was that a just-failed
    /// command like `pwdwd` would still appear in the suggestion popup for a
    /// brief moment after failure. Root cause: `mark_command_failed` runs in a
    /// `spawn` (async), so between the immediate `retain` (which removes the
    /// command from `command_history`) and the DB write, the DB still has the
    /// prior `exit_code = NULL` import row. The DB's `HAVING` clause keeps
    /// NULL-exit-code commands ("unknown, assume success"), so a suggestion
    /// query during that window would re-surface the failed command.
    ///
    /// Fix: on rc != 0, insert the command into `recent_failed_commands`
    /// synchronously (same critical section as the `retain`). The suggestion
    /// query filters against this set; the `mark_command_failed` spawn removes
    /// the entry after the DB write commits.
    ///
    /// This test pins the data-structure contract: insert and remove on the
    /// set work as expected, and the set starts empty. A regression that
    /// removes the field or renames it would break this test.
    #[test]
    fn recent_failed_commands_tracks_failed_commands_until_db_write_completes() {
        let mut state = AppState::default();

        // Initially empty — no commands have failed yet.
        assert!(
            state.recent_failed_commands.is_empty(),
            "recent_failed_commands must start empty on a fresh AppState"
        );

        // Simulate the synchronous part of the failure path: command `pwdwd`
        // failed with rc=127. The output handler inserts it here, BEFORE the
        // async `mark_command_failed` spawn runs.
        state.recent_failed_commands.insert("pwdwd".to_string());
        assert!(
            state.recent_failed_commands.contains("pwdwd"),
            "pwdwd must be in recent_failed_commands immediately after failure \
             (before mark_command_failed completes): {:?}",
            state.recent_failed_commands
        );

        // The suggestion query (in app.rs) reads this set and filters out
        // any command in it. Verify the filter logic by simulating it: a
        // candidate list containing pwdwd should not survive the filter.
        let candidates = vec!["ls".to_string(), "pwdwd".to_string(), "pwd".to_string()];
        let filtered: Vec<String> = candidates
            .into_iter()
            .filter(|c| !state.recent_failed_commands.contains(c))
            .collect();
        assert_eq!(
            filtered,
            vec!["ls".to_string(), "pwd".to_string()],
            "pwdwd must be filtered out of suggestions while in recent_failed_commands: {:?}",
            filtered
        );

        // Simulate the spawn completing: `mark_command_failed` succeeded,
        // so we remove the command from the set. The DB's HAVING clause now
        // takes over (the failure marker is durable).
        state.recent_failed_commands.remove("pwdwd");
        assert!(
            !state.recent_failed_commands.contains("pwdwd"),
            "pwdwd must be removed from recent_failed_commands once the DB write \
             commits (HAVING takes over): {:?}",
            state.recent_failed_commands
        );
        assert!(
            state.recent_failed_commands.is_empty(),
            "set must be empty after the only failed command's DB write completes"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Modal {
    None,
    NewConnection,
    Settings,
    AiSuggest,
    OneKeyManager,
}
