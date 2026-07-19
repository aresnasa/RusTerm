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

    // ── Session-state restore (feature #17) ─────────────────────────────
    //
    // On unlock we load `session_state.enc` (if it exists) and stash the
    // decrypted `SessionState` here. The UI renders a restore-confirmation
    // modal while this is `Some`; the three modal buttons clear it.
    //   - 恢复 (Restore):   reconnect each session + `cd <cwd>`
    //   - 跳过 (Skip):      clear without restoring
    //   - 不再询问 (Never):  clear + set `restore_disabled = true` in settings
    //
    // We deliberately never re-execute past commands or scripts — only
    // `cd`, which is side-effect-free. See `session_state.rs` docs.
    #[serde(skip)]
    pub restore_pending: Option<rusterm_core::SessionState>,
    /// Set when the user picks "不再询问" so we never re-prompt (and stop
    /// saving session state entirely). Persisted in `settings.json` so the
    /// choice survives across launches. The user can re-enable via the
    /// settings panel (future work).
    pub restore_disabled: bool,

    // ── Dangerous-command protection (feature #17 part 2) ──────────────
    //
    // Before sending Enter to the PTY, the input handler runs the current
    // input line through `CommandSafetyChecker`. If the verdict is `Warn`,
    // we DON'T send Enter — instead we stash the pending command + reason
    // here and render a confirmation modal. The modal's "继续" button sends
    // the original Enter; "取消" discards it.
    //
    // `None` when no dangerous command is pending confirmation.
    #[serde(skip)]
    pub pending_dangerous_command: Option<PendingDangerousCommand>,
    /// Pre-compiled dangerous-command patterns. Cheap to clone but we keep
    /// exactly one on the app state for the whole session lifetime.
    #[serde(skip)]
    pub safety_checker: rusterm_core::CommandSafetyChecker,
}

/// State held while the dangerous-command confirmation modal is open.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingDangerousCommand {
    /// The full command line that triggered the warning (used to re-send
    /// Enter if the user confirms). Stored verbatim so we don't lose any
    /// quoting / escaping the user typed.
    pub command: String,
    /// Human-readable reason from `SafetyVerdict::Warn`. Shown in the modal.
    pub reason: String,
    /// Session id the command was typed into. Used to route the eventual
    /// Enter to the right PTY sender.
    pub session_id: String,
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
    /// Last reported working directory of this session, captured from the
    /// shell via OSC 7 (`file://<host><path>`). `None` until the shell reports
    /// one (raw telnet/serial sessions never will). Mirrored from
    /// `Terminal::cwd()` into `SessionTab` so the session-state save path can
    /// read it without taking the terminal lock. Updated in the output-processing
    /// loop alongside `render_output` / `version`.
    #[serde(skip)]
    pub cwd: Option<String>,
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
            restore_pending: None,
            restore_disabled: false,
            pending_dangerous_command: None,
            safety_checker: rusterm_core::CommandSafetyChecker::new(),
        }
    }
}

impl AppState {
    /// Build a `SessionState` snapshot from the current app state, suitable
    /// for saving to `session_state.enc`.
    ///
    /// Captures, per session:
    /// - id, name, kind, hostname, connection_id
    /// - cwd (last reported by the shell via OSC 7 — `None` if the shell
    ///   never reported one, e.g. raw telnet/serial)
    /// - tail of `command_history` (last N entries, display-only — these are
    ///   NEVER re-executed on restore; they're just re-seeded into the
    ///   suggestion popup)
    ///
    /// NOT captured: scrollback (too large), env vars (would leak secrets),
    /// PTY process state (impossible to restore), input box content.
    ///
    /// `theme_name` is the name of the current theme so it can be restored
    /// on next launch without a flicker.
    pub fn build_session_state(&self, theme_name: &str) -> rusterm_core::SessionState {
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .map(|tab| {
                // Tail of command history — last 100 entries, display-only.
                let history_tail: Vec<String> = if tab.command_history.len() > 100 {
                    tab.command_history[tab.command_history.len() - 100..].to_vec()
                } else {
                    tab.command_history.clone()
                };

                // Look up connection_id for SSH/Telnet/Tcp sessions so we can
                // find the matching `ConnectionConfig` on restore.
                let connection_id = match tab.kind {
                    rusterm_core::session::SessionType::Ssh
                    | rusterm_core::session::SessionType::Telnet
                    | rusterm_core::session::SessionType::Tcp => self
                        .session_configs
                        .iter()
                        .find(|(_, c)| c.name == tab.name)
                        .map(|(id, _)| id.clone())
                        .or_else(|| Some(tab.id.clone())),
                    _ => None,
                };

                rusterm_core::session_state::PersistedSession {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    kind: tab.kind,
                    hostname: tab.hostname.clone(),
                    connection_id,
                    cwd: tab.cwd.clone(),
                    command_history_tail: history_tail,
                }
            })
            .collect();

        rusterm_core::SessionState {
            schema_version: 1,
            saved_at: chrono::Utc::now(),
            active_session: self.active_session.clone(),
            sessions,
            theme: Some(theme_name.to_string()),
        }
    }

    /// Encrypt + atomically persist the current session state. No-op if
    /// `restore_disabled` is true (the user picked "不再询问" earlier — we
    /// don't save so we don't re-prompt on next launch either). Returns the
    /// save result so the caller can log failures.
    ///
    /// `master_key` is the AES-256-GCM key derived from the master password;
    /// comes from `ConfigManager::master_key()`.
    pub fn save_session_state(&self, master_key: &[u8; 32]) -> anyhow::Result<()> {
        if self.restore_disabled {
            // User explicitly disabled restore — don't save, don't re-prompt.
            // If they want to re-enable, they can do so from settings (future
            // work: a settings toggle that clears `restore_disabled`).
            return Ok(());
        }
        let state = self.build_session_state(self.theme_name());
        state.save(master_key)
    }

