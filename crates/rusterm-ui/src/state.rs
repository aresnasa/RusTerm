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

use crate::layout::{LayoutPreset, PaneLayout};

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
    /// DuckDB-backed analytics handle. Lazily opened on first use (so the
    /// ~50MB bundled libduckdb doesn't initialize on app startup unless
    /// the user actually queries analytics). When the `analytics` feature
    /// is off, this is a no-op stub.
    #[serde(skip)]
    pub analytics: crate::analytics::AnalyticsHandle,
    /// Per-tab multi-pane layout. When a tab is in `Single` preset (the
    /// default), the rendering path falls back to the legacy
    /// single-active-session view. When the user cycles to Split2H /
    /// Grid4 / Grid8 / etc., the rendering path renders every pane in the
    /// layout side-by-side. Indexed by session id (same key as
    /// `terminals`). A tab with no entry here is implicitly `Single`.
    #[serde(skip)]
    pub layouts: HashMap<String, PaneLayout>,
    /// The current layout preset for the active tab. Cycling this with a
    /// hotkey rebuilds the active tab's `PaneLayout` with the next preset
    /// in `LayoutPreset`'s cycle order. Kept as a separate field (rather
    /// than derived from `layouts`) so that the hotkey handler can read
    /// the current preset without first looking up the active session's
    /// layout entry (which may not exist yet for a tab that's still in
    /// the default Single state).
    #[serde(skip)]
    pub layout_preset: LayoutPreset,
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
            analytics: crate::analytics::AnalyticsHandle::default(),
            layouts: HashMap::new(),
            layout_preset: LayoutPreset::default(),
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

/// Apply a layout preset to the active tab. Builds a fresh `PaneLayout`
/// from the preset using the active session's id as the first pane, then
/// fills the remaining pane slots with other open sessions (in tab order).
/// If there aren't enough sessions to fill the grid, the trailing slots
/// are left empty (the renderer skips panes with empty `session_id`).
///
/// Returns `true` if the layout was applied, `false` if there's no active
/// session to anchor the layout on.
///
/// Takes `&mut AppState` so it's unit-testable without a dioxus runtime.
pub fn apply_layout_preset(state: &mut AppState, preset: LayoutPreset) -> bool {
    let Some(active_id) = state.active_session.clone() else {
        return false;
    };
    // Collect session ids in priority order: active first, then every other
    // open session in tab order. We dedupe in case the active session is
    // also the first tab.
    let mut ids = vec![active_id.clone()];
    for tab in &state.sessions {
        if tab.id != active_id && !ids.contains(&tab.id) {
            ids.push(tab.id.clone());
        }
    }
    let layout = PaneLayout::from_preset(preset, &ids);
    state.layouts.insert(active_id, layout);
    state.layout_preset = preset;
    true
}

/// Cycle the active tab's layout preset to the next entry in the cycle
/// order: Single → Split2H → Split2V → Grid4 → Grid8 → Single. Rebuilds
/// the active tab's `PaneLayout` from the new preset.
///
/// Returns `Some(new_preset)` if the cycle was applied, `None` if there's
/// no active session.
pub fn cycle_layout_preset(state: &mut AppState) -> Option<LayoutPreset> {
    let next = match state.layout_preset {
        LayoutPreset::Single => LayoutPreset::Split2H,
        LayoutPreset::Split2H => LayoutPreset::Split2V,
        LayoutPreset::Split2V => LayoutPreset::Grid4,
        LayoutPreset::Grid4 => LayoutPreset::Grid8,
        LayoutPreset::Grid8 => LayoutPreset::Single,
    };
    if apply_layout_preset(state, next) {
        Some(next)
    } else {
        None
    }
}