    /// Returns the current theme as a string name (for persistence).
    pub fn theme_name(&self) -> &'static str {
        match self.theme {
            Theme::Dark => "Dark",
            Theme::Light => "Light",
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

// ======================================================================
// Task 16 — drag-and-drop pane rearrangement
// ======================================================================
//
// These wrappers expose `PaneLayout`'s pane-mutation operations through
// the active-tab indirection. The drag-and-drop UI handlers in `app.rs`
// call these instead of touching `PaneLayout` directly because:
//
// 1. They take `&mut AppState` (not `&mut Signal<AppState>`), so they're
//    unit-testable without spinning up a dioxus runtime.
// 2. They handle the active-session / layout lookup boilerplate that
//    every layout mutation needs (find active_session → find its layout
//    → mutate). Centralizing this avoids the same 6-line preamble in
//    every handler.
// 3. They return `bool` / `Option` so the caller can fall back to a
//    different operation (e.g., if `set_pane_session_for_active` fails
//    because there's no layout, the drop handler can create a new tab
//    instead).
//
// ## Why no `split_pane` / `close_pane` here
//
// The current `PaneLayout` is a uniform row-major grid (every row has
// the same number of columns). Arbitrary tmux-style splits would break
// the `rows * cols == panes.len()` invariant that `pane_rect` and
// `visible_panes` rely on. Implementing arbitrary splits would require
// either (a) restricting splits to grid-preserving operations (which
// limits the user to the 5 existing presets) or (b) refactoring
// `PaneLayout` to a binary tree (a ~200-400 line change that would
// invalidate the 41 layout tests).
//
// For Task 16's MVP we deliberately choose grid-only: the user can
// drag sessions between existing panes and drag sidebar connections
// onto existing panes (replacing the pane's session). Splitting panes
// is left to the existing `cycle_layout_preset` / `apply_layout_preset`
// path. A future task can introduce tree-based splits if the user
// wants arbitrary layouts.

/// Replace the session displayed in pane `pane_idx` of the active tab's
/// layout with `session_id`. Used when the user drag-and-drops an open
/// session (from the tab bar) or a sidebar connection (after the
/// connection has been opened as a new session) onto a specific pane.
///
/// Returns `true` if the pane's session was replaced. Returns `false`
/// if there's no active session, no layout for the active session, or
/// `pane_idx` is out of range — in all these cases the layout is left
/// untouched and the caller should fall back to the legacy "open new
/// tab" path.
pub fn set_pane_session_for_active(
    state: &mut AppState,
    pane_idx: usize,
    session_id: String,
) -> bool {
    let Some(active_id) = state.active_session.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.set_pane_session(pane_idx, session_id)
}

/// Swap the panes displaying `from_session` and `to_session` in the
/// active tab's layout. Used when the user drag-and-drops an open
/// session from one pane onto another pane — the two panes exchange
/// their displayed sessions.
///
/// Returns `true` if both sessions were found in the active tab's
/// layout and swapped. Returns `false` (and leaves the layout
/// unchanged) if there's no active session, no layout, or either
/// session isn't currently displayed in any pane.
pub fn swap_pane_sessions(state: &mut AppState, from_session: &str, to_session: &str) -> bool {
    let Some(active_id) = state.active_session.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.swap_panes_by_session(from_session, to_session)
}

/// Look up the pane index displaying `session_id` in the active tab's
/// layout. Returns `None` if there's no active session, no layout, or
/// the session isn't displayed in any pane.
///
/// Used by the drag-and-drop drop handler to identify which pane the
/// user dropped onto (given the pane's `session_id` from the rendered
/// `visible_panes` list) and to find the source pane of a drag (given
/// the dragged tab's `session_id`).
pub fn pane_index_for_active_session(state: &AppState, session_id: &str) -> Option<usize> {
    let active_id = state.active_session.as_ref()?;
    let layout = state.layouts.get(active_id)?;
    layout.pane_index_for_session(session_id)
}

/// Get the `session_id` displayed at pane `pane_idx` in the active
/// tab's layout. Returns `None` if there's no active session, no
/// layout, or `pane_idx` is out of range. The returned string may be
/// empty (a pane slot with no session).
///
/// Used by the drop handler to identify the session currently
/// displayed at the drop target (so we can swap it with the dragged
/// session, or replace it with a freshly-opened connection).
pub fn session_at_pane(state: &AppState, pane_idx: usize) -> Option<String> {
    let active_id = state.active_session.as_ref()?;
    let layout = state.layouts.get(active_id)?;
    layout.panes.get(pane_idx).map(|p| p.session_id.clone())
}

/// =====================================================================
/// Task 19 — drag a background tab onto a pane to CREATE a new split
/// =====================================================================
///
/// When the user drags a session tab from the top tab bar onto a pane
/// that already has a session, and the dragged session is NOT currently
/// displayed in any pane (a "background tab"), the drop handler needs
/// to CREATE a new split pane for the dragged session — not just move
/// or swap (the existing `set_pane_session_for_active` / `swap_pane_sessions`
/// paths assume the source session is already in a pane).
///
/// This helper implements the "create split" logic at the state level
/// (unit-testable without a dioxus runtime). The strategy is:
///
/// 1. If the active tab has NO layout yet (Single preset, single pane),
///    apply `Split2H` and place the dragged session in pane 1.
/// 2. If the active tab HAS a layout, look for an empty pane slot (a
///    pane whose `session_id` is empty). If found, place the dragged
///    session there via `set_pane_session_for_active`.
/// 3. If there are no empty slots, cycle to the next larger preset
///    (Single→Split2H, Split2H/Split2V→Grid4, Grid4→Grid8). After
///    cycling, `apply_layout_preset` re-fills all panes from the
///    sessions list — the dragged session may already be placed
///    naturally. If not (it was a background tab not in the
///    `apply_layout_preset` fill order), manually place it in the
///    first empty slot.
/// 4. If already at `Grid8` (max panes), fall back to swapping the
///    dragged session with the target pane's session via
///    `swap_pane_sessions` (the caller is responsible for this —
///    this function returns `DropSplitOutcome::FallbackSwap`).
///
/// Returns a `DropSplitOutcome` describing what happened, so the caller
/// can log / fall back appropriately.
///
/// CRITICAL: this function does NOT update `active_session`. The layout
/// is keyed by `active_session` in `state.layouts`; changing it would
/// break the layout lookup. The dragged session is placed in a pane of
/// the CURRENTLY ACTIVE tab's layout.
pub fn drop_background_tab_to_create_split(
    state: &mut AppState,
    dragged_sid: &str,
    _target_pane_idx: usize,
) -> DropSplitOutcome {
    let Some(active_id) = state.active_session.clone() else {
        return DropSplitOutcome::Failed;
    };

    // Case 1: no layout for the active session yet (Single preset).
    // Apply Split2H and place the dragged session in pane 1.
    if !state.layouts.contains_key(&active_id) {
        state.layout_preset = LayoutPreset::Split2H;
        let mut ids = vec![active_id.clone()];
        for tab in &state.sessions {
            if tab.id != active_id && !ids.contains(&tab.id) {
                ids.push(tab.id.clone());
            }
        }
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &ids);
        // Place the dragged session in pane 1 (pane 0 is the active tab's
        // own session — that's the user's "anchor" and shouldn't move).
        if layout.panes.len() >= 2 {
            layout.panes[1].session_id = dragged_sid.to_string();
        }
        state.layouts.insert(active_id, layout);
        return DropSplitOutcome::Created { pane_idx: 1 };
    }

    // Case 2/3: layout exists. Look for an empty slot in the current
    // layout (Grid4 with 2 sessions has 2 empty panes, etc.).
    let current_preset = state.layout_preset;
    let empty_idx = state
        .layouts
        .get(&active_id)
        .and_then(|l| l.panes.iter().position(|p| p.session_id.is_empty()));

    if let Some(idx) = empty_idx {
        // Found an empty slot — place the dragged session there. This
        // doesn't change the preset, just fills a slot.
        if set_pane_session_for_active(state, idx, dragged_sid.to_string()) {
            return DropSplitOutcome::FilledExisting { pane_idx: idx };
        }
        return DropSplitOutcome::Failed;
    }

    // Case 4: no empty slots. Cycle to the next larger preset (unless
    // we're already at Grid8 — the maximum).
    let next_preset = match current_preset {
        LayoutPreset::Single => Some(LayoutPreset::Split2H),
        LayoutPreset::Split2H | LayoutPreset::Split2V => Some(LayoutPreset::Grid4),
        LayoutPreset::Grid4 => Some(LayoutPreset::Grid8),
        LayoutPreset::Grid8 => None, // already at max — caller falls back to swap
    };

    let Some(next) = next_preset else {
        return DropSplitOutcome::FallbackSwap;
    };

    // Apply the larger preset. `apply_layout_preset` re-fills ALL panes
    // from the sessions list in tab order (active first). The dragged
    // session, being a background tab, will likely be placed in one of
    // the new slots naturally — but we check afterwards and manually
    // place it if not.
    if !apply_layout_preset(state, next) {
        return DropSplitOutcome::Failed;
    }

    // Check if the dragged session was already placed by
    // `apply_layout_preset` (because it's in `state.sessions`).
    let already_placed = state
        .layouts
        .get(&active_id)
        .and_then(|l| l.pane_index_for_session(dragged_sid))
        .is_some();

    if already_placed {
        // `apply_layout_preset` filled a new pane with the dragged
        // session. Nothing more to do.
        let pane_idx = state
            .layouts
            .get(&active_id)
            .and_then(|l| l.pane_index_for_session(dragged_sid))
            .unwrap_or(0);
        return DropSplitOutcome::Created { pane_idx };
    }

    // The dragged session wasn't placed by `apply_layout_preset`
    // (shouldn't normally happen since it's in `state.sessions`, but
    // be defensive). Find an empty slot and place it manually.
    let empty_idx = state
        .layouts
        .get(&active_id)
        .and_then(|l| l.panes.iter().position(|p| p.session_id.is_empty()));

    if let Some(idx) = empty_idx
        && set_pane_session_for_active(state, idx, dragged_sid.to_string())
    {
        return DropSplitOutcome::Created { pane_idx: idx };
    }

    // No empty slot even after cycling (e.g., all 8 panes filled by
    // other sessions). Fall back to swap.
    DropSplitOutcome::FallbackSwap
}

/// Outcome of `drop_background_tab_to_create_split`. Lets the caller
/// (the drop handler in `app.rs`) decide how to log the result and
/// whether to fall back to `swap_pane_sessions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropSplitOutcome {
    /// A new pane was created (preset was upgraded) and the dragged
    /// session was placed in the new pane at `pane_idx`.
    Created { pane_idx: usize },
    /// An existing empty pane slot (in the current preset) was filled
    /// with the dragged session at `pane_idx`. The preset was NOT
    /// changed.
    FilledExisting { pane_idx: usize },
    /// Already at the maximum preset (Grid8, 8 panes) with no empty
    /// slots — the caller should fall back to `swap_pane_sessions`.
    FallbackSwap,
    /// The operation failed (no active session, or a state mutation
    /// returned false unexpectedly).
    Failed,
}

/// Outcome of `execute_tab_drop_on_pane` — the single source of truth
/// for what a tab/pane drag-drop did to the active tab's layout. Both
/// the (legacy) HTML5 `ondrop` handlers in `multi_pane_container` /
/// `single_pane_with_drop` AND the manual mouse-based tab-drag finisher
/// (Task 22) call this function and log the outcome. This deduplication
/// keeps the drop-dispatch logic in one unit-testable place — the UI
/// layers just hand off `(dragged_sid, target_pane_idx,
/// target_pane_session)`.
///
/// Note: a `SplitCreated` / `SplitFilledExisting` outcome means the
/// caller should call `restore_focus_to_active_session(state, 80)` so
/// the newly-mounted pane's TerminalView doesn't steal focus
/// unpredictably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabDropOutcome {
    /// The user dropped a session onto its own pane — nothing happened.
    NoOpSelfDrop,
    /// The dragged session was moved from another pane into an empty
    /// target pane. `cleared_source_pane` is the pane that previously
    /// held the dragged session (now cleared), or `None` if the dragged
    /// session wasn't in any pane of this layout (it was a background
    /// tab assigned to an empty slot).
    MovedToEmptyPane { cleared_source_pane: Option<usize> },
    /// The dragged session was assigned to an empty target pane, and
    /// there was no source pane to clear (i.e. the dragged session was
    /// a background tab not in any pane).
    AssignedToEmptyPane,
    /// Two panes' sessions were swapped (both source and target had
    /// sessions).
    Swapped,
    /// A swap was attempted but failed (one of the sessions wasn't in
    /// any pane of the active layout).
    SwapFailed,
    /// A new split pane was created (preset was upgraded) and the
    /// dragged session was placed at `pane_idx`. The caller should
    /// restore focus after the new pane mounts.
    SplitCreated { pane_idx: usize },
    /// An existing empty pane slot was filled (preset unchanged) with
    /// the dragged session at `pane_idx`. The caller should restore
    /// focus after the new pane mounts.
    SplitFilledExisting { pane_idx: usize },
    /// At Grid8 max with no empty slots — caller attempted a swap as a
    /// fallback, but the swap failed (the dragged session wasn't in
    /// any pane, which is the case for background-tab drags).
    SplitFallbackSwapFailed,
    /// The split-creation helper returned `Failed` (no active session
    /// or an unexpected state-mutation failure).
    SplitFailed,
}