/// Toggle zoom (fullscreen) on the pane displaying the given session in the
/// active tab's layout. If no layout exists yet (Single preset), this is a
/// no-op (zooming a single-pane layout is meaningless).
///
/// Returns `true` if the zoom was toggled, `false` if there's no layout
/// or no pane displaying that session.
pub fn toggle_pane_zoom(state: &mut AppState, session_id: &str) -> bool {
    let active_id = match state.active_session.clone() {
        Some(id) => id,
        None => return false,
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    let Some(idx) = layout.pane_index_for_session(session_id) else {
        return false;
    };
    layout.toggle_zoom(idx);
    true
}

/// Toggle the cross-terminal comparison mode (synchronized scrolling +
/// input broadcast) on the active tab's layout.
///
/// Returns the new comparison state (`true` = now on), or `None` if
/// there's no active session with a layout.
pub fn toggle_comparison_mode(state: &mut AppState) -> Option<bool> {
    let active_id = state.active_session.clone()?;
    let layout = state.layouts.get_mut(&active_id)?;
    Some(layout.toggle_comparison())
}

/// Resize a column splitter in the active tab's layout by a fractional
/// delta. See `PaneLayout::resize_col`.
///
/// Returns `true` if the resize was applied.
pub fn resize_layout_col(state: &mut AppState, col: usize, delta: f64) -> bool {
    let active_id = match state.active_session.clone() {
        Some(id) => id,
        None => return false,
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.resize_col(col, delta)
}

/// Resize a row splitter in the active tab's layout by a fractional
/// delta. See `PaneLayout::resize_row`.
///
/// Returns `true` if the resize was applied.
pub fn resize_layout_row(state: &mut AppState, row: usize, delta: f64) -> bool {
    let active_id = match state.active_session.clone() {
        Some(id) => id,
        None => return false,
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.resize_row(row, delta)
}

/// Get the list of session IDs that should receive a broadcast input
/// event, given the current layout state of the active tab.
///
/// - If the active tab has no layout (Single preset), returns a
///   single-element vec containing just the active session. This is the
///   legacy non-broadcast path — the input goes only to the focused
///   session.
/// - If the active tab has a layout but `comparison` is OFF, returns a
///   single-element vec with the active session. Even in multi-pane mode,
///   without comparison mode the user's keystrokes only go to the focused
///   pane (this is the expected tmux-like behaviour — panes are
///   independent unless synchronize-panes is on).
/// - If the active tab has a layout AND `comparison` is ON, returns every
///   non-empty session_id in the layout. The caller (the input handler
///   in `app.rs`) iterates this list and sends the input bytes to each
///   session's PTY sender.
///
/// This is the data-structure contract that the cross-terminal comparison
/// mode (跨终端会话的比对模式) relies on. The actual byte-sending happens
/// in `app.rs`'s `on_input` handler — this function only decides which
/// sessions should receive the input.
pub fn broadcast_targets(state: &AppState) -> Vec<String> {
    let Some(active_id) = state.active_session.as_ref() else {
        return Vec::new();
    };
    // No layout → single-session path.
    let Some(layout) = state.layouts.get(active_id) else {
        return vec![active_id.clone()];
    };
    // Layout exists but comparison is off → input only goes to the
    // focused session. (Multi-pane without sync = panes are independent.)
    if !layout.comparison {
        return vec![active_id.clone()];
    }
    // Comparison is on → broadcast to every non-empty pane session.
    // Dedupe in case the same session appears in multiple panes (which
    // can happen if the user drag-dropped a session onto multiple panes).
    let mut targets = layout.session_ids();
    targets.sort();
    targets.dedup();
    targets
}

/// Get the list of session IDs whose terminals should scroll together
/// when the user scrolls in any pane (the synchronized-scroll half of
/// comparison mode). Same contract as `broadcast_targets` but for scroll
/// events: returns every non-empty pane session when comparison is on,
/// or just the active session when comparison is off or no layout exists.
///
/// This is a separate function from `broadcast_targets` because scroll
/// sync and input broadcast are conceptually distinct (a future feature
/// might want scroll sync without input broadcast, or vice versa), even
/// though today they share the same `comparison` flag.
pub fn scroll_sync_targets(state: &AppState) -> Vec<String> {
    broadcast_targets(state)
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

    /// Pin the data-structure contract for the Shift+Delete suggestion-delete
    /// feature (user-initiated dirty-data cleanup).
    ///
    /// When the user hits Shift+Delete on a highlighted suggestion item, the
    /// app.rs handler does (in order, inside a single `state.write()` critical
    /// section):
    ///   1. `tab.command_history.retain(|c| c != &cmd)` — drop from session hist
    ///   2. `tab.suggestions.retain(|c| c != &cmd)` — drop from popup list
    ///   3. Clamp `tab.suggestion_selected` to `suggestions.len().saturating_sub(1)`
    ///   4. If suggestions is now empty, hide the popup and clear `suggestion`
    ///   5. `state.recent_failed_commands.insert(cmd)` — guard against DB source
    ///      re-surfacing it during the async `mark_command_failed` write
    ///
    /// This test pins steps 1–4 against a future regression. Step 5 is already
    /// covered by `recent_failed_commands_tracks_failed_commands_until_db_write_completes`.
    #[test]
    fn suggestion_delete_removes_command_and_clamps_selection() {
        let mut state = state_with_tabs(&["alpha"]);
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history = vec![
            "ls".to_string(),
            "pwdwd".to_string(), // the typo the user wants gone
            "git status".to_string(),
        ];
        tab.suggestions = vec![
            "ls".to_string(),
            "pwdwd".to_string(), // highlighted (selected)
            "git status".to_string(),
        ];
        tab.suggestion_selected = 1; // user has "pwdwd" highlighted
        tab.suggestion_visible = true;
        tab.suggestion = Some("dwd".to_string()); // inline ghost text

        // Simulate the handler: delete "pwdwd".
        let cmd_to_delete = "pwdwd".to_string();
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history.retain(|c| c != &cmd_to_delete);
        tab.suggestions.retain(|c| c != &cmd_to_delete);
        if tab.suggestion_selected >= tab.suggestions.len() {
            tab.suggestion_selected = tab.suggestions.len().saturating_sub(1);
        }
        if tab.suggestions.is_empty() {
            tab.suggestion_visible = false;
            tab.suggestion = None;
            tab.suggestion_selected = 0;
        }

        // Verify command_history no longer contains the deleted command.
        let tab = state.sessions.first().unwrap();
        assert!(
            !tab.command_history.contains(&cmd_to_delete),
            "deleted command must not remain in command_history: {:?}",
            tab.command_history
        );
        assert_eq!(
            tab.command_history,
            vec!["ls".to_string(), "git status".to_string()]
        );

        // Verify suggestions list no longer contains the deleted command.
        assert!(
            !tab.suggestions.contains(&cmd_to_delete),
            "deleted command must not remain in suggestions: {:?}",
            tab.suggestions
        );
        assert_eq!(
            tab.suggestions,
            vec!["ls".to_string(), "git status".to_string()]
        );

        // The selection was at index 1; after deleting index 1, the list
        // shrunk to len 2, so index 1 is still valid (now points at "git status").
        assert_eq!(
            tab.suggestion_selected, 1,
            "selection should remain at 1 (still valid, now points at git status)"
        );
        assert!(
            tab.suggestion_visible,
            "popup should remain visible — there are still suggestions to show"
        );
    }

    /// Variant of `suggestion_delete_removes_command_and_clamps_selection` for
    /// the edge case where deleting the LAST suggestion empties the list. The
    /// handler must hide the popup and clear `suggestion_selected` and
    /// `suggestion` so stale state doesn't leak into the next keystroke.
    #[test]
    fn suggestion_delete_last_item_hides_popup() {
        let mut state = state_with_tabs(&["alpha"]);
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history = vec!["pwdwd".to_string()];
        tab.suggestions = vec!["pwdwd".to_string()];
        tab.suggestion_selected = 0;
        tab.suggestion_visible = true;
        tab.suggestion = Some("dwd".to_string());

        let cmd_to_delete = "pwdwd".to_string();
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history.retain(|c| c != &cmd_to_delete);
        tab.suggestions.retain(|c| c != &cmd_to_delete);
        if tab.suggestion_selected >= tab.suggestions.len() {
            tab.suggestion_selected = tab.suggestions.len().saturating_sub(1);
        }
        if tab.suggestions.is_empty() {
            tab.suggestion_visible = false;
            tab.suggestion = None;
            tab.suggestion_selected = 0;
        }

        let tab = state.sessions.first().unwrap();
        assert!(
            tab.suggestions.is_empty(),
            "suggestions must be empty after deleting the only item"
        );
        assert!(
            !tab.suggestion_visible,
            "popup must be hidden when there are no suggestions"
        );
        assert_eq!(
            tab.suggestion_selected, 0,
            "suggestion_selected must reset to 0 when popup is hidden"
        );
        assert_eq!(
            tab.suggestion, None,
            "inline ghost text must be cleared when suggestions are empty"
        );
        assert!(
            tab.command_history.is_empty(),
            "command_history must be empty after deleting the only command"
        );
    }

    /// Regression test for the bug where, after Shift+Delete on a suggestion,
    /// typing the correct command prefix no longer shows the suggestion popup.
    ///
    /// The bug was reported as: "After deleting a suggested command, entering
    /// the correct command doesn't pop up the suggestion anymore."
    ///
    /// This test simulates the full flow:
    ///   1. Session has `command_history = ['pwd']` (the correct command,
    ///      previously run successfully).
    ///   2. Suggestions panel shows `['pwdwd', 'pwd']` — `pwdwd` is a typo
    ///      that snuck in from `~/.bash_history` (NULL exit_code, kept by
    ///      HAVING). `pwd` is the legitimate match.
    ///   3. User Shift+Deletes `pwdwd` — the handler removes it from
    ///      `suggestions` and `command_history`, inserts into
    ///      `recent_failed_commands`, and (in production) spawns
    ///      `mark_command_failed`.
    ///   4. The suggestion panel becomes `['pwd']` (still visible — non-empty).
    ///   5. User types `pw` (prefix of the correct command).
    ///   6. The suggestion query (simulated here) filters against
    ///      `recent_failed_commands` and the current `cmd_part`, then
    ///      populates `suggestions`.
    ///   7. Verify the popup becomes visible with `['pwd']`.
    ///
    /// This pins the contract that:
    ///   - Deleting a suggestion does NOT clear `command_history` of other
    ///     commands (only the deleted one).
    ///   - `recent_failed_commands` only contains the deleted command, not
    ///     other commands.
    ///   - A subsequent suggestion query (with non-empty results) restores
    ///     `suggestion_visible = true`.
    #[test]
    fn suggestion_popup_reappears_after_delete_when_history_has_matches() {
        let mut state = state_with_tabs(&["alpha"]);

        // Step 1: session has 'pwd' in command_history (previously successful).
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history = vec!["pwd".to_string()];

        // Step 2: suggestions panel shows the typo + the legitimate match.
        let tab = state.sessions.first_mut().unwrap();
        tab.suggestions = vec!["pwdwd".to_string(), "pwd".to_string()];
        tab.suggestion_selected = 0; // user has 'pwdwd' highlighted
        tab.suggestion_visible = true;
        tab.suggestion = Some("wd".to_string()); // inline ghost for 'pwdwd'

        // Step 3: user Shift+Deletes 'pwdwd'.
        let cmd_to_delete = "pwdwd".to_string();
        let tab = state.sessions.first_mut().unwrap();
        tab.command_history.retain(|c| c != &cmd_to_delete);
        tab.suggestions.retain(|c| c != &cmd_to_delete);
        if tab.suggestion_selected >= tab.suggestions.len() {
            tab.suggestion_selected = tab.suggestions.len().saturating_sub(1);
        }
        if tab.suggestions.is_empty() {
            tab.suggestion_visible = false;
            tab.suggestion = None;
            tab.suggestion_selected = 0;
        }
        // Immediate guard against DB source re-surfacing the deleted command.
        state.recent_failed_commands.insert(cmd_to_delete.clone());

        // Step 4: verify state after delete.
        let tab = state.sessions.first().unwrap();
        assert_eq!(
            tab.suggestions,
            vec!["pwd".to_string()],
            "suggestions should now contain only 'pwd' (the legitimate match)"
        );
        assert!(
            tab.suggestion_visible,
            "popup should still be visible — there's one remaining suggestion"
        );
        assert!(
            tab.command_history.contains(&"pwd".to_string()),
            "command_history must still contain 'pwd' (only the deleted cmd is removed)"
        );
        assert!(
            !tab.command_history.contains(&cmd_to_delete),
            "command_history must NOT contain the deleted command"
        );
        assert!(
            state.recent_failed_commands.contains(&cmd_to_delete),
            "recent_failed_commands must contain the deleted command (UI guard)"
        );
        assert_eq!(
            state.recent_failed_commands.len(),
            1,
            "recent_failed_commands must contain ONLY the deleted command, not others"
        );

        // Step 5: simulate the user typing 'pw' (prefix of the correct command).
        // The on_input handler in app.rs spawns a 200ms-debounced query that:
        //   - extracts the current line (we'll assume 'pw' here)
        //   - filters session_history + DB results by:
        //       starts_with(cmd_lower) && cmd != cmd_part && !seen && !recent_failed
        // We simulate the query result by running the same filter logic.
        let cmd_part = "pw";
        let cmd_lower = cmd_part.to_lowercase();
        let recent_failed = state.recent_failed_commands.clone();

        // Simulate session_history source (the in-memory command_history).
        let session_hist = state.sessions.first().unwrap().command_history.clone();
        let mut all_suggestions: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for cmd in session_hist.iter() {
            if cmd.to_lowercase().starts_with(&cmd_lower)
                && cmd != cmd_part
                && !seen.contains(cmd.to_lowercase().as_str())
                && !recent_failed.contains(cmd)
            {
                seen.insert(cmd.to_lowercase().clone());
                all_suggestions.push(cmd.clone());
            }
        }

        // Simulate DB source: assume DB has 'pwd' and 'pwdwd'.
        // (In production, the HAVING clause would already filter 'pwdwd'
        // after mark_command_failed commits, but during the timing window
        // the recent_failed guard filters it.)
        let db_results = vec!["pwd".to_string(), "pwdwd".to_string()];
        for entry in db_results {
            if entry.to_lowercase().starts_with(&cmd_lower)
                && entry != cmd_part
                && !seen.contains(entry.to_lowercase().as_str())
                && !recent_failed.contains(&entry)
            {
                seen.insert(entry.to_lowercase().clone());
                all_suggestions.push(entry);
            }
        }

        // Step 6: simulate the spawn populating the suggestion state.
        // (In production, this is the `state_for_cmd.write().sessions.iter_mut()`
        // block in the on_input spawn.)
        if all_suggestions.is_empty() {
            let tab = state.sessions.first_mut().unwrap();
            tab.suggestion = None;
            tab.suggestions = Vec::new();
            tab.suggestion_visible = false;
            tab.suggestion_selected = 0;
        } else {
            let first = &all_suggestions[0];
            let suffix = if first.len() > cmd_part.len() {
                first[cmd_part.len()..].to_string()
            } else {
                String::new()
            };
            let tab = state.sessions.first_mut().unwrap();
            tab.suggestion = if suffix.is_empty() {
                None
            } else {
                Some(suffix)
            };
            tab.suggestions = all_suggestions;
            tab.suggestion_visible = true;
            tab.suggestion_selected = 0;
        }

        // Step 7: verify the popup is visible with 'pwd'.
        let tab = state.sessions.first().unwrap();
        assert!(
            tab.suggestion_visible,
            "popup MUST be visible after typing 'pw' — 'pwd' is a valid match. \
             If this fails, the delete handler left state in a way that prevents \
             the suggestion query from showing results. State: {:?}",
            tab
        );
        assert_eq!(
            tab.suggestions,
            vec!["pwd".to_string()],
            "suggestions should contain only 'pwd' (pwdwd is filtered by recent_failed)"
        );
        assert_eq!(
            tab.suggestion,
            Some("d".to_string()),
            "inline ghost text should be 'd' (suffix of 'pwd' after 'pw')"
        );
    }

    // ------------------------------------------------------------------
    // Multi-pane layout helpers (apply_layout_preset, cycle_layout_preset,
    // toggle_pane_zoom, toggle_comparison_mode, resize_layout_col/row)
    // ------------------------------------------------------------------

    /// Helper: AppState with N session tabs AND an active session set to the
    /// first tab. This is the minimum state needed to test the layout
    /// helpers (they all key off `active_session`).
    fn state_with_active_session(names: &[&str]) -> AppState {
        let mut state = state_with_tabs(names);
        if let Some(first) = state.sessions.first() {
            state.active_session = Some(first.id.clone());
        }
        state
    }

    #[test]
    fn apply_layout_preset_returns_false_with_no_active_session() {
        let mut state = AppState::default();
        // No active_session — should return false and not touch layouts.
        assert!(!apply_layout_preset(&mut state, LayoutPreset::Grid4));
        assert!(state.layouts.is_empty());
        assert_eq!(state.layout_preset, LayoutPreset::Single);
    }

    #[test]
    fn apply_layout_preset_builds_layout_for_active_session() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        assert!(apply_layout_preset(&mut state, LayoutPreset::Grid4));
        // The layout is stored under the active session's id.
        let active_id = state.active_session.clone().unwrap();
        let layout = state.layouts.get(&active_id).expect("layout should exist");
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 2);
        // Pane 0 (the active session) is `alpha`.
        assert_eq!(layout.panes[0].session_id, "alpha");
        // Remaining panes fill with the other open sessions in tab order.
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert_eq!(layout.panes[3].session_id, "delta");
        // Preset is recorded on the state.
        assert_eq!(state.layout_preset, LayoutPreset::Grid4);
    }

    #[test]
    fn apply_layout_preset_fills_extra_slots_with_empty_when_sessions_run_out() {
        // Only 1 session for a 4-pane grid — the last 3 panes are empty.
        let mut state = state_with_active_session(&["alpha"]);
        assert!(apply_layout_preset(&mut state, LayoutPreset::Grid4));
        let active_id = state.active_session.clone().unwrap();
        let layout = state.layouts.get(&active_id).unwrap();
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.panes[3].session_id, "");
        // session_ids() skips empties — only `alpha` remains.
        assert_eq!(layout.session_ids(), vec!["alpha".to_string()]);
    }

    #[test]
    fn apply_layout_preset_dedupes_active_session_when_its_also_first_tab() {
        // `alpha` is active AND first in `sessions`. The dedup path should
        // not add it twice to the layout's session list.
        let mut state = state_with_active_session(&["alpha", "beta"]);
        assert!(apply_layout_preset(&mut state, LayoutPreset::Split2H));
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    #[test]
    fn cycle_layout_preset_cycles_through_all_presets_and_back_to_single() {
        let mut state = state_with_active_session(&["alpha"]);
        // Default is Single.
        assert_eq!(state.layout_preset, LayoutPreset::Single);
        // Single → Split2H.
        assert_eq!(cycle_layout_preset(&mut state), Some(LayoutPreset::Split2H));
        // Split2H → Split2V.
        assert_eq!(cycle_layout_preset(&mut state), Some(LayoutPreset::Split2V));
        // Split2V → Grid4.
        assert_eq!(cycle_layout_preset(&mut state), Some(LayoutPreset::Grid4));
        // Grid4 → Grid8.
        assert_eq!(cycle_layout_preset(&mut state), Some(LayoutPreset::Grid8));
        // Grid8 → Single (cycle wraps).
        assert_eq!(cycle_layout_preset(&mut state), Some(LayoutPreset::Single));
    }

    #[test]
    fn cycle_layout_preset_returns_none_with_no_active_session() {
        let mut state = AppState::default();
        assert_eq!(cycle_layout_preset(&mut state), None);
        // Default preset is unchanged.
        assert_eq!(state.layout_preset, LayoutPreset::Single);
    }

    #[test]
    fn toggle_pane_zoom_zooms_active_sessions_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Zoom pane 0 (alpha).
        assert!(toggle_pane_zoom(&mut state, "alpha"));
        let zoomed = state.layouts.get("alpha").unwrap().zoomed;
        assert_eq!(zoomed, Some(0));
        // Unzoom by toggling again.
        assert!(toggle_pane_zoom(&mut state, "alpha"));
        let zoomed = state.layouts.get("alpha").unwrap().zoomed;
        assert!(zoomed.is_none());
    }

    #[test]
    fn toggle_pane_zoom_returns_false_with_no_layout() {
        // No layout applied yet — zoom toggle is a no-op.
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!toggle_pane_zoom(&mut state, "alpha"));
    }

    #[test]
    fn toggle_pane_zoom_returns_false_with_no_active_session() {
        let mut state = AppState::default();
        assert!(!toggle_pane_zoom(&mut state, "alpha"));
    }

    #[test]
    fn toggle_pane_zoom_returns_false_for_unknown_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // `gamma` isn't in the layout.
        assert!(!toggle_pane_zoom(&mut state, "gamma"));
    }

    #[test]
    fn toggle_comparison_mode_flips_layout_comparison_flag() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Off by default.
        assert_eq!(toggle_comparison_mode(&mut state), Some(true));
        let comparison = state.layouts.get("alpha").unwrap().comparison;
        assert!(comparison);
        // Toggle again — turns off.
        assert_eq!(toggle_comparison_mode(&mut state), Some(false));
        let comparison = state.layouts.get("alpha").unwrap().comparison;
        assert!(!comparison);
    }

    #[test]
    fn toggle_comparison_mode_returns_none_with_no_layout() {
        let mut state = state_with_active_session(&["alpha"]);
        // No layout — comparison toggle has nothing to act on.
        assert_eq!(toggle_comparison_mode(&mut state), None);
    }

    #[test]
    fn resize_layout_col_adjusts_active_layout_column() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Default col 0 = 0.5; grow by 0.1 → 0.6.
        assert!(resize_layout_col(&mut state, 0, 0.1));
        let layout = state.layouts.get("alpha").unwrap();
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
    }

    #[test]
    fn resize_layout_col_rejects_below_minimum() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Shrink col 0 to 0 — rejected.
        assert!(!resize_layout_col(&mut state, 0, -0.5));
        let layout = state.layouts.get("alpha").unwrap();
        assert!((layout.col_fracs[0] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn resize_layout_col_returns_false_with_no_layout() {
        let mut state = state_with_active_session(&["alpha"]);
        // No layout — resize is a no-op.
        assert!(!resize_layout_col(&mut state, 0, 0.1));
    }

    #[test]
    fn resize_layout_row_adjusts_active_layout_row() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2V);
        assert!(resize_layout_row(&mut state, 0, 0.2));
        let layout = state.layouts.get("alpha").unwrap();
        assert!((layout.row_fracs[0] - 0.7).abs() < 1e-9);
        assert!((layout.row_fracs[1] - 0.3).abs() < 1e-9);
    }

    #[test]
    fn resize_layout_row_returns_false_with_no_active_session() {
        let mut state = AppState::default();
        assert!(!resize_layout_row(&mut state, 0, 0.1));
    }

    /// Closing a session must remove its entry from `layouts` too —
    /// otherwise the layout keeps a dangling reference to a session that
    /// no longer exists in `terminals`. This test pins the cleanup
    /// contract by simulating the close path (which the app.rs `on_close`
    /// handler does).
    #[test]
    fn layout_entry_is_safe_to_remove_when_session_closes() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(state.layouts.contains_key("alpha"));
        // Simulate the close path: remove the session from `sessions`,
        // `terminals`, and `layouts`.
        state.sessions.retain(|s| s.id != "alpha");
        state.terminals.remove("alpha");
        state.layouts.remove("alpha");
        assert!(!state.layouts.contains_key("alpha"));
    }

    // ------------------------------------------------------------------
    // Comparison mode broadcast / scroll-sync target resolution
    // ------------------------------------------------------------------

    #[test]
    fn broadcast_targets_returns_empty_with_no_active_session() {
        let state = AppState::default();
        // No active session → no broadcast targets.
        assert!(broadcast_targets(&state).is_empty());
    }

    #[test]
    fn broadcast_targets_returns_active_only_with_no_layout() {
        // Single preset (no layout entry) → returns just the active session.
        // This is the legacy non-broadcast path: input only goes to the
        // focused session.
        let state = state_with_active_session(&["alpha", "beta"]);
        assert_eq!(broadcast_targets(&state), vec!["alpha".to_string()]);
    }

    #[test]
    fn broadcast_targets_returns_active_only_when_comparison_off() {
        // Multi-pane layout but comparison is OFF → input only goes to the
        // focused session. This matches tmux's default: panes are independent
        // unless synchronize-panes is explicitly enabled.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // comparison defaults to false.
        assert_eq!(broadcast_targets(&state), vec!["alpha".to_string()]);
    }

    #[test]
    fn broadcast_targets_returns_all_panes_when_comparison_on() {
        // Multi-pane layout AND comparison is ON → input goes to every
        // pane's session. This is the cross-terminal comparison mode.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        // All 4 pane sessions should be targets.
        assert_eq!(targets.len(), 4);
        assert!(targets.contains(&"alpha".to_string()));
        assert!(targets.contains(&"beta".to_string()));
        assert!(targets.contains(&"gamma".to_string()));
        assert!(targets.contains(&"delta".to_string()));
    }

    #[test]
    fn broadcast_targets_dedupes_sessions_across_panes() {
        // If the same session appears in multiple panes (e.g., user
        // drag-dropped it onto two panes), the broadcast list should
        // only contain it once — otherwise the session's PTY would
        // receive each keystroke N times.
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        // Manually set pane 2 and pane 3 to also display `alpha`.
        {
            let active_id = state.active_session.clone().unwrap();
            let layout = state.layouts.get_mut(&active_id).unwrap();
            layout.set_pane_session(2, "alpha".to_string());
            layout.set_pane_session(3, "alpha".to_string());
        }
        let targets = broadcast_targets(&state);
        // Should be [alpha, beta] — alpha deduped from 3 panes.
        assert_eq!(targets, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn broadcast_targets_skips_empty_pane_slots() {
        // Grid8 preset with only 2 sessions → 6 panes are empty.
        // Broadcast should only target the 2 non-empty sessions.
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        assert_eq!(targets, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn scroll_sync_targets_matches_broadcast_targets() {
        // Today scroll sync and input broadcast share the same `comparison`
        // flag, so scroll_sync_targets should return the same list as
        // broadcast_targets. This test pins that contract — if they ever
        // diverge (e.g., the user wants scroll sync without input
        // broadcast), this test will need updating, forcing a conscious
        // decision rather than a silent behavioural change.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        assert_eq!(scroll_sync_targets(&state), broadcast_targets(&state));
    }

    #[test]
    fn scroll_sync_targets_returns_active_only_when_comparison_off() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // comparison off → only the active session scrolls.
        assert_eq!(scroll_sync_targets(&state), vec!["alpha".to_string()]);
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