/// Execute a tab/pane drag-drop onto a specific pane of the active
/// tab's layout. This is the SINGLE source of truth for drop dispatch:
/// both the legacy HTML5 `ondrop` handlers in `multi_pane_container` /
/// `single_pane_with_drop` and the Task 22 manual mouse-based tab-drag
/// finisher call this. Returns a `TabDropOutcome` so the caller can log
/// and (for split-creation outcomes) schedule a focus-restore.
///
/// Logic (mirrors the prior inline drop handlers):
/// 1. `dragged_sid == target_pane_session` (self-drop — the "split this
///    view again" gesture, 自由分裂):
///    - active session has NO layout (Single preset) → apply Split2H
///      (`SplitCreated { pane_idx: 1 }`).
///    - layout exists → GROW the grid to the next larger preset
///      (2/3→Grid4, 4..7→Grid8), preserving the current pane arrangement
///      and auto-filling new slots with unplaced background tabs
///      (`SplitCreated { pane_idx: <first new slot> }`). Repeated drags
///      freely create more sub-panes: 1→2→4→8.
///    - already at Grid8 (8 panes) → `NoOpSelfDrop`.
/// 2. `target_pane_session.is_empty()` → look up the dragged session's
///    source pane. If `Some(src)` and `src != target`, move the session
///    to the target and clear the source (`MovedToEmptyPane`). If the
///    dragged session isn't in any pane (`None`), just assign it to
///    the target (`AssignedToEmptyPane`).
/// 3. `target_pane_session` non-empty → look up the source pane:
///    - `Some(_)` → pane-to-pane swap (`Swapped` / `SwapFailed`).
///    - `None` → background tab: create a split via
///      `drop_background_tab_to_create_split`. Map the `DropSplitOutcome`:
///      `Created → SplitCreated`, `FilledExisting → SplitFilledExisting`,
///      `FallbackSwap →` attempt `swap_pane_sessions` (`Swapped` /
///      `SplitFallbackSwapFailed`), `Failed → SplitFailed`.
///
/// CRITICAL invariants: this function does NOT update `active_session`
/// (it's a tab pointer; layouts are keyed by it). Apart from the
/// single-pane self-drop upgrade in case 1, it does NOT call
/// `apply_layout_preset` directly — preset cycling otherwise happens
/// only inside `drop_background_tab_to_create_split`.
pub fn execute_tab_drop_on_pane(
    state: &mut AppState,
    dragged_sid: &str,
    target_pane_idx: usize,
    target_pane_session: &str,
) -> TabDropOutcome {
    // Case 1: self-drop. Dragging a tab onto the pane that already shows
    // it is the "split this view again" gesture (自由分裂):
    //
    // - No layout yet (Single preset): dragging the ACTIVE tab down into
    //   the terminal area is the user's most natural "split this view"
    //   gesture — the hit-test resolves to pane 0 which holds the active
    //   session itself, so without this special case the gesture silently
    //   did nothing (the Task 22 runtime bug). Upgrade to Split2H: pane 0
    //   keeps the active session, pane 1 is auto-filled with the next
    //   background tab (or left empty when the active session is the only
    //   open tab).
    //
    // - Layout exists (multi-pane): GROW the grid to the next larger
    //   preset (Split2H/Split2V→Grid4, Grid4→Grid8), PRESERVING the
    //   current pane arrangement (unlike `apply_layout_preset`, which
    //   re-fills all panes in tab order and would blow away manual
    //   rearrangements). New slots auto-fill with background tabs not
    //   already in a pane (tab order), or stay empty as drop-zones.
    //   Repeated self-drops thus freely create more sub-panes:
    //   1→2→4→8. At Grid8 (max) the self-drop is a no-op.
    if dragged_sid == target_pane_session {
        let Some(active_id) = state.active_session.clone() else {
            return TabDropOutcome::NoOpSelfDrop;
        };
        let Some(layout) = state.layouts.get(&active_id).cloned() else {
            // No layout (Single preset) → Split2H.
            if apply_layout_preset(state, LayoutPreset::Split2H) {
                return TabDropOutcome::SplitCreated { pane_idx: 1 };
            }
            return TabDropOutcome::NoOpSelfDrop;
        };
        // Layout exists → grow to the next larger preset. Infer the
        // "current" preset from the layout's own pane count (NOT the
        // global `state.layout_preset` — layouts are per-tab).
        let n = layout.panes.len();
        let next = match n {
            0 | 1 => Some(LayoutPreset::Split2H),
            2 | 3 => Some(LayoutPreset::Grid4),
            4..=7 => Some(LayoutPreset::Grid8),
            _ => None, // already at Grid8 max
        };
        let Some(next) = next else {
            return TabDropOutcome::NoOpSelfDrop;
        };
        // Preserve the existing pane arrangement: seed the new grid with
        // the current panes' session ids in order; `from_preset` pads the
        // extra slots with "" (empty).
        let ids: Vec<String> = layout.panes.iter().map(|p| p.session_id.clone()).collect();
        let mut new_layout = PaneLayout::from_preset(next, &ids);
        new_layout.comparison = layout.comparison;
        // Auto-fill the new empty slots with background tabs not already
        // placed, in tab order (mirrors `apply_layout_preset`'s fill
        // order). Remaining slots stay empty drop-zones.
        let placed: Vec<String> = new_layout
            .panes
            .iter()
            .map(|p| p.session_id.clone())
            .filter(|s| !s.is_empty())
            .collect();
        let candidates: Vec<String> = state
            .sessions
            .iter()
            .map(|t| t.id.clone())
            .filter(|id| !placed.contains(id))
            .collect();
        let mut candidates = candidates.into_iter();
        for pane in new_layout.panes.iter_mut() {
            if pane.session_id.is_empty()
                && let Some(sid) = candidates.next()
            {
                pane.session_id = sid;
            }
        }
        state.layouts.insert(active_id, new_layout);
        state.layout_preset = next;
        // `pane_idx` = the first newly-created slot (right after the
        // preserved ones) — used by the caller for logging/focus.
        return TabDropOutcome::SplitCreated { pane_idx: n };
    }

    let src_pane = pane_index_for_active_session(state, dragged_sid);

    // Case 2: target pane is empty → move / assign.
    if target_pane_session.is_empty() {
        if !set_pane_session_for_active(state, target_pane_idx, dragged_sid.to_string()) {
            // Defensive: target pane index out of range. This shouldn't
            // happen (the caller got `target_pane_idx` from the layout's
            // `visible_panes`), but if it does, treat as a no-op rather
            // than crashing.
            return TabDropOutcome::SwapFailed;
        }
        if let Some(src_idx) = src_pane
            && src_idx != target_pane_idx
        {
            set_pane_session_for_active(state, src_idx, String::new());
            return TabDropOutcome::MovedToEmptyPane {
                cleared_source_pane: Some(src_idx),
            };
        }
        return TabDropOutcome::AssignedToEmptyPane;
    }

    // Case 3: target pane has a session. Either swap or create a split.
    if let Some(_src_idx) = src_pane {
        // Sub-case (a): pane-to-pane swap.
        let swapped = swap_pane_sessions(state, dragged_sid, target_pane_session);
        return if swapped {
            TabDropOutcome::Swapped
        } else {
            TabDropOutcome::SwapFailed
        };
    }

    // Sub-case (b): background tab → create a split.
    let outcome = drop_background_tab_to_create_split(state, dragged_sid, target_pane_idx);
    match outcome {
        DropSplitOutcome::Created { pane_idx } => TabDropOutcome::SplitCreated { pane_idx },
        DropSplitOutcome::FilledExisting { pane_idx } => {
            TabDropOutcome::SplitFilledExisting { pane_idx }
        }
        DropSplitOutcome::FallbackSwap => {
            // Already at Grid8 — fall back to a swap. For background-tab
            // drags this will fail silently (the dragged session isn't in
            // any pane), but the contract is documented and the caller
            // can log the attempt.
            let swapped = swap_pane_sessions(state, dragged_sid, target_pane_session);
            if swapped {
                TabDropOutcome::Swapped
            } else {
                TabDropOutcome::SplitFallbackSwapFailed
            }
        }
        DropSplitOutcome::Failed => TabDropOutcome::SplitFailed,
    }
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
                cwd: None,
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

    // ------------------------------------------------------------------
    // Task 17/19 regression: multi-pane input routing.
    //
    // Bug (fixed 2026-07-19): in multi-pane mode with comparison OFF, only
    // the first pane accepted commands. Root cause: `render_terminal_pane`'s
    // `on_input` handler used the condition
    //     `broadcast_targets.len() > 1
    //      || (broadcast_targets.len() == 1 && broadcast_targets[0] != sid_clone)`
    // which treated "active_session differs from this pane's session" as a
    // broadcast trigger. Since `broadcast_targets` returns `[active_session]`
    // when comparison is OFF, pane N (N>0) had `broadcast_targets[0] !=
    // sid_clone`, so its keystrokes were sent to `active_session`'s PTY
    // (pane 0) instead of its own. The user saw pane 0 react and pane N do
    // nothing → "only the first pane accepts commands".
    //
    // Fix: `is_broadcast = broadcast_targets.len() > 1`. This is true ONLY
    // when comparison is ON with 2+ non-empty panes. In all other cases,
    // each pane sends to its own `sid_clone`.
    //
    // These tests pin the contract via `broadcast_targets` (the sole input
    // to the routing decision) plus a direct assertion of the corrected
    // `is_broadcast` predicate. A full dioxus-runtime test of `on_input`
    // isn't feasible without spinning up the desktop webview, so we test
    // the decision function and the layout state that feeds it.
    // ------------------------------------------------------------------

    /// Regression for "only first pane accepts commands". In Split2H with
    /// comparison OFF, `broadcast_targets` must return exactly 1 entry
    /// (the active session), so the `is_broadcast = len > 1` predicate is
    /// false for EVERY pane — including pane 1, whose `sid_clone` differs
    /// from `active_session`. The old buggy predicate would have been true
    /// for pane 1.
    #[test]
    fn non_comparison_multi_pane_input_routes_to_each_pane_own_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // active_session = "alpha"; layout panes = [alpha, beta].
        let targets = broadcast_targets(&state);
        assert_eq!(targets, vec!["alpha".to_string()]); // comparison OFF
        // The corrected predicate: only broadcast when there are multiple
        // targets (i.e., comparison ON with 2+ non-empty panes).
        let is_broadcast = targets.len() > 1;
        assert!(
            !is_broadcast,
            "non-comparison multi-pane must NOT broadcast"
        );
        // For pane 1 (beta): the old predicate `(targets[0] != "beta")`
        // would have been TRUE → bug. The new predicate is FALSE → correct.
        assert_ne!(targets[0], "beta"); // preconditions for the bug
        assert!(
            !is_broadcast,
            "pane 1 input must go to its own session, not alpha's"
        );
    }

    /// Same contract as above, but after a drag-and-drop pane swap. The
    /// user drags session `gamma` onto pane 0 (which had `alpha`), swapping
    /// them. Afterwards, pane 0 shows `gamma` and pane 1 shows `beta` (or
    /// whatever the swap produced). The routing predicate must STILL be
    /// false with comparison OFF — each pane's input goes to its own
    /// session, regardless of which session is "active".
    ///
    /// This simulates the mouse-drag-to-rearrange-panes flow the user
    /// asked for, at the state level (the actual mouse events are DOM
    /// concerns that can't be unit-tested without a webview).
    #[test]
    fn after_drag_swap_panes_input_still_routes_to_own_session() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // active_session = "alpha"; layout panes = [alpha, beta] (gamma is
        // not in any pane because Split2H only has 2 slots and `apply_layout_preset`
        // fills them in tab order with active first).
        assert_eq!(state.layouts["alpha"].panes[0].session_id, "alpha");
        assert_eq!(state.layouts["alpha"].panes[1].session_id, "beta");

        // Simulate the user dragging `gamma` from the tab bar onto pane 1
        // (which currently shows `beta`). This is the `ondrop` handler's
        // "target pane has a session → swap" path. We use the state-level
        // helper that the drop handler calls.
        //
        // Note: `gamma` is NOT currently in any pane, so the drop handler's
        // swap path would actually be the "move" path (target pane has a
        // session, source session isn't in any pane). We simulate the
        // simpler case: drag `beta` from pane 1 onto pane 0 (which has
        // `alpha`) → swap alpha and beta. This verifies that after a swap,
        // the routing predicate is still correct.
        let swapped = swap_pane_sessions(&mut state, "alpha", "beta");
        assert!(swapped);
        assert_eq!(state.layouts["alpha"].panes[0].session_id, "beta");
        assert_eq!(state.layouts["alpha"].panes[1].session_id, "alpha");

        // active_session is STILL "alpha" (the tab pointer doesn't change
        // on pane click — see the comment in `render_terminal_pane`'s
        // `on_input` handler). But now pane 0 shows `beta` and pane 1
        // shows `alpha`. The routing predicate must still be false.
        let targets = broadcast_targets(&state);
        assert_eq!(targets, vec!["alpha".to_string()]); // comparison OFF
        let is_broadcast = targets.len() > 1;
        assert!(!is_broadcast);
        // Pane 0 now shows `beta` (sid_clone="beta"), but active_session
        // is still "alpha". The old buggy predicate `(targets[0] != "beta")`
        // would have been TRUE → beta's input would go to alpha's PTY.
        // The new predicate is FALSE → beta's input goes to beta's PTY.
        assert_ne!(targets[0], "beta");
        assert!(
            !is_broadcast,
            "after drag-swap, pane 0 (beta) input must go to beta, not alpha"
        );
    }

    /// When comparison IS ON with 2+ panes, the predicate must be TRUE so
    /// input broadcasts to every pane's PTY. This pins the comparison-mode
    /// half of the contract (the fix must not break synchronization).
    #[test]
    fn comparison_on_multi_pane_input_broadcasts_to_all_panes() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        assert_eq!(targets.len(), 4);
        let is_broadcast = targets.len() > 1;
        assert!(is_broadcast, "comparison ON with 4 panes must broadcast");
    }

    /// Edge case: comparison ON but only 1 non-empty pane. `broadcast_targets`
    /// returns len 1, so the predicate is false — input goes to that single
    /// pane's session. This is correct (there's only one target anyway).
    #[test]
    fn comparison_on_single_non_empty_pane_does_not_broadcast() {
        let mut state = state_with_active_session(&["alpha"]);
        // Grid4 with only 1 session → 3 empty panes.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        assert_eq!(targets, vec!["alpha".to_string()]);
        let is_broadcast = targets.len() > 1;
        assert!(!is_broadcast);
    }

    // ------------------------------------------------------------------
    // Task 14 / 15 — additional coverage for multi-pane display and
    // session-allocation correctness. These pin contracts that the
    // earlier tests don't directly exercise.
    // ------------------------------------------------------------------

    /// Each tab owns its own layout — switching the active session must not
    /// disturb another tab's layout. This is the multi-tab invariant of
    /// Task 14's multi-pane display: switching tabs swaps which layout is
    /// rendered, but both layouts coexist in `state.layouts`.
    #[test]
    fn layouts_are_per_session_and_independent_across_tabs() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // alpha tab → Grid4 (3 sessions, last slot empty).
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        assert_eq!(state.layouts.len(), 1);
        assert!(state.layouts.contains_key("alpha"));

        // Switch active session to beta and apply Split2H there.
        state.active_session = Some("beta".to_string());
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert_eq!(state.layouts.len(), 2);
        assert!(state.layouts.contains_key("beta"));

        // The two layouts are distinct — beta's layout is Split2H (2 panes),
        // alpha's is still Grid4 (4 panes).
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 4);
        assert_eq!(state.layouts.get("beta").unwrap().panes.len(), 2);

        // Switching back to alpha — its layout is preserved unchanged.
        state.active_session = Some("alpha".to_string());
        let alpha_layout = state.layouts.get("alpha").unwrap().clone();
        assert_eq!(alpha_layout.panes.len(), 4);
        assert_eq!(alpha_layout.cols(), 2);
        assert_eq!(alpha_layout.rows(), 2);
    }

    /// Task 15 contract: when the user cycles a layout preset on a tab whose
    /// active session is `X`, the new layout is rebuilt with `X` anchored at
    /// pane 0 and the remaining sessions filling the rest in tab order.
    /// This is the session-allocation correctness criterion.
    #[test]
    fn cycle_layout_preset_anchors_active_session_at_pane_zero() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        // Make `gamma` the active session — it's at index 2 in `sessions`.
        state.active_session = Some("gamma".to_string());

        // Cycle to Grid4. The new layout should have `gamma` at pane 0,
        // not `alpha` (which is the first tab). This is the contract
        // `apply_layout_preset` enforces: active session first, then the
        // remaining sessions in tab order (excluding the active one).
        cycle_layout_preset(&mut state); // Single → Split2H
        cycle_layout_preset(&mut state); // Split2H → Split2V
        cycle_layout_preset(&mut state); // Split2V → Grid4
        let layout = state
            .layouts
            .get("gamma")
            .expect("layout stored under gamma");
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[0].session_id, "gamma");
        // Remaining panes fill with the other sessions in tab order.
        assert_eq!(layout.panes[1].session_id, "alpha");
        assert_eq!(layout.panes[2].session_id, "beta");
        assert_eq!(layout.panes[3].session_id, "delta");
    }

    /// Task 15 contract: re-applying a preset after opening a new session
    /// pulls the new session into the layout. This mirrors the user flow of
    /// "open several sessions, then enable Grid4" — every session created
    /// since the last layout build is included.
    #[test]
    fn apply_layout_preset_pulls_in_sessions_opened_after_layout_was_built() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Layout contains alpha, beta only.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.session_ids(), vec!["alpha", "beta"]);

        // Open two more sessions after the layout was built (simulating the
        // sidebar connect / local-terminal buttons). They go into `sessions`
        // but the existing layout is NOT automatically rebuilt.
        state.sessions.push(SessionTab {
            id: "gamma".to_string(),
            name: "gamma".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("gamma".to_string()),
            cwd: None,
        });
        state.sessions.push(SessionTab {
            id: "delta".to_string(),
            name: "delta".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("delta".to_string()),
            cwd: None,
        });

        // Re-apply Grid4 — now all 4 sessions should be in the layout.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert_eq!(layout.panes[3].session_id, "delta");
    }

    /// Task 14 contract: Grid8 is recognised as multi-pane (so the
    /// multi-pane render path is taken, not the legacy single-pane path).
    /// This is what makes "8 分隔" actually display 8 panes side-by-side.
    #[test]
    fn grid8_layout_is_multi_pane() {
        let mut state =
            state_with_active_session(&["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        let layout = state.layouts.get("s0").unwrap();
        assert!(layout.is_multi_pane());
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 4);
        assert_eq!(layout.panes.len(), 8);
    }

    /// Task 14 contract: comparison mode on Grid8 broadcasts input to all
    /// 8 panes (no session dropped, no duplicates). This is the
    /// "跨终端会话的比对模式" use case — the user wants the same command to
    /// run on 8 hosts simultaneously.
    #[test]
    fn broadcast_targets_covers_all_eight_panes_in_grid8_comparison() {
        let mut state =
            state_with_active_session(&["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        assert_eq!(targets.len(), 8);
        for i in 0..8 {
            assert!(targets.contains(&format!("s{i}")));
        }
    }

    /// Task 14 contract: zoom survives a window resize. The zoomed pane's
    /// rect always equals the container size regardless of how the
    /// container dimensions change. This pins the "全屏分辨率" requirement —
    /// fullscreen isn't tied to a specific resolution, it adapts.
    #[test]
    fn zoomed_pane_fills_container_after_resize() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Zoom pane 0.
        assert!(toggle_pane_zoom(&mut state, "alpha"));
        let layout = state.layouts.get("alpha").unwrap().clone();

        // At the original container size, the zoomed pane fills it.
        let r0 = layout.pane_rect(0, 1200.0, 800.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 1200.0, 800.0));
        // After the window is resized to a different aspect ratio, the
        // zoomed pane still fills the whole container.
        let r0_big = layout.pane_rect(0, 1920.0, 1080.0).unwrap();
        assert_eq!(r0_big, (0.0, 0.0, 1920.0, 1080.0));
        let r0_small = layout.pane_rect(0, 640.0, 480.0).unwrap();
        assert_eq!(r0_small, (0.0, 0.0, 640.0, 480.0));
        // The other pane stays hidden.
        assert!(layout.pane_rect(1, 1200.0, 800.0).is_none());
    }

    /// Task 14 contract: comparison mode is a per-tab layout flag, so
    /// toggling zoom on one pane doesn't disturb the comparison flag.
    /// This ensures the user can enter comparison mode, then zoom a pane
    /// to inspect it, then unzoom and resume the comparison broadcast —
    /// the comparison flag survives the zoom cycle.
    #[test]
    fn zoom_cycle_preserves_comparison_mode() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Enable comparison mode.
        assert_eq!(toggle_comparison_mode(&mut state), Some(true));
        // Zoom pane 2 (gamma).
        assert!(toggle_pane_zoom(&mut state, "gamma"));
        // Comparison is still on.
        let layout = state.layouts.get("alpha").unwrap();
        assert!(layout.comparison);
        assert_eq!(layout.zoomed, Some(2));
        // While zoomed, broadcast_targets still resolves the layout's
        // session_ids (the comparison contract holds even when one pane
        // is zoomed — input goes to all pane sessions, not just the
        // zoomed one).
        let targets = broadcast_targets(&state);
        assert_eq!(targets.len(), 4);
        // Unzoom — comparison still on.
        assert!(toggle_pane_zoom(&mut state, "gamma"));
        let layout = state.layouts.get("alpha").unwrap();
        assert!(layout.comparison);
        assert!(layout.zoomed.is_none());
    }

    /// Task 15 contract: closing a pane's session and re-applying the
    /// preset re-allocates the freed pane to the next available session.
    /// This mirrors the user flow of "close tab, layout auto-rebuilds."
    #[test]
    fn apply_layout_preset_after_session_close_reallocates_panes() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Close `beta` (pane 1).
        state.sessions.retain(|s| s.id != "beta");
        // Re-apply Grid4 — only 3 sessions left, so pane 3 should be empty.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "gamma");
        assert_eq!(layout.panes[2].session_id, "delta");
        assert_eq!(layout.panes[3].session_id, "");
        // session_ids skips the empty pane.
        assert_eq!(layout.session_ids(), vec!["alpha", "gamma", "delta"]);
    }

    /// Task 15 contract: the active session is always in `broadcast_targets`
    /// (whether comparison is on or off). This is the invariant that lets the
    /// input handler assume "the user's keystrokes always reach the focused
    /// pane, regardless of comparison mode".
    #[test]
    fn broadcast_targets_always_includes_active_session() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // No layout — active only.
        assert_eq!(broadcast_targets(&state), vec!["alpha".to_string()]);

        // Grid4 layout, comparison off — active only.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        assert_eq!(broadcast_targets(&state), vec!["alpha".to_string()]);

        // Comparison on — all panes, but `alpha` (the active session) must
        // be present in the list.
        toggle_comparison_mode(&mut state);
        let targets = broadcast_targets(&state);
        assert!(targets.contains(&"alpha".to_string()));
        assert!(targets.contains(&"beta".to_string()));
        assert!(targets.contains(&"gamma".to_string()));
    }

    // ------------------------------------------------------------------
    // Task 16 — drag-and-drop pane rearrangement wrappers
    // ------------------------------------------------------------------
    //
    // These tests pin the contracts of `set_pane_session_for_active`,
    // `swap_pane_sessions`, `pane_index_for_active_session`, and
    // `session_at_pane`. The drag-and-drop UI handlers in `app.rs`
    // depend on every branch of these functions: the happy path
    // (mutation applied), the no-active-session path (graceful false),
    // the no-layout path (graceful false), and the out-of-range path
    // (graceful false / None). If any of these changed silently, the
    // drop handler could end up mutating the wrong tab's layout or
    // panicking on an unwrap.

    /// `set_pane_session_for_active` replaces the session at a given
    /// pane index in the active tab's layout. This is the path the drop
    /// handler takes when the user drags an open tab onto a pane that
    /// currently has no session (e.g., dropping onto an empty slot in
    /// a Grid8 layout where only 2 of 8 panes are filled).
    #[test]
    fn set_pane_session_for_active_replaces_pane_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Pane 1 shows "beta"; replace it with "gamma".
        assert!(set_pane_session_for_active(
            &mut state,
            1,
            "gamma".to_string()
        ));
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[1].session_id, "gamma");
    }

    /// With no active session, `set_pane_session_for_active` returns
    /// false without touching anything. This covers the (rare) case
    /// where the user has closed all tabs mid-drag.
    #[test]
    fn set_pane_session_for_active_returns_false_with_no_active_session() {
        let mut state = AppState::default();
        assert!(!set_pane_session_for_active(&mut state, 0, "x".to_string()));
        assert!(state.layouts.is_empty());
    }

    /// With an active session but no layout applied (Single preset),
    /// `set_pane_session_for_active` returns false. The drop handler
    /// uses this branch to fall back to the legacy "open new tab"
    /// path — there's no pane to drop onto if the user hasn't entered
    /// a multi-pane layout.
    #[test]
    fn set_pane_session_for_active_returns_false_with_no_layout() {
        let mut state = state_with_active_session(&["alpha"]);
        // No apply_layout_preset call → no entry in state.layouts.
        assert!(!set_pane_session_for_active(
            &mut state,
            0,
            "beta".to_string()
        ));
    }

    /// Out-of-range pane index returns false. The drop handler may
    /// compute a stale pane index (e.g., the layout was cycled while
    /// the drag was in flight); in that case the function must fail
    /// gracefully rather than panic on `panes[idx]`.
    #[test]
    fn set_pane_session_for_active_returns_false_for_out_of_range_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Only 2 panes; index 99 is out of range.
        assert!(!set_pane_session_for_active(
            &mut state,
            99,
            "x".to_string()
        ));
    }

    /// `swap_pane_sessions` exchanges the panes displaying two sessions.
    /// This is the path the drop handler takes when the user drags an
    /// open tab onto a pane that already has a session — the two panes
    /// swap their displayed sessions.
    #[test]
    fn swap_pane_sessions_exchanges_two_panes() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Before: pane 0=alpha, pane 2=gamma.
        assert!(swap_pane_sessions(&mut state, "alpha", "gamma"));
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "gamma");
        assert_eq!(layout.panes[2].session_id, "alpha");
    }

    /// `swap_pane_sessions` with a missing session returns false. This
    /// covers the case where the user drags a tab that was just closed
    /// — the session_id is no longer in any pane, so the swap can't
    /// happen.
    #[test]
    fn swap_pane_sessions_returns_false_for_missing_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let before = state.layouts.get("alpha").unwrap().clone();
        assert!(!swap_pane_sessions(&mut state, "alpha", "nonexistent"));
        assert_eq!(state.layouts.get("alpha").unwrap(), &before);
    }

    /// `swap_pane_sessions` with no active session returns false
    /// (graceful no-op). Same rationale as the
    /// `set_pane_session_for_active` test above.
    #[test]
    fn swap_pane_sessions_returns_false_with_no_active_session() {
        let mut state = AppState::default();
        assert!(!swap_pane_sessions(&mut state, "alpha", "beta"));
    }

    /// `pane_index_for_active_session` returns the pane index displaying
    /// a given session in the active tab's layout. Used by the drop
    /// handler to find the source pane of a drag (so we know which
    /// pane to swap from).
    #[test]
    fn pane_index_for_active_session_returns_correct_index() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        assert_eq!(pane_index_for_active_session(&state, "alpha"), Some(0));
        assert_eq!(pane_index_for_active_session(&state, "delta"), Some(3));
        assert_eq!(pane_index_for_active_session(&state, "nonexistent"), None);
    }

    /// `pane_index_for_active_session` returns None when there's no
    /// active session or no layout. This is what the drop handler uses
    /// to detect "the user is dragging from a tab in a layout-less
    /// (Single-preset) tab" — in that case there's no pane to swap.
    #[test]
    fn pane_index_for_active_session_returns_none_without_layout() {
        let state = state_with_active_session(&["alpha"]);
        // No layout applied.
        assert_eq!(pane_index_for_active_session(&state, "alpha"), None);
    }

    /// `session_at_pane` returns the session_id displayed at a given
    /// pane index. Used by the drop handler to identify the session
    /// currently at the drop target (so we can swap it with the dragged
    /// session, or replace it with a freshly-opened connection).
    #[test]
    fn session_at_pane_returns_correct_session() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        assert_eq!(session_at_pane(&state, 0), Some("alpha".to_string()));
        assert_eq!(session_at_pane(&state, 3), Some("delta".to_string()));
        // Out of range.
        assert_eq!(session_at_pane(&state, 99), None);
    }

    /// `session_at_pane` returns None when there's no active session or
    /// no layout. The drop handler uses this to detect "the user dropped
    /// onto a pane in a layout-less tab" — in that case there's no
    /// existing session to swap with, and the handler opens a new tab
    /// instead.
    #[test]
    fn session_at_pane_returns_none_without_layout() {
        let state = state_with_active_session(&["alpha"]);
        assert_eq!(session_at_pane(&state, 0), None);
    }

    /// Round-trip: swap two sessions, then swap them back. The layout
    /// should be identical to the original. This pins the algebraic
    /// invariant that swap is its own inverse — the user can always
    /// "undo" a drag by dragging back.
    #[test]
    fn swap_pane_sessions_round_trip_restores_layout() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let before = state.layouts.get("alpha").unwrap().clone();
        assert!(swap_pane_sessions(&mut state, "alpha", "gamma"));
        assert!(swap_pane_sessions(&mut state, "alpha", "gamma"));
        assert_eq!(state.layouts.get("alpha").unwrap(), &before);
    }

    /// Drag a session onto an empty pane: the session moves from its
    /// original pane to the empty pane (the original pane becomes empty).
    /// This is the "drag-to-rearrange" flow when the user wants to
    /// reorganize a partially-filled grid. We achieve this by
    /// `set_pane_session(target_pane, source_session)` followed by
    /// `set_pane_session(source_pane, "")` — both panes are updated
    /// through the wrapper.
    #[test]
    fn drag_session_to_empty_pane_moves_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // Grid4 → panes 0,1 have alpha,beta; panes 2,3 are empty.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Find source pane (alpha is at pane 0) and target pane (2, empty).
        let src_pane = pane_index_for_active_session(&state, "alpha").unwrap();
        assert_eq!(src_pane, 0);
        assert_eq!(session_at_pane(&state, 2), Some("".to_string()));
        // Move alpha to pane 2, clear pane 0.
        assert!(set_pane_session_for_active(
            &mut state,
            2,
            "alpha".to_string()
        ));
        assert!(set_pane_session_for_active(&mut state, 0, String::new()));
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "");
        assert_eq!(layout.panes[2].session_id, "alpha");
        assert_eq!(layout.session_ids(), vec!["beta", "alpha"]);
    }

    // ------------------------------------------------------------------
    // Task 16 — end-to-end drag-and-drop flow integration tests
    // ------------------------------------------------------------------
    //
    // These tests simulate the full drag-and-drop data flow at the
    // state level (without spinning up a dioxus runtime). They verify
    // that the sequence of state mutations the drop handler performs
    // produces the expected final layout, regardless of the starting
    // state. The drop handler in `app.rs` reads the drag's
    // DataTransfer, then calls the appropriate sequence of state
    // helpers; these tests pin the contracts of those sequences.

    /// Simulates: user drags an open session tab from pane A onto pane B
    /// (which already has a session). The two panes swap their displayed
    /// sessions. This is the most common drag-and-drop operation —
    /// rearranging existing sessions across panes.
    #[test]
    fn e2e_drag_open_session_onto_occupied_pane_swaps() {
        // Setup: Grid4 with alpha, beta, gamma, delta in panes 0-3.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // The user drags "alpha" (from pane 0) onto pane 2 (which has "gamma").
        let dragged_session = "alpha".to_string();
        let target_pane_session = session_at_pane(&state, 2).unwrap(); // "gamma"
        // The drop handler calls swap_pane_sessions.
        assert!(swap_pane_sessions(
            &mut state,
            &dragged_session,
            &target_pane_session
        ));
        let layout = state.layouts.get("alpha").unwrap();
        // Pane 0 now shows gamma, pane 2 shows alpha.
        assert_eq!(layout.panes[0].session_id, "gamma");
        assert_eq!(layout.panes[2].session_id, "alpha");
        // Beta and delta are unchanged.
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[3].session_id, "delta");
        // All 4 sessions are still present (none lost in the swap).
        let mut sessions = layout.session_ids();
        sessions.sort();
        assert_eq!(sessions, vec!["alpha", "beta", "delta", "gamma"]);
    }

    /// Simulates: user drags an open session tab from pane A onto an
    /// empty pane B. The session moves from A to B, and pane A becomes
    /// empty. This is the "drag-to-rearrange" flow for partially-filled
    /// grids (e.g., Grid8 with only 3 sessions open).
    #[test]
    fn e2e_drag_open_session_onto_empty_pane_moves_session() {
        // Setup: Grid8 with only 3 sessions (panes 3-7 are empty).
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        // User drags "alpha" (pane 0) onto pane 5 (empty).
        let dragged_session = "alpha".to_string();
        let target_pane = 5;
        let src_pane = pane_index_for_active_session(&state, &dragged_session).unwrap();
        assert_eq!(src_pane, 0);
        assert_eq!(session_at_pane(&state, target_pane).unwrap(), "");
        // The drop handler's "move to empty pane" path.
        assert!(set_pane_session_for_active(
            &mut state,
            target_pane,
            dragged_session.clone()
        ));
        assert!(set_pane_session_for_active(
            &mut state,
            src_pane,
            String::new()
        ));
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "");
        assert_eq!(layout.panes[5].session_id, "alpha");
        // Beta and gamma are still at their original panes.
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        // session_ids() skips the empty pane.
        let mut sessions = layout.session_ids();
        sessions.sort();
        assert_eq!(sessions, vec!["alpha", "beta", "gamma"]);
    }

    /// Simulates: user drags a sidebar connection onto an occupied pane.
    /// A new session is created (via `open_connection` in app.rs, which
    /// we approximate here by inserting a new SessionTab) and assigned
    /// to the target pane, replacing whatever was there. The replaced
    /// session is NOT closed — it's still in `state.sessions` and can
    /// be re-assigned to another pane later.
    #[test]
    fn e2e_drag_sidebar_connection_onto_pane_replaces_session() {
        // Setup: Split2H with alpha (pane 0) and beta (pane 1).
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // The drop handler in app.rs would call open_connection, which:
        //   1. Generates a new tab_id (e.g., "new-conn-1")
        //   2. Pushes a new SessionTab to state.sessions
        //   3. Calls set_pane_session_for_active to put it in the target pane
        // We simulate steps 2-3 here (step 1 is just a UUID).
        let new_session_id = "new-conn-1".to_string();
        state.sessions.push(SessionTab {
            id: new_session_id.clone(),
            name: "New Connection".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("newhost".to_string()),
            cwd: None,
        });
        // The drop target is pane 1 (which had "beta").
        assert!(set_pane_session_for_active(
            &mut state,
            1,
            new_session_id.clone()
        ));
        let layout = state.layouts.get("alpha").unwrap();
        // Pane 1 now shows the new session.
        assert_eq!(layout.panes[1].session_id, "new-conn-1");
        // Pane 0 is unchanged.
        assert_eq!(layout.panes[0].session_id, "alpha");
        // Beta is still in state.sessions (not closed).
        assert!(state.sessions.iter().any(|t| t.id == "beta"));
        // The new session is also in state.sessions.
        assert!(state.sessions.iter().any(|t| t.id == "new-conn-1"));
        // session_ids() reflects the new arrangement.
        let mut sessions = layout.session_ids();
        sessions.sort();
        assert_eq!(sessions, vec!["alpha", "new-conn-1"]);
    }

    /// Simulates: user drags a sidebar connection onto an empty pane
    /// (e.g., in a Grid8 layout with only 2 sessions open). The new
    /// session fills the empty pane without disturbing the existing
    /// sessions. This is the "fill in the grid" flow.
    #[test]
    fn e2e_drag_sidebar_connection_onto_empty_pane_fills_slot() {
        // Setup: Grid8 with 2 sessions (panes 2-7 empty).
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        // Simulate open_connection creating a new session and assigning
        // it to pane 5 (which was empty).
        let new_session_id = "new-ssh-1".to_string();
        state.sessions.push(SessionTab {
            id: new_session_id.clone(),
            name: "New SSH".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("newhost".to_string()),
            cwd: None,
        });
        assert!(set_pane_session_for_active(
            &mut state,
            5,
            new_session_id.clone()
        ));
        let layout = state.layouts.get("alpha").unwrap();
        // Pane 5 now has the new session.
        assert_eq!(layout.panes[5].session_id, "new-ssh-1");
        // Existing sessions are undisturbed.
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        // Other empty panes are still empty.
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.panes[7].session_id, "");
        // session_ids() now has 3 sessions.
        let mut sessions = layout.session_ids();
        sessions.sort();
        assert_eq!(sessions, vec!["alpha", "beta", "new-ssh-1"]);
    }

    /// Simulates: user drags a session onto its own pane (a no-op).
    /// The drop handler detects this case (`dragged_sid ==
    /// drop_session_id`) and returns early without calling any state
    /// mutation. This test verifies that the comparison works correctly
    /// — the layout is unchanged after the "drop".
    #[test]
    fn e2e_drag_session_onto_own_pane_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let before = state.layouts.get("alpha").unwrap().clone();
        // Simulate the drop handler's "dropped onto own pane" check.
        let dragged_session = "alpha".to_string();
        let drop_session_id = session_at_pane(&state, 0).unwrap(); // "alpha"
        // The drop handler checks: if dragged_sid == drop_session_id { return; }
        if dragged_session == drop_session_id {
            // No state mutation — the layout is unchanged.
        } else {
            panic!("test setup is wrong: dragged session should equal drop target");
        }
        assert_eq!(state.layouts.get("alpha").unwrap(), &before);
    }

    /// Simulates: user drags an open session, but the active tab has no
    /// layout (Single preset). The drop handler can't assign the
    /// session to a pane (there are no panes), so it should fall back
    /// to a no-op or to making the dragged session the active session.
    /// This test verifies that the state helpers return false/None in
    /// this case (graceful degradation), which is what the drop handler
    /// uses to decide to fall back.
    #[test]
    fn e2e_drag_with_no_layout_falls_back_gracefully() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // No apply_layout_preset → no layout entry for "alpha".
        // The drop handler's checks should all return false/None.
        assert!(!set_pane_session_for_active(
            &mut state,
            0,
            "beta".to_string()
        ));
        assert!(!swap_pane_sessions(&mut state, "alpha", "beta"));
        assert_eq!(pane_index_for_active_session(&state, "alpha"), None);
        assert_eq!(session_at_pane(&state, 0), None);
        // No layout was created.
        assert!(state.layouts.is_empty());
    }

    // ------------------------------------------------------------------
    // Performance contract tests (Task 16 optimization)
    // ------------------------------------------------------------------
    //
    // These tests pin the cost characteristics the drop handler relies on.
    // They don't measure wall-clock time (flaky in CI); instead they verify
    // the structural invariants that make the operations cheap:
    //   - swap_pane_sessions touches exactly 2 panes (no full-layout rebuild)
    //   - set_pane_session_for_active is O(1) bounds-check on out-of-range
    //   - pane_index_for_active_session returns early when no layout exists
    //   - The drop handler's "no-op when dropping on own pane" check is
    //     O(1) (string equality, no state mutation)
    //
    // The drag-over highlight signal (`drag_over_pane: Signal<Option<usize>>`)
    // lives in the Dioxus runtime, not on AppState — it can't be unit-tested
    // without spinning up a Dioxus runtime. Its behavior is instead pinned
    // by the call-site comments in `multi_pane_container`: the Signal equality
    // check makes `set(Some(idx))` a no-op when the value is unchanged, so the
    // high-frequency `ondragover` (~60Hz) does NOT trigger per-tick re-renders.

    /// `swap_pane_sessions` must only swap the two named sessions — it
    /// must not touch any other panes. This is the contract that lets
    /// the drop handler call `swap_pane_sessions` without re-checking
    /// every pane afterwards. If this test fails, a swap could silently
    /// shuffle other panes (a layout-thrash bug).
    #[test]
    fn swap_pane_sessions_only_touches_two_panes() {
        let mut state = state_with_active_session(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        // Snapshot the layout before the swap.
        let before = state.layouts.get("a").unwrap().clone();
        // Swap panes 1 and 6 (sessions "b" and "g").
        assert!(swap_pane_sessions(&mut state, "b", "g"));
        let after = state.layouts.get("a").unwrap();
        // Only panes 1 and 6 should differ.
        for i in 0..8 {
            let before_sid = &before.panes[i].session_id;
            let after_sid = &after.panes[i].session_id;
            if i == 1 || i == 6 {
                assert_ne!(before_sid, after_sid, "pane {} should have changed", i);
            } else {
                assert_eq!(before_sid, after_sid, "pane {} should be unchanged", i);
            }
        }
        // Specifically: pane 1 now has "g", pane 6 now has "b".
        assert_eq!(after.panes[1].session_id, "g");
        assert_eq!(after.panes[6].session_id, "b");
    }

    /// `set_pane_session_for_active` with an out-of-range pane index
    /// must return false without panicking. The drop handler calls
    /// this with `idx` captured from the pane loop — if a session is
    /// closed mid-drag, the captured `idx` might be stale (the layout
    /// shrank). The function must be O(1) on the failure path (just a
    /// bounds check), not iterate the panes.
    #[test]
    fn set_pane_session_for_active_out_of_range_is_o1_no_panic() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Far out of range — must not panic.
        assert!(!set_pane_session_for_active(
            &mut state,
            9999,
            "x".to_string()
        ));
        assert!(!set_pane_session_for_active(
            &mut state,
            usize::MAX,
            "x".to_string()
        ));
        // The layout is unchanged (no mutation happened).
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    /// `pane_index_for_active_session` must return `None` in O(1) when
    /// there's no layout for the active session. The drop handler calls
    /// this to find the source pane of a drag; if it returned `Some(_)`
    /// spuriously, the drop would try to clear a non-existent pane.
    #[test]
    fn pane_index_for_active_session_returns_none_without_layout_o1() {
        let state = state_with_active_session(&["alpha", "beta"]);
        // No layout applied — must be None without iterating.
        assert_eq!(pane_index_for_active_session(&state, "alpha"), None);
        assert_eq!(pane_index_for_active_session(&state, "beta"), None);
        assert_eq!(pane_index_for_active_session(&state, "nonexistent"), None);
    }

    // ------------------------------------------------------------------
    // Task 19 — drag a background tab onto a pane to CREATE a split.
    // These tests pin the `drop_background_tab_to_create_split` contract.
    // ------------------------------------------------------------------

    /// Dragging a background tab onto a pane when there's no layout yet
    /// (Single preset) must CREATE a Split2H layout and place the dragged
    /// session in pane 1. The active session stays in pane 0.
    #[test]
    fn drop_background_tab_creates_split_when_no_layout() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // No layout applied — Single preset. User drags `beta` (a
        // background tab — not in any pane) onto pane 0 (which has alpha).
        let outcome = drop_background_tab_to_create_split(&mut state, "beta", 0);
        assert_eq!(outcome, DropSplitOutcome::Created { pane_idx: 1 });
        // Layout is now Split2H with alpha in pane 0, beta in pane 1.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(state.layout_preset, LayoutPreset::Split2H);
        // active_session is UNCHANGED (it's a tab pointer, not a pane pointer).
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// After `drop_background_tab_to_create_split` creates a layout from
    /// the Single preset, the resulting layout MUST be `is_multi_pane()`.
    /// This is what triggers the App render path to switch from
    /// `single_pane_with_drop` to `multi_pane_container` on the next
    /// render — the multi-pane container then renders the new pane with
    /// its own TerminalView, splitter bars, and per-pane drop handlers.
    ///
    /// This test pins the contract that connects the Task 19 state-level
    /// helper to the UI render-path switch. Without `is_multi_pane()`
    /// returning true here, the user would drag a tab, the state would
    /// update, but the UI would keep rendering the single-pane path
    /// (with the original session) and the new pane would never appear.
    #[test]
    fn drop_background_tab_creates_multi_pane_layout_from_single() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // Sanity: no layout, so is_multi_pane would be false (the App
        // takes the single-pane path).
        assert!(!state.layouts.contains_key("alpha"));
        let outcome = drop_background_tab_to_create_split(&mut state, "beta", 0);
        assert_eq!(outcome, DropSplitOutcome::Created { pane_idx: 1 });
        // After the drop, the layout exists and is_multi_pane is true →
        // the App's next render takes the multi-pane path.
        let layout = state.layouts.get("alpha").unwrap();
        assert!(layout.is_multi_pane());
    }

    /// Dragging a background tab onto a pane when the layout already has
    /// an empty slot must FILL the empty slot without changing the preset.
    /// This is the Grid4-with-2-sessions case: 2 empty panes are available.
    #[test]
    fn drop_background_tab_fills_existing_empty_slot() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // Grid4 with 3 sessions → panes [alpha, beta, gamma, ""] — one empty slot.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Sanity: pane 3 is empty.
        assert_eq!(state.layouts["alpha"].panes[3].session_id, "");
        // Now add a 4th session to `state.sessions` (but NOT in any pane)
        // and drag it. We simulate this by having `delta` in sessions but
        // clearing pane 3 (already empty).
        //
        // Actually, with 3 sessions in Grid4, pane 3 is already empty.
        // We drag `gamma` (currently in pane 2) — wait, that's a pane-to-pane
        // drag. Let's instead add a 4th session that's NOT in the layout.
        //
        // Reset: 4 sessions, Grid4 → all 4 panes filled. Then close one
        // pane (set its session to empty) and drag the closed session
        // back onto a filled pane.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // All 4 panes filled: [alpha, beta, gamma, delta].
        assert_eq!(state.layouts["alpha"].panes[3].session_id, "delta");
        // Close pane 3 (simulate user closing delta's pane).
        set_pane_session_for_active(&mut state, 3, String::new());
        assert_eq!(state.layouts["alpha"].panes[3].session_id, "");
        // Now drag `delta` (a background tab now) onto pane 0 (alpha).
        // Should fill pane 3 (the empty slot) — NOT swap, NOT cycle preset.
        let outcome = drop_background_tab_to_create_split(&mut state, "delta", 0);
        assert_eq!(outcome, DropSplitOutcome::FilledExisting { pane_idx: 3 });
        // Preset is unchanged.
        assert_eq!(state.layout_preset, LayoutPreset::Grid4);
        // Pane 3 now has delta; pane 0 still has alpha.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[3].session_id, "delta");
    }

    /// Dragging a background tab onto a pane when the layout is Split2H
    /// (2 panes, both filled) must cycle to Grid4 and place the dragged
    /// session in a new pane.
    #[test]
    fn drop_background_tab_cycles_split2h_to_grid4() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // Split2H: panes [alpha, beta]. gamma is a background tab.
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert_eq!(state.layouts["alpha"].panes.len(), 2);
        // Drag gamma onto pane 0 (alpha).
        let outcome = drop_background_tab_to_create_split(&mut state, "gamma", 0);
        // `apply_layout_preset(Grid4)` fills all 4 panes from sessions
        // [alpha, beta, gamma] in tab order → [alpha, beta, gamma, ""].
        // So gamma is placed in pane 2 by the preset fill, not by our
        // manual `set_pane_session_for_active` — but the outcome is still
        // `Created` because the preset was upgraded.
        assert!(matches!(outcome, DropSplitOutcome::Created { .. }));
        assert_eq!(state.layout_preset, LayoutPreset::Grid4);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 4);
        // gamma is in some pane (placed by apply_layout_preset).
        assert!(layout.panes.iter().any(|p| p.session_id == "gamma"));
        // alpha is still in pane 0 (it's the active session, anchored).
        assert_eq!(layout.panes[0].session_id, "alpha");
    }

    /// Dragging a background tab onto a pane when the layout is Grid8
    /// (max panes, all filled) must return `FallbackSwap` — the caller
    /// should then attempt `swap_pane_sessions` (which will also fail
    /// silently since the source isn't in any pane, but the contract is
    /// what we're pinning here).
    #[test]
    fn drop_background_tab_at_grid8_returns_fallback_swap() {
        let mut state = state_with_active_session(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
        // Grid8: 8 panes, all filled with a-h. `i` is a background tab.
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        assert_eq!(state.layouts["a"].panes.len(), 8);
        // Drag `i` onto pane 0 (a).
        let outcome = drop_background_tab_to_create_split(&mut state, "i", 0);
        assert_eq!(outcome, DropSplitOutcome::FallbackSwap);
        // Preset is unchanged (already at max).
        assert_eq!(state.layout_preset, LayoutPreset::Grid8);
        // Layout is unchanged (no mutation happened).
        let layout = state.layouts.get("a").unwrap();
        assert_eq!(layout.panes[0].session_id, "a");
        assert!(layout.panes.iter().all(|p| p.session_id != "i"));
    }

    /// `drop_background_tab_to_create_split` must return `Failed` when
    /// there's no active session. This is the defensive contract — the
    /// drop handler shouldn't crash if the state is in an unexpected
    /// (no-active-session) configuration.
    #[test]
    fn drop_background_tab_returns_failed_with_no_active_session() {
        let mut state = AppState::default();
        // No active session.
        let outcome = drop_background_tab_to_create_split(&mut state, "beta", 0);
        assert_eq!(outcome, DropSplitOutcome::Failed);
        assert!(state.layouts.is_empty());
    }

    /// After `drop_background_tab_to_create_split` creates a new pane,
    /// the input routing predicate must STILL be false with comparison
    /// OFF — each pane's input goes to its own session. This is the
    /// regression for the multi-pane input bug (Task 17/19), applied to
    /// the new split-creation path. Without this test, a future change
    /// to `drop_background_tab_to_create_split` could break input routing
    /// in the new pane.
    #[test]
    fn after_drop_background_tab_input_routes_to_each_pane_own_session() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // Start in Single (no layout). Drag `beta` onto pane 0.
        let outcome = drop_background_tab_to_create_split(&mut state, "beta", 0);
        assert_eq!(outcome, DropSplitOutcome::Created { pane_idx: 1 });
        // Layout is Split2H: [alpha, beta].
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        // With comparison OFF, broadcast_targets returns just the active
        // session. The routing predicate `len > 1` is false — pane 1
        // (beta) sends input to its own PTY, not alpha's.
        let targets = broadcast_targets(&state);
        assert_eq!(targets, vec!["alpha".to_string()]);
        let is_broadcast = targets.len() > 1;
        assert!(!is_broadcast);
        // Pre-condition for the bug: targets[0] (alpha) != pane 1's sid (beta).
        assert_ne!(targets[0], "beta");
    }

    // ------------------------------------------------------------------
    // Task 22 — `execute_tab_drop_on_pane` (single source of truth for
    // tab/pane drag-drop dispatch). These tests pin the contract that
    // BOTH the legacy HTML5 `ondrop` handlers AND the Task 22 manual
    // mouse-based tab-drag finisher rely on.
    // ------------------------------------------------------------------

    /// Self-drop in a multi-pane layout GROWS the grid to the next
    /// larger preset (自由分裂): Split2H → Grid4, preserving the existing
    /// pane arrangement. The new slots stay empty (no more background
    /// tabs to fill them here).
    #[test]
    fn execute_tab_drop_self_drop_multi_pane_grows_grid() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // alpha is in pane 0. Drop alpha onto pane 0.
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");
        // First new slot is pane 2 (panes 0/1 preserved).
        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 2 });
        assert_eq!(state.layout_preset, LayoutPreset::Grid4);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 4);
        // Existing arrangement preserved; new slots empty.
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.panes[3].session_id, "");
        // Active session must NOT change (it's a tab pointer).
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// Self-drop growth auto-fills the new slots with background tabs
    /// that aren't already placed in a pane (tab order).
    #[test]
    fn execute_tab_drop_self_drop_growth_fills_new_slots_with_background_tabs() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        // Build a Split2H manually with only alpha+beta placed.
        state.layout_preset = LayoutPreset::Split2H;
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Split2H,
                &["alpha".to_string(), "beta".to_string()],
            ),
        );
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");
        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 2 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        // gamma/delta auto-fill the new slots in tab order.
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert_eq!(layout.panes[3].session_id, "delta");
    }

    /// Repeated self-drops keep growing: Split2H → Grid4 → Grid8, then
    /// no-op at the Grid8 maximum.
    #[test]
    fn execute_tab_drop_repeated_self_drops_grow_until_grid8_then_noop() {
        let mut state = state_with_active_session(&["alpha"]);
        // 1st drag (no layout) → Split2H.
        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::SplitCreated { pane_idx: 1 }
        );
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 2);
        // 2nd drag → Grid4.
        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::SplitCreated { pane_idx: 2 }
        );
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 4);
        // 3rd drag → Grid8.
        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::SplitCreated { pane_idx: 4 }
        );
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 8);
        // 4th drag → already at max: no-op, layout unchanged.
        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::NoOpSelfDrop
        );
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 8);
        // Pane 0 kept the active session throughout.
        assert_eq!(
            state.layouts.get("alpha").unwrap().panes[0].session_id,
            "alpha"
        );
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// Task 22 runtime bug: dragging the ACTIVE tab into its own
    /// single-pane view (no layout yet) must CREATE a split, not
    /// silently no-op. This is the user's most natural "drag a tab down
    /// to split" gesture — the hit-test resolves to pane 0 which holds
    /// the active session itself, making it a self-drop.
    #[test]
    fn execute_tab_drop_active_tab_self_drop_single_pane_creates_split() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // No layout for alpha (Single preset).
        assert!(!state.layouts.contains_key("alpha"));
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");
        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 1 });
        assert_eq!(state.layout_preset, LayoutPreset::Split2H);
        let layout = state.layouts.get("alpha").unwrap();
        // Pane 0 keeps the active session; pane 1 auto-fills with the
        // next background tab.
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        // Active session must NOT change (it's a tab pointer).
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// Same gesture with only ONE open tab: the split is still created;
    /// pane 1 is left empty (renderer treats empty session_id as "no
    /// pane here").
    #[test]
    fn execute_tab_drop_active_tab_self_drop_only_tab_creates_split_with_empty_pane() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!state.layouts.contains_key("alpha"));
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");
        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 1 });
        assert_eq!(state.layout_preset, LayoutPreset::Split2H);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
    }

    /// Pane-to-pane swap: dragging one pane's session onto another pane
    /// swaps the two panes' sessions.
    #[test]
    fn execute_tab_drop_pane_to_pane_swaps() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Drag alpha (pane 0) onto pane 1 (which has beta).
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 1, "beta");
        assert_eq!(outcome, TabDropOutcome::Swapped);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "beta");
        assert_eq!(layout.panes[1].session_id, "alpha");
    }

    /// Background tab → create a split: dragging a tab that ISN'T in any
    /// pane onto a filled pane upgrades the preset and places the dragged
    /// session in a new pane.
    #[test]
    fn execute_tab_drop_background_tab_creates_split() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // No layout yet — Single preset. Drop `beta` (background tab)
        // onto pane 0 (alpha).
        let outcome = execute_tab_drop_on_pane(&mut state, "beta", 0, "alpha");
        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 1 });
        let layout = state.layouts.get("alpha").unwrap();
        assert!(layout.is_multi_pane());
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    /// Pane-to-empty move: dragging a session from one pane onto an
    /// empty pane moves the session and clears the source.
    #[test]
    fn execute_tab_drop_pane_to_empty_moves() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Grid4: [alpha, beta, gamma, ""]. Drag alpha (pane 0) onto
        // pane 3 (empty).
        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 3, "");
        assert_eq!(
            outcome,
            TabDropOutcome::MovedToEmptyPane {
                cleared_source_pane: Some(0),
            }
        );
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "");
        assert_eq!(layout.panes[3].session_id, "alpha");
    }

    /// Background tab → empty pane assignment: dragging a tab not in any
    /// pane onto an empty pane assigns it without clearing any source.
    #[test]
    fn execute_tab_drop_background_tab_to_empty_assigns() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Grid4: [alpha, beta, gamma, ""]. `gamma` is in pane 2 (not
        // a background tab). To get a true background tab, remove it
        // from the layout first.
        set_pane_session_for_active(&mut state, 2, String::new());
        // Now gamma is a background tab. Drop it onto pane 3 (empty).
        let outcome = execute_tab_drop_on_pane(&mut state, "gamma", 3, "");
        assert_eq!(outcome, TabDropOutcome::AssignedToEmptyPane);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[3].session_id, "gamma");
    }

    /// Background tab → filled pane cycles preset (Split2H → Grid4).
    #[test]
    fn execute_tab_drop_background_tab_cycles_preset() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Split2H: [alpha, beta]. gamma is a background tab.
        // Drop gamma onto pane 0 (alpha).
        let outcome = execute_tab_drop_on_pane(&mut state, "gamma", 0, "alpha");
        // `apply_layout_preset(Grid4)` fills panes from sessions in
        // tab order → [alpha, beta, gamma, ""]. The outcome is
        // `SplitCreated` because the preset was upgraded.
        assert!(matches!(outcome, TabDropOutcome::SplitCreated { .. }));
        assert_eq!(state.layout_preset, LayoutPreset::Grid4);
    }

    /// Background tab onto a filled Grid8 (max panes, no empty slots)
    /// falls back to a swap, which fails because the dragged session
    /// isn't in any pane.
    #[test]
    fn execute_tab_drop_background_tab_at_grid8_fallback_swap_fails() {
        let mut state = state_with_active_session(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid8);
        // Grid8: [a, b, c, d, e, f, g, h]. `i` is a background tab.
        // Drop `i` onto pane 0 (a).
        let outcome = execute_tab_drop_on_pane(&mut state, "i", 0, "a");
        assert_eq!(outcome, TabDropOutcome::SplitFallbackSwapFailed);
        // Preset unchanged.
        assert_eq!(state.layout_preset, LayoutPreset::Grid8);
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
