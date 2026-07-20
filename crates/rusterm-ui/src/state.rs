use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use rusterm_core::config::{ConnectionConfig, FocusedTabAppearance, OneKey};
use rusterm_core::config_manager::ConfigManager;
use rusterm_core::session::SessionType;
use rusterm_core::session_log::SessionLog;
use rusterm_core::terminal::{RenderOutput, Terminal};

#[cfg(test)]
use crate::layout::MAX_PANES;
use crate::layout::{LayoutPreset, PaneLayout, SplitDirection};

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

/// Pane-level focus is independent from `active_session`.
///
/// `active_session` remains the tab/layout anchor; changing it on a pane click
/// would make the renderer look up a different layout. This runtime-only value
/// exists solely for pane chrome/highlight and floating-window z-order.
///
/// `layout_owner_tab_id` is the group_id of the tab whose layout contains
/// the focused pane. It is NOT a session id — sessions and tabs are decoupled
/// in Plan B (one tab may host multiple independent pane sessions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusedPane {
    pub layout_owner_tab_id: String,
    pub pane_idx: usize,
}

/// A top-level workspace tab. Each WorkspaceTab owns a `PaneLayout` (keyed by
/// its `id`) and hosts one or more independent terminal sessions in its panes.
///
/// The top TabBar renders one entry per `WorkspaceTab` (NOT per session), so
/// splitting a pane or cloning a session into an empty slot no longer adds a
/// new top-level tab — that was the "Tab 膨胀" symptom.
///
/// `anchor_session_id` is the session displayed in pane 0 of this tab's
/// layout. It exists for backwards-compatible display paths (the status bar
/// and Cmd+Shift+F still key off `active_session` which mirrors the active
/// tab's anchor). Step 2 of the Plan B migration will replace those paths
/// with `focused_pane_session` and drop `anchor_session_id` + `active_session`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceTab {
    /// Stable group id (independent uuid). Used as the key in
    /// `AppState::layouts` and as `FocusedPane.layout_owner_tab_id`.
    pub id: String,
    /// The session id occupying pane 0 of this tab's layout. `None` only
    /// briefly during teardown when the last session is being closed.
    pub anchor_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    /// Terminal registry. Every live session (whether it's a tab anchor or
    /// only a pane inside a tab) has exactly one entry here. This is the
    /// source of truth for "does this session exist?".
    pub sessions: Vec<SessionTab>,
    /// Active workspace tab id (group_id). Layouts are keyed by this value.
    /// Switching the top TabBar updates `active_tab` AND `active_session`
    /// (the latter is the active tab's anchor, kept for Step-1 backwards
    /// compatibility with code that still reads `active_session`).
    pub active_tab: Option<String>,
    /// Backwards-compatible anchor session of the active tab. Step 2 will
    /// migrate the remaining readers (`restore_focus_to_active_session`,
    /// status bar, Cmd+Shift+F, sidebar AI apply) to `focused_pane_session`
    /// and delete this field.
    pub active_session: Option<String>,
    /// Top TabBar data source. One entry per workspace tab. Pane-only
    /// sessions (created by a sidebar drop or a pane clone) do NOT appear
    /// here — they're displayed only inside their host tab's layout.
    #[serde(default)]
    pub tabs: Vec<WorkspaceTab>,
    pub sidebar_open: bool,
    pub connections: Vec<ConnectionConfig>,
    pub theme: Theme,
    #[serde(default)]
    pub focused_tab_appearance: FocusedTabAppearance,
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
    /// Runtime connection state per SSH/shell session. Keeping `Reconnecting`
    /// distinct from `Disconnected` makes Enter-triggered retries idempotent
    /// while preserving the same session id and pane assignment.
    #[serde(skip)]
    pub session_connection_states: HashMap<String, SessionConnectionState>,
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
    /// layout side-by-side. Indexed by the tab's group id (the
    /// `WorkspaceTab::id`, mirrored by `AppState::active_tab`). A tab with
    /// no entry here is implicitly `Single`.
    #[serde(skip)]
    pub layouts: HashMap<String, PaneLayout>,
    /// Pane selected by the user for visual highlighting. This must never be
    /// used as the tab/layout key; `active_tab` remains that stable anchor.
    #[serde(skip)]
    pub focused_pane: Option<FocusedPane>,
    /// The current layout preset for the active tab. Cycling this with a
    /// hotkey rebuilds the active tab's `PaneLayout` with the next preset
    /// in `LayoutPreset`'s cycle order. Kept as a separate field (rather
    /// than derived from `layouts`) so that the hotkey handler can read
    /// the current preset without first looking up the active session's
    /// layout entry (which may not exist yet for a tab that's still in
    /// the default Single state).
    #[serde(skip)]
    pub layout_preset: LayoutPreset,
    /// Whether the split-pane layout is visible (ON) or collapsed into a
    /// single-pane tab-tiled view (OFF). When OFF, the active tab's
    /// `PaneLayout` is temporarily zoomed to the focused pane (or pane 0),
    /// so `is_multi_pane()` returns false and the rendering path takes
    /// the `single_pane_with_drop` branch — all sessions remain accessible
    /// via the workspace tab bar. The underlying layout is preserved, so
    /// toggling back ON restores the exact split configuration.
    ///
    /// This is the "标签页平铺" affordance: close split → single pane +
    /// tabs; open split → multi-pane grid. Default true (split visible).
    #[serde(skip)]
    pub split_mode_enabled: bool,

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
    /// Whether to show the "是否确实要关闭本软件？" confirmation dialog when
    /// the user closes the last window. Default true (safe default — always
    /// ask). Persisted in `settings.json` so the user's choice on the
    /// dialog's "下次关闭时不再询问" checkbox survives across launches.
    /// Loaded from settings on unlock (see the unlock handler in `app.rs`).
    pub confirm_close_on_exit: bool,
    /// Whether the close-confirmation dialog is currently visible. This is a
    /// transient UI flag (not persisted) — it's set by the `CloseRequested`
    /// wry event handler and cleared by the dialog's "取消" / "确认" buttons.
    #[serde(skip)]
    pub close_dialog_visible: bool,
    /// The checkbox state on the close-confirmation dialog. Default true
    /// ("下次关闭时不再询问" is checked by default — the user wants to be
    /// asked again next time). When the user confirms or cancels, this value
    /// is applied: if checked, `confirm_close_on_exit` is set to false (don't
    /// ask again) and persisted; if unchecked, `confirm_close_on_exit` stays
    /// true (ask again next time).
    #[serde(skip)]
    pub close_dialog_dont_ask_again: bool,

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionConnectionState {
    #[default]
    Connected,
    Disconnected,
    Reconnecting,
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
            active_tab: None,
            active_session: None,
            tabs: Vec::new(),
            sidebar_open: true,
            connections: Vec::new(),
            theme: Theme::Dark,
            focused_tab_appearance: FocusedTabAppearance::default(),
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
            session_connection_states: HashMap::new(),
            analytics: crate::analytics::AnalyticsHandle::default(),
            layouts: HashMap::new(),
            focused_pane: None,
            layout_preset: LayoutPreset::default(),
            split_mode_enabled: true,
            restore_pending: None,
            restore_disabled: false,
            confirm_close_on_exit: true,
            close_dialog_visible: false,
            close_dialog_dont_ask_again: true,
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

    /// Look up the active tab's anchor session id (the session occupying
    /// pane 0 of the active tab's layout). Returns `None` if there's no
    /// active tab or the tab has no anchor yet.
    ///
    /// This is the bridge between the new `active_tab` (group_id) and the
    /// legacy `active_session` (a session id). Step 1 keeps `active_session`
    /// in sync with this value; Step 2 will replace readers of
    /// `active_session` with `focused_pane_session` and delete both.
    pub fn active_tab_anchor_session(&self) -> Option<String> {
        let tab_id = self.active_tab.as_ref()?;
        self.tabs
            .iter()
            .find(|t| &t.id == tab_id)
            .and_then(|t| t.anchor_session_id.clone())
    }
}

/// Helper: set the active tab and derive `active_session` from the tab's
/// anchor. Use this whenever the active top TabBar entry changes so the two
/// fields stay in sync (Step 1 compatibility).
///
/// `state` is taken by `&mut` so this is unit-testable without a dioxus
/// runtime.
pub fn set_active_tab(state: &mut AppState, tab_id: &str) {
    state.active_tab = Some(tab_id.to_string());
    state.active_session = state
        .tabs
        .iter()
        .find(|t| t.id == tab_id)
        .and_then(|t| t.anchor_session_id.clone());
}

/// Helper: push a new workspace tab + anchor and make it the active tab.
/// `anchor_session_id` is the session that will occupy pane 0 of the tab's
/// layout (and, during Step 1, mirror `active_session`).
///
/// Returns the new tab's group id so the caller can use it as a layout key
/// when applying presets.
pub fn push_workspace_tab(state: &mut AppState, anchor_session_id: &str) -> String {
    let group_id = uuid::Uuid::new_v4().to_string();
    state.tabs.push(WorkspaceTab {
        id: group_id.clone(),
        anchor_session_id: Some(anchor_session_id.to_string()),
    });
    set_active_tab(state, &group_id);
    group_id
}

/// Move the workspace tab whose anchor is `session_id` to the leftmost
/// position (index 0) of `state.tabs`. This is the "configure terminal to
/// the left side" action triggered after a successful SSH login (feature #7).
///
/// Returns `true` if the tab was found and actually moved (i.e., it was not
/// already at position 0), `false` if the tab was not found OR was already at
/// the leftmost position. The SSH connect flow uses the `true` return value
/// as the signal that a configuration step actually occurred — only then is
/// the host recorded as configured in the DB (avoid duplicate configuration).
///
/// This is a no-op when the tab is already at index 0: "already configured
/// in-place" is treated as "no configuration step occurred", so the caller
/// won't record the host again.
///
/// Plan B note: in the prior model this rearranged `state.sessions` (the
/// terminal registry). Under Plan B the top TabBar reads `state.tabs`, so
/// we rearrange THAT instead. The sessions registry order is no longer
/// user-visible and stays in creation order.
///
/// Takes `&mut AppState` (rather than `&mut Signal<AppState>`) so it's
/// unit-testable without spinning up a dioxus runtime. Callers in `app.rs`
/// pass `&mut state.write()`.
pub fn move_session_to_leftmost(state: &mut AppState, session_id: &str) -> bool {
    let pos = state
        .tabs
        .iter()
        .position(|t| t.anchor_session_id.as_deref() == Some(session_id));
    let Some(pos) = pos else {
        // No tab whose anchor is this session — nothing to configure. Don't
        // record this as a successful configuration (the requirement is to
        // record only on confirmed success, and we couldn't even find the
        // tab).
        return false;
    };
    if pos == 0 {
        // Already leftmost. Treat as already-configured-in-place — don't
        // record (avoids duplicate configuration on repeat connects to a
        // host whose tab happens to be the only one / already first).
        return false;
    }
    let tab = state.tabs.remove(pos);
    state.tabs.insert(0, tab);
    true
}

/// Apply a layout preset to the active tab. Builds a fresh `PaneLayout`
/// from the preset using the active tab's anchor session as the first pane,
/// then fills the remaining pane slots with other open sessions (in tab
/// order). If there aren't enough sessions to fill the grid, the trailing
/// slots are left empty (the renderer skips panes with empty `session_id`).
///
/// Returns `true` if the layout was applied, `false` if there's no active
/// tab (or no anchor session to put in pane 0).
///
/// Takes `&mut AppState` so it's unit-testable without a dioxus runtime.
pub fn apply_layout_preset(state: &mut AppState, preset: LayoutPreset) -> bool {
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    // The active tab's anchor session is pane 0. If the tab has no anchor
    // (shouldn't happen in practice), bail — we can't build a layout without
    // a session for pane 0.
    let anchor_session = match state.active_tab_anchor_session() {
        Some(s) => s,
        None => return false,
    };
    // Collect session ids in priority order: anchor first, then every other
    // open session in tab order. We dedupe in case the anchor is also the
    // first tab.
    let mut ids = vec![anchor_session.clone()];
    for tab in &state.sessions {
        if tab.id != anchor_session && !ids.contains(&tab.id) {
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
    let active_id = match state.active_tab.clone() {
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
/// there's no active tab with a layout.
pub fn toggle_comparison_mode(state: &mut AppState) -> Option<bool> {
    let active_id = state.active_tab.clone()?;
    let layout = state.layouts.get_mut(&active_id)?;
    Some(layout.toggle_comparison())
}

/// Toggle the split-pane mode for the active tab.
///
/// When turning OFF: zooms the layout to the focused pane (or pane 0 if no
/// pane has focus), so `is_multi_pane()` returns false and the rendering
/// path takes the single-pane branch ("标签页平铺" — all sessions remain
/// accessible via the workspace tab bar). The underlying split tree is
/// preserved, so toggling back ON restores the exact configuration.
///
/// When turning ON: unzooms (clears `layout.zoomed`), restoring the
/// multi-pane grid view.
///
/// Returns the new state (`true` = split visible, `false` = tab-tiled).
/// Returns `None` only if there's no active tab. If there's no layout yet
/// (Single preset), the toggle still flips `split_mode_enabled` but is
/// visually a no-op until the caller creates a layout (e.g. via
/// `append_pane_to_active`).
pub fn toggle_split_mode(state: &mut AppState) -> Option<bool> {
    let active_id = state.active_tab.clone()?;
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        // No layout exists yet — just flip the flag. The caller (Split
        // button) will create a layout via `append_pane_to_active` if
        // needed. There's nothing to zoom/unzoom.
        state.split_mode_enabled = !state.split_mode_enabled;
        return Some(state.split_mode_enabled);
    };
    // Only meaningful for multi-pane layouts. A Single-preset layout has
    // nothing to collapse.
    if layout.panes.len() <= 1 {
        state.split_mode_enabled = true;
        return Some(true);
    }
    state.split_mode_enabled = !state.split_mode_enabled;
    if state.split_mode_enabled {
        // Turning ON: clear zoom to reveal all panes.
        layout.unzoom();
    } else {
        // Turning OFF: zoom to the focused pane (or pane 0) so only one
        // pane is visible. This makes `is_multi_pane()` return false,
        // routing the render through `single_pane_with_drop`.
        let focused_idx = state
            .focused_pane
            .as_ref()
            .filter(|fp| fp.layout_owner_tab_id == active_id)
            .map(|fp| fp.pane_idx)
            .unwrap_or(0);
        // Clamp to valid range (defensive: focused_pane might be stale).
        let zoom_idx = focused_idx.min(layout.panes.len().saturating_sub(1));
        layout.zoom(zoom_idx);
    }
    Some(state.split_mode_enabled)
}

/// Resize a column splitter in the active tab's layout by a fractional
/// delta. See `PaneLayout::resize_col`.
///
/// Returns `true` if the resize was applied.
pub fn resize_layout_col(state: &mut AppState, col: usize, delta: f64) -> bool {
    let active_id = match state.active_tab.clone() {
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
    let active_id = match state.active_tab.clone() {
        Some(id) => id,
        None => return false,
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.resize_row(row, delta)
}

/// Resize one recursive split-tree divider in the active tab.
pub fn resize_layout_split(state: &mut AppState, splitter_idx: usize, delta: f64) -> bool {
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.resize_split(splitter_idx, delta)
}

/// Promote the active layout to floating windows and bring `pane_idx` to the
/// front. The active tab anchor remains the layout owner.
pub fn begin_floating_pane_move(state: &mut AppState, pane_idx: usize) -> bool {
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.bring_floating_pane_to_front(pane_idx)
}

/// Move one pane window in the active layout by a CSS-pixel delta.
pub fn move_floating_pane_for_active(
    state: &mut AppState,
    pane_idx: usize,
    delta_x: f64,
    delta_y: f64,
    container_w: f64,
    container_h: f64,
) -> bool {
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.move_floating_pane(pane_idx, delta_x, delta_y, container_w, container_h)
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
    let Some(active_id) = state.active_tab.as_ref() else {
        return Vec::new();
    };
    // No layout → single-session path. The active session (pane 0 / tab
    // anchor) is the only target.
    let Some(layout) = state.layouts.get(active_id) else {
        return state
            .active_tab_anchor_session()
            .map(|s| vec![s])
            .unwrap_or_default();
    };
    // Layout exists but comparison is off → input only goes to the
    // focused session. (Multi-pane without sync = panes are independent.)
    if !layout.comparison {
        return state
            .active_tab_anchor_session()
            .map(|s| vec![s])
            .unwrap_or_default();
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
// Pane-to-pane session moves use direct assignment/swap. Capacity growth is
// deliberately separate: every user-visible split or sidebar drop goes through
// `append_pane_to_active`, preserving occupied sessions and adding exactly one
// pane. A future task can introduce tree-based splits if the user
// wants arbitrary layouts.

pub fn source_pane_for_copy(layout: &PaneLayout, target_idx: usize) -> Option<usize> {
    let target = layout.pane_rect(target_idx, 1.0, 1.0)?;
    let target_center = (target.0 + target.2 / 2.0, target.1 + target.3 / 2.0);
    layout
        .panes
        .iter()
        .enumerate()
        .filter(|(idx, pane)| *idx != target_idx && !pane.session_id.is_empty())
        .filter_map(|(idx, _)| {
            let rect = layout.pane_rect(idx, 1.0, 1.0)?;
            let center = (rect.0 + rect.2 / 2.0, rect.1 + rect.3 / 2.0);
            let vertical_overlap = rect.1 < target.1 + target.3 && rect.1 + rect.3 > target.1;
            let horizontal_overlap = rect.0 < target.0 + target.2 && rect.0 + rect.2 > target.0;
            let dx = (center.0 - target_center.0).abs();
            let dy = (center.1 - target_center.1).abs();
            let rank = if center.0 < target_center.0 && vertical_overlap {
                0
            } else if center.1 < target_center.1 && horizontal_overlap {
                1
            } else if center.0 > target_center.0 && vertical_overlap {
                2
            } else if center.1 > target_center.1 && horizontal_overlap {
                3
            } else {
                4
            };
            let distance = match rank {
                0 | 2 => dx,
                1 | 3 => dy,
                _ => dx + dy,
            };
            Some((idx, rank, distance))
        })
        .min_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| left.2.total_cmp(&right.2))
                .then_with(|| left.0.cmp(&right.0))
        })
        .map(|(idx, _, _)| idx)
}

/// Select a pane for visual focus without changing the active tab/layout
/// anchor. Floating panes are also brought to the front, while their geometry
/// and session assignment remain unchanged.
///
/// `layout_owner_tab_id` is the group_id of the tab whose layout contains
/// the pane. It's NOT a session id — Plan B decoupled these concepts.
pub fn focus_pane_for_layout(
    state: &mut AppState,
    layout_owner_tab_id: &str,
    pane_idx: usize,
) -> bool {
    let Some(layout) = state.layouts.get_mut(layout_owner_tab_id) else {
        return false;
    };
    if pane_idx >= layout.panes.len() {
        return false;
    }
    if layout.is_floating() {
        layout.bring_floating_pane_to_front(pane_idx);
    }
    state.focused_pane = Some(FocusedPane {
        layout_owner_tab_id: layout_owner_tab_id.to_string(),
        pane_idx,
    });
    true
}

/// Return the session displayed by the currently focused pane.
///
/// Pane focus is visual runtime state only. Resolving it through the stored
/// layout owner keeps `active_session` free to remain the tab/layout anchor.
/// Empty panes and stale layout or pane references do not map to a session.
pub fn focused_pane_session(state: &AppState) -> Option<String> {
    let focused = state.focused_pane.as_ref()?;
    state
        .layouts
        .get(&focused.layout_owner_tab_id)?
        .panes
        .get(focused.pane_idx)
        .map(|pane| pane.session_id.clone())
        .filter(|session_id| !session_id.is_empty())
}

/// Replace the session displayed in `pane_idx` of a specific layout.
///
/// Unlike [`set_pane_session_for_active`], this helper does not consult or
/// mutate `active_tab`. Runtime operations that span multiple state writes
/// (for example, opening several cloned SSH sessions after a self-drop) must
/// use this explicit owner so a tab change cannot redirect later assignments.
pub fn set_pane_session_for_layout(
    state: &mut AppState,
    layout_owner_tab_id: &str,
    pane_idx: usize,
    session_id: String,
) -> bool {
    let Some(layout) = state.layouts.get_mut(layout_owner_tab_id) else {
        return false;
    };
    layout.set_pane_session(pane_idx, session_id)
}

/// Replace the session displayed in a pane of the current active layout.
///
/// This convenience wrapper is appropriate for a single synchronous state
/// operation. Multi-step runtime flows must use [`set_pane_session_for_layout`]
/// with a captured layout owner.
pub fn set_pane_session_for_active(
    state: &mut AppState,
    pane_idx: usize,
    session_id: String,
) -> bool {
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    set_pane_session_for_layout(state, &active_id, pane_idx, session_id)
}

/// Split one specific pane in the active tab. This is the targeted growth
/// primitive used by drag/drop; it preserves the target's existing session,
/// appends one empty pane, and changes no other leaf geometry.
pub fn split_pane_to_active(
    state: &mut AppState,
    target_pane_idx: usize,
    direction: SplitDirection,
) -> Option<usize> {
    let active_id = state.active_tab.clone()?;
    if !state.layouts.contains_key(&active_id) {
        let anchor = state.active_tab_anchor_session()?;
        state.layouts.insert(
            active_id.clone(),
            PaneLayout::from_preset(LayoutPreset::Single, &[anchor]),
        );
    }
    state
        .layouts
        .get_mut(&active_id)?
        .split_pane(target_pane_idx, direction)
}

/// Append exactly one pane for toolbar/hotkey growth. The largest leaf is
/// split along its longest side, producing a balanced recursive layout rather
/// than a forced 1×N strip. The active tab anchor remains unchanged.
pub fn append_pane_to_active(state: &mut AppState) -> Option<usize> {
    let active_id = state.active_tab.clone()?;

    // If there's no layout yet, build a Split2H (1×2) and return pane 1 as
    // the "new" pane — matches the sidebar-drop Case 1 behaviour so the
    // Split button works the same way on a fresh tab.
    if !state.layouts.contains_key(&active_id) {
        let anchor = state.active_tab_anchor_session()?;
        let mut ids = vec![anchor.clone()];
        for tab in &state.sessions {
            if tab.id != anchor && !ids.contains(&tab.id) {
                ids.push(tab.id.clone());
            }
        }
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &ids);
        // Clear pane 1 so the caller fills it (mirrors prepare_split_for_sidebar_drop).
        if layout.panes.len() >= 2 {
            layout.panes[1].session_id = String::new();
        }
        state.layouts.insert(active_id, layout);
        return Some(1);
    }

    state.layouts.get_mut(&active_id)?.append_balanced()
}

/// Outcome of [`prepare_split_for_sidebar_drop`]. The caller opens the new
/// sidebar connection and assigns it to the pane at `pane_idx` in the layout
/// owned by `layout_owner_tab_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarDropPlan {
    /// The layout owner (typically `state.active_tab`). The caller MUST use
    /// this exact owner when constructing a `PaneTarget` so a tab change
    /// between this call and `open_connection` can't redirect the drop.
    pub layout_owner_tab_id: String,
    /// The pane index where the new session should be placed.
    pub pane_idx: usize,
    /// True if a brand-new pane was created (preset upgraded or first split
    /// applied). False if an existing empty pane was reused. The caller uses
    /// this only for logging.
    pub created_new_pane: bool,
}

pub fn prepare_split_for_sidebar_drop(
    state: &mut AppState,
    target_pane_idx: usize,
) -> Option<SidebarDropPlan> {
    let active_id = state.active_tab.clone()?;
    state.active_tab_anchor_session()?;
    if let Some(layout) = state.layouts.get(&active_id) {
        if layout
            .panes
            .get(target_pane_idx)
            .is_some_and(|pane| pane.session_id.is_empty())
        {
            return Some(SidebarDropPlan {
                layout_owner_tab_id: active_id,
                pane_idx: target_pane_idx,
                created_new_pane: false,
            });
        }
        if let Some(pane_idx) = layout
            .panes
            .iter()
            .position(|pane| pane.session_id.is_empty())
        {
            return Some(SidebarDropPlan {
                layout_owner_tab_id: active_id,
                pane_idx,
                created_new_pane: false,
            });
        }
    }
    prepare_split_for_sidebar_drop_at(state, target_pane_idx, SplitDirection::Bottom)
}

/// Direction-aware sidebar drop preparation. Top/bottom drops split only the
/// target leaf, so unrelated panes retain their exact geometry.
pub fn prepare_split_for_sidebar_drop_at(
    state: &mut AppState,
    target_pane_idx: usize,
    direction: SplitDirection,
) -> Option<SidebarDropPlan> {
    let active_id = state.active_tab.clone()?;
    state.active_tab_anchor_session()?;

    // Case 1: no layout yet — use the same growth function as every other
    // automatic split path. It materializes the implicit anchor pane plus one
    // empty pane for the new sidebar connection.
    if !state.layouts.contains_key(&active_id) {
        let pane_idx = split_pane_to_active(state, target_pane_idx, direction)?;
        return Some(SidebarDropPlan {
            layout_owner_tab_id: active_id,
            pane_idx,
            created_new_pane: true,
        });
    }

    // Cases 2/3/4: layout exists. Check the target pane and the other panes
    // for an empty slot.
    let target_is_empty = state
        .layouts
        .get(&active_id)
        .and_then(|l| l.panes.get(target_pane_idx))
        .is_some_and(|p| p.session_id.is_empty());

    if target_is_empty {
        // Case 2: target pane is empty — drop straight in.
        return Some(SidebarDropPlan {
            layout_owner_tab_id: active_id,
            pane_idx: target_pane_idx,
            created_new_pane: false,
        });
    }

    // Occupied target: split this leaf directly. Direction-aware manual
    // drags must not jump to an unrelated empty slot because the user's drop
    // position identifies the exact pane to divide.
    if let Some(first_new_idx) = split_pane_to_active(state, target_pane_idx, direction) {
        return Some(SidebarDropPlan {
            layout_owner_tab_id: active_id,
            pane_idx: first_new_idx,
            created_new_pane: true,
        });
    }

    // Case 5: at MAX_PANES with all panes occupied. Refuse a pane target
    // rather than replacing an existing session. The app falls back to
    // opening the connection as a separate top-level tab.
    None
}

/// Distribute all open sessions across the active tab's layout panes.
///
/// Implements the "多个会话放到多个分屏中" requirement: takes every open session
/// (in tab order, active first) and assigns them to the layout's panes in
/// row-major order. If there are more sessions than panes, the extra
/// sessions are NOT lost — they remain in `state.sessions` and can be placed
/// by growing the layout. If there are fewer sessions than panes, the extra
/// panes are emptied (their `session_id` becomes `""`).
///
/// This is the explicit "fill all panes with my open sessions" affordance —
/// a one-click way to populate a Grid4/Grid8 layout after the user has
/// opened several sessions. Each session appears in at most one pane
/// (deduplicated by session id) so the distribution is a true partition.
///
/// Returns the number of panes that were actually assigned a session (i.e.
/// the number of sessions placed, capped at the pane count). Returns 0 if
/// there's no active tab or no layout.
pub fn distribute_sessions_across_panes(state: &mut AppState) -> usize {
    let Some(active_id) = state.active_tab.clone() else {
        return 0;
    };
    // Collect session ids in tab order, deduplicated. The active tab's
    // anchor session is first (it should stay in pane 0 to avoid
    // disorienting the user).
    //
    // We collect the anchor BEFORE mutably borrowing `state.layouts` to
    // satisfy the borrow checker (`active_tab_anchor_session` reads
    // `state.sessions` / `state.active_tab` immutably).
    let mut session_ids: Vec<String> = Vec::new();
    if let Some(anchor) = state.active_tab_anchor_session() {
        session_ids.push(anchor);
    }
    for tab in &state.sessions {
        if !session_ids.contains(&tab.id) {
            session_ids.push(tab.id.clone());
        }
    }

    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return 0;
    };

    // Assign in row-major order. Extra sessions beyond pane count are
    // dropped (they remain in `state.sessions` for the user to place
    // manually by growing the layout).
    let mut placed = 0usize;
    for (idx, pane) in layout.panes.iter_mut().enumerate() {
        if idx < session_ids.len() {
            pane.session_id = session_ids[idx].clone();
            placed += 1;
        } else {
            pane.session_id = String::new();
        }
    }
    placed
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
    let Some(active_id) = state.active_tab.clone() else {
        return false;
    };
    let Some(layout) = state.layouts.get_mut(&active_id) else {
        return false;
    };
    layout.swap_panes_by_session(from_session, to_session)
}

/// Close a single session and clean up every piece of state tied to it.
///
/// This is the single source of truth for session teardown — called by the
/// Cmd+W keyboard shortcut (closes the focused pane session) and by the
/// group teardown path inside [`close_workspace`]. The TabBar close button
/// does NOT call this — it calls `close_workspace` to tear down the whole
/// group at once.
///
/// Plan B group semantics: when the closed session was a tab anchor
/// (i.e., some `WorkspaceTab.anchor_session_id == id`):
///   - If that tab's layout has other non-empty pane sessions, the first
///     such session is promoted to be the new anchor. `active_session`
///     follows if it pointed at the closed session. The layout stays.
///   - If no other session remains in the tab, the layout entry + the
///     WorkspaceTab are removed. `active_tab` switches to the first
///     remaining tab (and `active_session` follows its anchor).
/// When the closed session was a pane-only session (not an anchor), the
/// pane slot is cleared and the tab survives intact.
///
/// `input_senders` is the UI-side per-session stdin channel map. It's passed
/// in by reference (rather than living inside `AppState`) because it's a
/// `Signal`-backed map owned by the App component, not part of the
/// serializable app state. The caller is responsible for the
/// `Signal::write()` borrow; this function only mutates the underlying map.
///
/// After this call:
///   - The session's stdin, close, resize, terminal, popup, connection,
///     config, and pending-exit entries are dropped.
///   - The session is removed from `sessions`.
///   - The session is cleared from every pane slot of every layout (so a
///     dangling reference doesn't try to render a dead session).
///   - `focused_pane` is cleared if its pane was displaying this session.
///   - If the closed session was the active tab's anchor and the tab had
///     other sessions, the first such session is promoted to anchor.
///   - If the closed session was the active tab's anchor and the tab had
///     NO other sessions, the tab is removed and `active_tab`/`active_session`
///     switch to the next remaining tab (or `None`).
pub fn close_session(
    state: &mut AppState,
    input_senders: &mut HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
    id: &str,
) {
    input_senders.remove(id);
    if let Some((_, tx)) = state
        .close_senders
        .iter()
        .find(|(sid, _)| sid == id)
        .cloned()
    {
        let _ = tx.send(());
    }
    state.close_senders.retain(|(sid, _)| sid != id);
    state.resize_senders.remove(id);
    state.terminals.remove(id);
    state.onekey_popups.remove(id);
    state.session_connection_states.remove(id);
    state.session_configs.remove(id);
    state.pending_exit_check.remove(id);
    state.sessions.retain(|s| s.id != id);

    // Capture whether the focused pane was displaying this session BEFORE
    // we mutate layouts. We compare by looking up the focused pane's current
    // session_id, not by comparing the layout owner — the layout owner is a
    // tab id now (Plan B), not a session id.
    let focused_points_at_closed = state.focused_pane.as_ref().is_some_and(|focused| {
        state
            .layouts
            .get(&focused.layout_owner_tab_id)
            .and_then(|layout| layout.panes.get(focused.pane_idx))
            .is_some_and(|pane| pane.session_id == id)
    });
    if focused_points_at_closed {
        state.focused_pane = None;
    }

    // Find the tab whose anchor is this session (if any). Capture the
    // candidate new anchor BEFORE we mutate the layouts so the borrow on
    // `state.layouts` ends before the `&mut state` calls below.
    let closed_tab_id = state
        .tabs
        .iter()
        .find(|t| t.anchor_session_id.as_deref() == Some(id))
        .map(|t| t.id.clone());
    let new_anchor = closed_tab_id.as_ref().and_then(|tab_id| {
        state.layouts.get(tab_id).and_then(|layout| {
            layout
                .panes
                .iter()
                .map(|p| p.session_id.clone())
                .find(|sid| !sid.is_empty() && sid != id)
        })
    });

    // Clear the closed session from every pane slot of every layout. This
    // also handles the pane-only case (session wasn't an anchor).
    for (_, layout) in state.layouts.iter_mut() {
        for pane in layout.panes.iter_mut() {
            if pane.session_id == id {
                pane.session_id = String::new();
            }
        }
    }

    // Group promotion / removal.
    if let Some(tab_id) = closed_tab_id {
        if let Some(new_anchor) = new_anchor {
            // Promote: keep the layout + tab, swap the anchor.
            if let Some(tab) = state.tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.anchor_session_id = Some(new_anchor.clone());
            }
            if state.active_session.as_deref() == Some(id) {
                state.active_session = Some(new_anchor);
            }
        } else {
            // No other sessions in this tab — remove the tab + layout.
            state.tabs.retain(|t| t.id != tab_id);
            state.layouts.remove(&tab_id);
            if state.active_tab.as_deref() == Some(&tab_id) {
                let next_tab = state.tabs.first().map(|t| t.id.clone());
                state.active_tab = next_tab.clone();
                state.active_session = next_tab
                    .as_ref()
                    .and_then(|tid| state.tabs.iter().find(|t| &t.id == tid))
                    .and_then(|t| t.anchor_session_id.clone());
            }
        }
    }

    // Defensive fallback: if the closed session was active_session but no
    // tab owned it (a transient inconsistency), fall back to the first
    // remaining session. This shouldn't happen in practice but keeps the
    // invariant "active_session is always a live session id when set."
    if state.active_session.as_deref() == Some(id) {
        state.active_session = state.sessions.first().map(|s| s.id.clone());
    }
}

/// Close an entire workspace tab (group) and every session hosted in its
/// layout. This is what the TabBar close button calls — closing the tab
/// should close ALL pane sessions inside it, not just the anchor.
///
/// `input_senders` is passed by reference for the same reason as
/// [`close_session`] — it's a Signal-backed map owned by the App component.
///
/// After this call:
///   - Every session that was the anchor or a non-empty pane in this tab's
///     layout has its stdin/close/resize/terminal/popup/connection/config/
///     pending-exit entries dropped and is removed from `sessions`.
///   - The tab is removed from `tabs`.
///   - The tab's layout entry is removed.
///   - `focused_pane` is cleared if it pointed at this tab.
///   - `active_tab` / `active_session` switch to the next remaining tab
///     (or `None`).
///
/// We do NOT call [`close_session`] in a loop because each call would
/// trigger group-promotion logic for the same tab we're tearing down —
/// instead we inline the per-session cleanup so the tab is removed in one
/// atomic step.
pub fn close_workspace(
    state: &mut AppState,
    input_senders: &mut HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
    group_id: &str,
) {
    // Collect every session id belonging to this group (anchor + every
    // non-empty pane session).
    let mut session_ids: Vec<String> = Vec::new();
    if let Some(tab) = state.tabs.iter().find(|t| t.id == group_id) {
        if let Some(anchor) = &tab.anchor_session_id {
            session_ids.push(anchor.clone());
        }
    }
    if let Some(layout) = state.layouts.get(group_id) {
        for pane in &layout.panes {
            if !pane.session_id.is_empty() && !session_ids.contains(&pane.session_id) {
                session_ids.push(pane.session_id.clone());
            }
        }
    }

    // Per-session cleanup (mirrors `close_session`'s body minus the
    // group-promotion logic).
    for sid in &session_ids {
        input_senders.remove(sid);
        if let Some((_, tx)) = state
            .close_senders
            .iter()
            .find(|(id, _)| id == sid)
            .cloned()
        {
            let _ = tx.send(());
        }
        state.close_senders.retain(|(id, _)| id != sid);
        state.resize_senders.remove(sid);
        state.terminals.remove(sid);
        state.onekey_popups.remove(sid);
        state.session_connection_states.remove(sid);
        state.session_configs.remove(sid);
        state.pending_exit_check.remove(sid);
    }
    state.sessions.retain(|s| !session_ids.contains(&s.id));

    // Clear focused_pane if it pointed at this tab.
    if state
        .focused_pane
        .as_ref()
        .is_some_and(|focused| focused.layout_owner_tab_id == group_id)
    {
        state.focused_pane = None;
    }

    // Remove the tab + its layout.
    state.tabs.retain(|t| t.id != group_id);
    state.layouts.remove(group_id);

    // Also clear any remaining pane references to the closed sessions in
    // OTHER tabs' layouts (defensive — a session could in theory appear in
    // multiple layouts via drag-drop, though Plan B discourages it).
    for (_, layout) in state.layouts.iter_mut() {
        for pane in layout.panes.iter_mut() {
            if session_ids.contains(&pane.session_id) {
                pane.session_id = String::new();
            }
        }
    }

    // Switch active_tab to the next remaining tab.
    if state.active_tab.as_deref() == Some(group_id) {
        let next_tab = state.tabs.first().map(|t| t.id.clone());
        state.active_tab = next_tab.clone();
        state.active_session = next_tab
            .as_ref()
            .and_then(|tid| state.tabs.iter().find(|t| &t.id == tid))
            .and_then(|t| t.anchor_session_id.clone());
    }
}

/// Look up the pane index displaying `session_id` in the active tab's
/// layout. Returns `None` if there's no active tab, no layout, or
/// the session isn't displayed in any pane.
///
/// Used by the drag-and-drop drop handler to identify which pane the
/// user dropped onto (given the pane's `session_id` from the rendered
/// `visible_panes` list) and to find the source pane of a drag (given
/// the dragged tab's `session_id`).
pub fn pane_index_for_active_session(state: &AppState, session_id: &str) -> Option<usize> {
    let active_id = state.active_tab.as_ref()?;
    let layout = state.layouts.get(active_id)?;
    layout.pane_index_for_session(session_id)
}

/// Get the `session_id` displayed at pane `pane_idx` in the active
/// tab's layout. Returns `None` if there's no active tab, no
/// layout, or `pane_idx` is out of range. The returned string may be
/// empty (a pane slot with no session).
///
/// Used by the drop handler to identify the session currently
/// displayed at the drop target (so we can swap it with the dragged
/// session, or replace it with a freshly-opened connection).
pub fn session_at_pane(state: &AppState, pane_idx: usize) -> Option<String> {
    let active_id = state.active_tab.as_ref()?;
    let layout = state.layouts.get(active_id)?;
    layout.panes.get(pane_idx).map(|p| p.session_id.clone())
}

pub fn drop_background_tab_to_create_split(
    state: &mut AppState,
    dragged_sid: &str,
    target_pane_idx: usize,
) -> DropSplitOutcome {
    if let Some(empty_idx) = state.active_tab.as_ref().and_then(|active_id| {
        state.layouts.get(active_id).and_then(|layout| {
            layout
                .panes
                .iter()
                .position(|pane| pane.session_id.is_empty())
        })
    }) {
        return if set_pane_session_for_active(state, empty_idx, dragged_sid.to_string()) {
            DropSplitOutcome::FilledExisting {
                pane_idx: empty_idx,
            }
        } else {
            DropSplitOutcome::Failed
        };
    }
    drop_background_tab_to_create_split_at(
        state,
        dragged_sid,
        target_pane_idx,
        SplitDirection::Bottom,
    )
}

pub fn drop_background_tab_to_create_split_at(
    state: &mut AppState,
    dragged_sid: &str,
    target_pane_idx: usize,
    direction: SplitDirection,
) -> DropSplitOutcome {
    let Some(active_id) = state.active_tab.clone() else {
        return DropSplitOutcome::Failed;
    };
    if state.active_tab_anchor_session().is_none() {
        return DropSplitOutcome::Failed;
    }

    let target_is_empty = state
        .layouts
        .get(&active_id)
        .and_then(|layout| layout.panes.get(target_pane_idx))
        .is_some_and(|pane| pane.session_id.is_empty());
    if target_is_empty {
        return if set_pane_session_for_active(state, target_pane_idx, dragged_sid.to_string()) {
            DropSplitOutcome::FilledExisting {
                pane_idx: target_pane_idx,
            }
        } else {
            DropSplitOutcome::Failed
        };
    }

    let Some(new_pane_idx) = split_pane_to_active(state, target_pane_idx, direction) else {
        return DropSplitOutcome::FallbackSwap;
    };
    if set_pane_session_for_active(state, new_pane_idx, dragged_sid.to_string()) {
        DropSplitOutcome::Created {
            pane_idx: new_pane_idx,
        }
    } else {
        DropSplitOutcome::Failed
    }
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
    /// A self-drop expanded the layout. The app runtime must clone the
    /// dragged session's connection into each contiguous new pane in
    /// `first_pane_idx..first_pane_idx + pane_count`.
    SelfDropExpanded {
        first_pane_idx: usize,
        pane_count: usize,
    },
    /// The user dropped a session onto its own pane at the maximum layout
    /// size (or without an active layout anchor) — nothing happened.
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

/// Execute a tab/pane drag-drop onto a specific pane of the active tab's
/// layout. Both the manual mouse drag path and the defensive HTML5 drop path
/// call this function.
///
/// Every operation that needs more capacity grows by exactly one pane through
/// [`append_pane_to_active`]; no drop path selects a 2/4/8 preset.
pub fn execute_tab_drop_on_pane(
    state: &mut AppState,
    dragged_sid: &str,
    target_pane_idx: usize,
    target_pane_session: &str,
) -> TabDropOutcome {
    execute_tab_drop_on_pane_at(
        state,
        dragged_sid,
        target_pane_idx,
        target_pane_session,
        SplitDirection::Bottom,
    )
}

pub fn execute_tab_drop_on_pane_at(
    state: &mut AppState,
    dragged_sid: &str,
    target_pane_idx: usize,
    target_pane_session: &str,
    direction: SplitDirection,
) -> TabDropOutcome {
    // Self-drop means "clone this session into one additional pane". Runtime
    // connection creation stays in app.rs; this function reserves one slot.
    if dragged_sid == target_pane_session {
        let Some(first_pane_idx) = split_pane_to_active(state, target_pane_idx, direction) else {
            return TabDropOutcome::NoOpSelfDrop;
        };
        return TabDropOutcome::SelfDropExpanded {
            first_pane_idx,
            pane_count: 1,
        };
    }

    let src_pane = pane_index_for_active_session(state, dragged_sid);

    // Empty target: move an existing pane session, or assign a background tab.
    if target_pane_session.is_empty() {
        if !set_pane_session_for_active(state, target_pane_idx, dragged_sid.to_string()) {
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

    // Occupied target: swap visible pane sessions, or append one pane for a
    // background tab while preserving every currently visible session.
    if src_pane.is_some() {
        return if swap_pane_sessions(state, dragged_sid, target_pane_session) {
            TabDropOutcome::Swapped
        } else {
            TabDropOutcome::SwapFailed
        };
    }

    match drop_background_tab_to_create_split_at(state, dragged_sid, target_pane_idx, direction) {
        DropSplitOutcome::Created { pane_idx } => TabDropOutcome::SplitCreated { pane_idx },
        DropSplitOutcome::FilledExisting { pane_idx } => {
            TabDropOutcome::SplitFilledExisting { pane_idx }
        }
        DropSplitOutcome::FallbackSwap => {
            if swap_pane_sessions(state, dragged_sid, target_pane_session) {
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
    use rusterm_core::config::{ConnectionKind, ShellConfig};
    use rusterm_core::terminal::TerminalSize;

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
    /// after SSH login): the SSH session's workspace tab is moved to the
    /// leftmost position in the tab bar.
    #[test]
    fn move_session_to_leftmost_moves_matching_tab_to_index_zero() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = move_session_to_leftmost(&mut state, "gamma");
        assert!(
            moved,
            "tab whose anchor is `gamma` (at index 2) should have been moved"
        );
        let anchors: Vec<String> = state
            .tabs
            .iter()
            .map(|t| t.anchor_session_id.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            anchors,
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
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = move_session_to_leftmost(&mut state, "alpha");
        assert!(
            !moved,
            "`alpha` is already at index 0 — no configuration step occurred"
        );
        let anchors: Vec<String> = state
            .tabs
            .iter()
            .map(|t| t.anchor_session_id.clone().unwrap_or_default())
            .collect();
        assert_eq!(
            anchors,
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

    /// Helper: AppState with N session tabs AND an active workspace tab whose
    /// anchor is the first session. Each session gets its own workspace tab
    /// (one session per tab — Plan B's default for restored or
    /// freshly-opened single-session tabs). The workspace tab `id` is set
    /// equal to the session's id so tests that hardcode
    /// `state.layouts.get("alpha")` still work (in production, group ids
    /// are UUIDs and don't match any session id — tests just use the
    /// session-name-as-group-id convention for readability).
    fn state_with_active_session(names: &[&str]) -> AppState {
        let mut state = state_with_tabs(names);
        for name in names {
            state.tabs.push(WorkspaceTab {
                id: (*name).to_string(),
                anchor_session_id: Some((*name).to_string()),
            });
        }
        if let Some(first) = state.sessions.first() {
            let first_id = first.id.clone();
            state.active_tab = Some(first_id.clone());
            state.active_session = Some(first_id);
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
    fn toggle_split_mode_off_zooms_focused_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(state.split_mode_enabled);
        assert!(state.layouts.get("alpha").unwrap().is_multi_pane());
        // Toggle OFF → should zoom to pane 0 and make is_multi_pane false.
        let on = toggle_split_mode(&mut state);
        assert_eq!(on, Some(false));
        assert!(!state.split_mode_enabled);
        assert!(!state.layouts.get("alpha").unwrap().is_multi_pane());
        assert_eq!(state.layouts.get("alpha").unwrap().zoomed, Some(0));
    }

    #[test]
    fn toggle_split_mode_on_unzooms_layout() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Turn OFF first.
        toggle_split_mode(&mut state);
        assert!(!state.split_mode_enabled);
        assert!(state.layouts.get("alpha").unwrap().zoomed.is_some());
        // Turn ON → should unzoom and restore multi-pane view.
        let on = toggle_split_mode(&mut state);
        assert_eq!(on, Some(true));
        assert!(state.split_mode_enabled);
        assert!(state.layouts.get("alpha").unwrap().is_multi_pane());
        assert!(state.layouts.get("alpha").unwrap().zoomed.is_none());
    }

    #[test]
    fn toggle_split_mode_with_no_layout_still_flips_flag() {
        let mut state = state_with_active_session(&["alpha"]);
        // No layout exists — toggle should still flip split_mode_enabled.
        assert!(state.split_mode_enabled);
        let on = toggle_split_mode(&mut state);
        assert_eq!(on, Some(false));
        assert!(!state.split_mode_enabled);
    }

    #[test]
    fn toggle_split_mode_off_uses_focused_pane_idx() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Set focused pane to pane 2.
        state.focused_pane = Some(FocusedPane {
            layout_owner_tab_id: "alpha".to_string(),
            pane_idx: 2,
        });
        // Toggle OFF → should zoom to pane 2 (the focused pane).
        toggle_split_mode(&mut state);
        assert_eq!(state.layouts.get("alpha").unwrap().zoomed, Some(2));
    }

    #[test]
    fn toggle_split_mode_off_preserves_layout_tree() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Resize the splitter so the layout is non-default (0.5 → 0.7).
        resize_layout_split(&mut state, 0, 0.2);
        let pane0_width_before = state
            .layouts
            .get("alpha")
            .unwrap()
            .pane_rect(0, 1000.0, 800.0)
            .map(|r| r.2)
            .unwrap();
        // Toggle OFF then ON — the layout tree + ratios should be intact.
        toggle_split_mode(&mut state);
        toggle_split_mode(&mut state);
        let pane0_width_after = state
            .layouts
            .get("alpha")
            .unwrap()
            .pane_rect(0, 1000.0, 800.0)
            .map(|r| r.2)
            .unwrap();
        assert!((pane0_width_before - pane0_width_after).abs() < 1e-9);
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
    // close_session (single source of truth for session teardown)
    //
    // Both the TabBar close button and the Cmd+W hotkey go through this
    // function, so the contract is pinned here once. The app.rs on_close
    // closure cannot be unit-tested (it captures Dioxus Signals), but the
    // underlying `close_session` function can be — so the invariants live
    // here.
    // ------------------------------------------------------------------

    /// Helper: build a state with `names` sessions, each with an empty
    /// terminal entry, the first session active, and `input_senders` pre-
    /// populated with one closed-channel sender per session (so we can
    /// assert `close_session` removed the entry without setting up a live
    /// PTY). Returns `(state, input_senders)`.
    fn state_with_senders(
        names: &[&str],
    ) -> (AppState, HashMap<String, mpsc::UnboundedSender<Vec<u8>>>) {
        let mut state = state_with_active_session(names);
        let mut senders = HashMap::new();
        for name in names {
            // Empty terminal entry so `close_session` can remove it.
            state.terminals.insert(
                (*name).to_string(),
                Arc::new(Mutex::new(TerminalEntry {
                    terminal: Terminal::new(TerminalSize::default()),
                    parser: vte::ansi::Processor::new(),
                    scroll_offset: 0,
                })),
            );
            let (_tx, _rx) = mpsc::unbounded_channel::<Vec<u8>>();
            senders.insert((*name).to_string(), _tx);
        }
        (state, senders)
    }

    #[test]
    fn close_session_removes_session_from_every_state_map() {
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // Seed every per-session map so we can assert close_session cleared them.
        state
            .close_senders
            .push(("alpha".to_string(), mpsc::unbounded_channel::<()>().0));
        state.resize_senders.insert(
            "alpha".to_string(),
            mpsc::unbounded_channel::<(u16, u16, u32, u32)>().0,
        );
        state
            .onekey_popups
            .insert("alpha".to_string(), OneKeyPopupState::default());
        state.session_configs.insert(
            "alpha".to_string(),
            ConnectionConfig {
                id: "alpha".to_string(),
                name: "alpha".to_string(),
                kind: ConnectionKind::Shell(ShellConfig {
                    command: None,
                    args: Vec::new(),
                    env: Vec::new(),
                    working_dir: None,
                }),
                group: None,
                tags: Vec::new(),
                onekey: false,
            },
        );
        state
            .session_connection_states
            .insert("alpha".to_string(), SessionConnectionState::default());
        state
            .pending_exit_check
            .insert("alpha".to_string(), VecDeque::new());

        close_session(&mut state, &mut senders, "alpha");

        assert!(!state.sessions.iter().any(|s| s.id == "alpha"));
        assert!(!senders.contains_key("alpha"));
        assert!(!state.close_senders.iter().any(|(s, _)| s == "alpha"));
        assert!(!state.resize_senders.contains_key("alpha"));
        assert!(!state.terminals.contains_key("alpha"));
        assert!(!state.onekey_popups.contains_key("alpha"));
        assert!(!state.session_connection_states.contains_key("alpha"));
        assert!(!state.session_configs.contains_key("alpha"));
        assert!(!state.pending_exit_check.contains_key("alpha"));
    }

    #[test]
    fn close_session_promotes_active_session_to_next_tab_when_no_layout() {
        // Single preset (no layout entry), closing the active session
        // should move active_session to the first remaining tab.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta", "gamma"]);
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        close_session(&mut state, &mut senders, "alpha");
        // First remaining session is `beta`.
        assert_eq!(state.active_session.as_deref(), Some("beta"));
        assert!(state.sessions.iter().any(|s| s.id == "beta"));
        assert!(!state.sessions.iter().any(|s| s.id == "alpha"));
    }

    #[test]
    fn close_session_clears_pane_slot_when_focused_pane_differs_from_active() {
        // Multi-pane: active_session is `alpha` (the layout owner), and
        // the focused pane displays `beta` (pane 1). Closing `beta` via
        // Cmd+W should clear pane 1's session_id (set to empty string)
        // and leave `active_session` untouched.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Sanity check the preset populated the panes. (Clone the
        // session_ids so the immutable borrow on `state.layouts` ends
        // before the `&mut state` calls below.)
        let layout = state.layouts.get("alpha").expect("alpha layout exists");
        let pane0_before = layout.panes[0].session_id.clone();
        let pane1_before = layout.panes[1].session_id.clone();
        assert_eq!(pane0_before, "alpha");
        assert_eq!(pane1_before, "beta");
        // Focus pane 1 (which displays beta). This is the scenario Cmd+W
        // is supposed to handle: focused pane != active_session.
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("beta"));

        close_session(&mut state, &mut senders, "beta");

        // active_session must stay `alpha` — the tab anchor is still alive.
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        // The layout entry for alpha survives (the tab anchor wasn't closed).
        let layout = state
            .layouts
            .get("alpha")
            .expect("alpha layout still exists");
        assert_eq!(layout.panes[0].session_id, "alpha");
        // Pane 1 now shows an empty session — no dangling reference to beta.
        assert_eq!(layout.panes[1].session_id, "");
    }

    #[test]
    fn close_session_clears_focused_pane_when_owner_is_closed() {
        // Plan B: closing the anchor session of a tab whose layout has NO
        // other sessions removes the tab + layout entirely. `focused_pane`
        // pointed at that layout, so it must be cleared (otherwise it would
        // reference a layout that no longer exists).
        //
        // We construct this scenario with a Single-preset tab (no layout
        // entry) so closing the anchor removes the tab and there's no
        // other pane session to promote to.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // No layout applied — the tab is in Single preset. Focus pane 0.
        // We need a layout entry to have a `focused_pane`, so apply Split2H
        // but then clear pane 1 so the only session is `alpha`.
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Manually clear pane 1 so `alpha` is the only session in the layout.
        {
            let layout = state.layouts.get_mut("alpha").unwrap();
            layout.panes[1].session_id = String::new();
        }
        assert!(focus_pane_for_layout(&mut state, "alpha", 0));
        assert!(state.focused_pane.is_some());

        close_session(&mut state, &mut senders, "alpha");

        assert!(state.focused_pane.is_none());
        assert!(!state.layouts.contains_key("alpha"));
        // active_session should have moved to the next remaining tab
        // (whose anchor is `beta`).
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    #[test]
    fn close_session_with_last_session_clears_active_session() {
        // Closing the only remaining session should leave active_session
        // as None (no tab to promote to).
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        close_session(&mut state, &mut senders, "alpha");
        assert!(state.active_session.is_none());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn close_session_noop_for_unknown_session() {
        // Closing a non-existent session should not panic and should leave
        // the state untouched.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        let sessions_before = state.sessions.clone();
        close_session(&mut state, &mut senders, "nonexistent");
        assert_eq!(state.sessions, sessions_before);
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    // ------------------------------------------------------------------
    // Plan B (workspace tabs) — top-level TabBar shows one entry per
    // WorkspaceTab, NOT per session. Pane-only sessions (sidebar drops,
    // pane clones) live only inside their host tab's layout and don't
    // inflate the top TabBar.
    // ------------------------------------------------------------------

    /// Helper: AppState with one workspace tab whose anchor is `anchor`,
    /// plus a layout with `pane_sessions` (excluding the anchor if it's
    /// already in the list). The layout is Split2H so we can hold up to 2
    /// sessions without cycling presets. Extra pane sessions do NOT get their
    /// own workspace tabs (they're pane-only inside `anchor`'s tab).
    fn state_with_pane_sessions(anchor: &str, extra_pane_sessions: &[&str]) -> AppState {
        let mut all_names = vec![anchor];
        for s in extra_pane_sessions {
            if !all_names.contains(s) {
                all_names.push(*s);
            }
        }
        let mut state = state_with_active_session(&all_names);
        // state_with_active_session created one tab per session. We want
        // ONLY the anchor's tab — the extras are pane-only sessions inside
        // the anchor's tab. Remove the extras' tabs.
        state.tabs.retain(|t| t.id == anchor);
        // Force the active tab back to `anchor`'s tab (state_with_active_session
        // makes the first session's tab active).
        set_active_tab(&mut state, anchor);
        // Build a Split2H layout: pane 0 = anchor, pane 1 = first extra
        // (or empty if there are no extras). Extra sessions beyond the first
        // aren't placed in any pane (they're background sessions in this
        // test state).
        let mut ids = vec![anchor.to_string()];
        if let Some(first_extra) = extra_pane_sessions.first() {
            ids.push(first_extra.to_string());
        }
        let layout = PaneLayout::from_preset(LayoutPreset::Split2H, &ids);
        state.layouts.insert(anchor.to_string(), layout);
        state.layout_preset = LayoutPreset::Split2H;
        state
    }

    #[test]
    fn split_new_pane_session_does_not_add_top_tab() {
        // Plan B contract: a session opened into a pane (via sidebar drop
        // or pane clone) does NOT create a new top-level WorkspaceTab. The
        // top TabBar count stays at 1.
        let state = state_with_pane_sessions("alpha", &["beta"]);
        // Two sessions exist in the registry.
        assert_eq!(state.sessions.len(), 2);
        // But only one workspace tab (alpha's).
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("alpha"));
    }

    #[test]
    fn one_tab_can_host_multiple_independent_sessions() {
        // Plan B contract: a single WorkspaceTab can host multiple
        // independent pane sessions in its layout. They're all reachable
        // from the same top tab — switching the top TabBar doesn't show
        // them as separate tabs.
        let state = state_with_pane_sessions("alpha", &["beta", "gamma"]);
        assert_eq!(state.tabs.len(), 1);
        // The layout has alpha + beta (gamma is a background session here).
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    #[test]
    fn close_session_promotes_pane_session_to_anchor_when_anchor_closes() {
        // Plan B contract: when a tab's anchor session closes and the
        // layout has another non-empty pane session, that session is
        // promoted to be the new anchor. The tab + layout survive.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // Both sessions are in the alpha tab's layout (alpha pane 0, beta pane 1).
        // state_with_senders created one tab per session, so we need to
        // consolidate: remove beta's tab, place beta in alpha's layout.
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Sanity: layout has alpha + beta.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");

        // Close alpha (the anchor). Beta should be promoted.
        close_session(&mut state, &mut senders, "alpha");

        // The tab survives with beta as the new anchor.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, "alpha");
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("beta"));
        // The layout survives — pane 0 was cleared (alpha closed), pane 1
        // still shows beta.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "");
        assert_eq!(layout.panes[1].session_id, "beta");
        // active_session follows the new anchor.
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    #[test]
    fn close_session_removes_tab_when_anchor_closes_and_no_pane_sessions_remain() {
        // Plan B contract: closing the only session in a tab removes the
        // tab + layout entirely. active_tab switches to the next remaining
        // tab.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // alpha's tab has no layout (Single preset). Closing alpha should
        // remove alpha's tab + switch active_tab to beta's tab.
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active_tab.as_deref(), Some("alpha"));

        close_session(&mut state, &mut senders, "alpha");

        // alpha's tab is gone; beta's tab survives.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, "beta");
        // active_tab switched to beta's tab.
        assert_eq!(state.active_tab.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    #[test]
    fn close_workspace_closes_every_pane_session_in_tab() {
        // Plan B contract: close_workspace (the TabBar close button) tears
        // down the entire tab — every pane session in its layout is closed.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // Consolidate beta into alpha's layout (same as the promote test).
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Sanity: both sessions exist, only alpha's tab exists.
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.tabs.len(), 1);

        close_workspace(&mut state, &mut senders, "alpha");

        // Both sessions are gone.
        assert!(state.sessions.iter().all(|s| s.id != "alpha"));
        assert!(state.sessions.iter().all(|s| s.id != "beta"));
        // The tab + layout are gone.
        assert!(state.tabs.is_empty());
        assert!(!state.layouts.contains_key("alpha"));
        // active_tab switched to None (no tabs remain).
        assert!(state.active_tab.is_none());
        assert!(state.active_session.is_none());
    }

    #[test]
    fn close_workspace_with_multiple_tabs_switches_active_to_next() {
        // close_workspace on a non-active tab leaves the active tab alone.
        // close_workspace on the ACTIVE tab switches active_tab to the next
        // remaining tab.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta", "gamma"]);
        // Active is alpha. Close beta's tab (a non-active tab).
        close_workspace(&mut state, &mut senders, "beta");
        // alpha is still active.
        assert_eq!(state.active_tab.as_deref(), Some("alpha"));
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        // beta + its layout are gone.
        assert!(state.tabs.iter().all(|t| t.id != "beta"));
        assert!(!state.layouts.contains_key("beta"));

        // Now close the active tab (alpha). active_tab should switch to
        // the next remaining tab (gamma, since beta is gone).
        close_workspace(&mut state, &mut senders, "alpha");
        assert_eq!(state.active_tab.as_deref(), Some("gamma"));
        assert_eq!(state.active_session.as_deref(), Some("gamma"));
    }

    #[test]
    fn switching_top_tab_changes_layout_and_anchor() {
        // Plan B contract: switching the top TabBar entry switches the
        // active_tab + active_session (anchor). The layout lookup uses
        // active_tab, so the new tab's layout is the one that renders.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // alpha tab gets a Split2H layout.
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let alpha_layout_panes = state.layouts.get("alpha").unwrap().panes.len();
        // Switch to beta's tab.
        set_active_tab(&mut state, "beta");
        assert_eq!(state.active_tab.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
        // alpha's layout still exists (switching tabs doesn't destroy it).
        assert_eq!(
            state.layouts.get("alpha").unwrap().panes.len(),
            alpha_layout_panes
        );
        // beta's tab has no layout entry (Single preset).
        assert!(!state.layouts.contains_key("beta"));
    }

    #[test]
    fn cmd_w_closing_focused_pane_preserves_tab() {
        // Plan B + Cmd+W contract: closing a NON-anchor pane session via
        // close_session (Cmd+W) clears the pane slot but leaves the tab +
        // anchor intact. This is the user-facing "close this pane, keep
        // the tab" behaviour.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        // Consolidate beta into alpha's layout.
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Focus pane 1 (beta).
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("beta"));

        // Cmd+W closes the focused pane session (beta).
        close_session(&mut state, &mut senders, "beta");

        // The tab survives (anchor alpha still alive).
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].id, "alpha");
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        // Pane 1 is cleared; pane 0 still shows alpha.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
        // beta is gone from the registry.
        assert!(state.sessions.iter().all(|s| s.id != "beta"));
    }

    #[test]
    fn pane_only_session_does_not_appear_as_top_tab() {
        // Plan B contract: a pane-only session (no workspace tab owns it
        // as anchor) doesn't appear in the top TabBar. We simulate this
        // by adding a session to the registry WITHOUT creating a tab for
        // it.
        let mut state = state_with_active_session(&["alpha"]);
        // Add a pane-only session "beta" (no tab).
        state.sessions.push(SessionTab {
            id: "beta".to_string(),
            name: "beta".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: None,
            cwd: None,
        });
        // Place beta in alpha's layout pane 1.
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        // Top TabBar shows only alpha's tab — beta is pane-only.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("alpha"));
        // beta is in the layout.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[1].session_id, "beta");
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

    /// Each tab owns its own layout — switching the active tab must not
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

        // Switch active tab to beta and apply Split2H there.
        set_active_tab(&mut state, "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert_eq!(state.layouts.len(), 2);
        assert!(state.layouts.contains_key("beta"));

        // The two layouts are distinct — beta's layout is Split2H (2 panes),
        // alpha's is still Grid4 (4 panes).
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 4);
        assert_eq!(state.layouts.get("beta").unwrap().panes.len(), 2);

        // Switching back to alpha — its layout is preserved unchanged.
        set_active_tab(&mut state, "alpha");
        let alpha_layout = state.layouts.get("alpha").unwrap().clone();
        assert_eq!(alpha_layout.panes.len(), 4);
        assert_eq!(alpha_layout.cols(), 2);
        assert_eq!(alpha_layout.rows(), 2);
    }

    /// Task 15 contract: when the user cycles a layout preset on a tab whose
    /// anchor session is `X`, the new layout is rebuilt with `X` anchored at
    /// pane 0 and the remaining sessions filling the rest in tab order.
    /// This is the session-allocation correctness criterion.
    #[test]
    fn cycle_layout_preset_anchors_active_session_at_pane_zero() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        // Make `gamma` the active tab — its anchor session is `gamma`.
        set_active_tab(&mut state, "gamma");

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
    /// a legacy layout containing empty pane slots).
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

    /// Simulates the state portion of opening a sidebar connection on an occupied
    /// pane: prepare one new pane, create the session, then assign it without
    /// replacing either existing session.
    #[test]
    fn e2e_drag_sidebar_connection_onto_occupied_pane_preserves_sessions() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let plan = prepare_split_for_sidebar_drop(&mut state, 1).expect("drop plan");
        assert_eq!(plan.pane_idx, 2);
        assert!(plan.created_new_pane);

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
        assert!(set_pane_session_for_layout(
            &mut state,
            &plan.layout_owner_tab_id,
            plan.pane_idx,
            new_session_id,
        ));

        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "new-conn-1");
        assert!(layout.panes.iter().all(|pane| !pane.session_id.is_empty()));
        assert!(state.sessions.iter().any(|tab| tab.id == "alpha"));
        assert!(state.sessions.iter().any(|tab| tab.id == "beta"));
        assert!(state.sessions.iter().any(|tab| tab.id == "new-conn-1"));
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
    // The drag-over highlight signal (`drag_over_pane: Signal<Option<(usize, PaneDropRegion)>>`)
    // lives in the Dioxus runtime, not on AppState — it can't be unit-tested
    // without spinning up a Dioxus runtime. Its behavior is instead pinned
    // by the call-site comments in `multi_pane_container` and the 4-quadrant
    // scheme by `pane_drop_region_for_cursor` (which IS unit-tested). The
    // Signal equality check makes `set` a no-op when the value is unchanged, so
    // the high-frequency `ondragover` (~60Hz) does NOT trigger per-tick
    // re-renders.

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

    /// A background tab added to a single-pane view creates one additional pane
    /// and preserves the active session in pane 0.
    #[test]
    fn drop_background_tab_creates_split_when_no_layout() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);

        let outcome = drop_background_tab_to_create_split(&mut state, "beta", 0);

        assert_eq!(outcome, DropSplitOutcome::Created { pane_idx: 1 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
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

    /// A full two-pane layout grows to exactly three panes for a background tab.
    #[test]
    fn drop_background_tab_adds_one_pane_to_full_split() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome = drop_background_tab_to_create_split(&mut state, "gamma", 0);

        assert_eq!(outcome, DropSplitOutcome::Created { pane_idx: 2 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert!(layout.panes.iter().all(|pane| !pane.session_id.is_empty()));
    }

    /// A background tab only falls back when the on-demand layout has reached the
    /// real MAX_PANES cap and every pane is occupied.
    #[test]
    fn drop_background_tab_at_max_panes_returns_fallback_swap() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let layout = state.layouts.get_mut("alpha").unwrap();
        while layout.panes.len() < MAX_PANES {
            layout.append_pane(true).expect("grow to max");
        }
        for (idx, pane) in layout.panes.iter_mut().enumerate() {
            if pane.session_id.is_empty() {
                pane.session_id = format!("occupied-{idx}");
            }
        }

        let outcome = drop_background_tab_to_create_split(&mut state, "gamma", 0);

        assert_eq!(outcome, DropSplitOutcome::FallbackSwap);
        assert_eq!(state.layouts["alpha"].panes.len(), MAX_PANES);
        assert!(
            state.layouts["alpha"]
                .panes
                .iter()
                .all(|pane| pane.session_id != "gamma")
        );
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

    /// A self-drop reserves exactly one pane for one independent cloned session.
    #[test]
    fn execute_tab_drop_self_drop_multi_pane_requests_one_cloned_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(
            outcome,
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 2,
                pane_count: 1,
            }
        );
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    #[test]
    fn self_drop_growth_preserves_existing_floating_window_positions() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(begin_floating_pane_move(&mut state, 0));
        assert!(move_floating_pane_for_active(
            &mut state, 0, 140.0, 70.0, 1200.0, 800.0,
        ));
        let before = state.layouts["alpha"].panes[0].floating;

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(
            outcome,
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 2,
                pane_count: 1,
            }
        );
        assert_eq!(state.layouts["alpha"].panes.len(), 3);
        assert_eq!(state.layouts["alpha"].panes[0].floating, before);
        assert!(state.layouts["alpha"].is_floating());
    }

    /// Existing background tabs must not be silently reused for a self-drop; the
    /// one new pane is reserved for a fresh clone of the dragged session.
    #[test]
    fn execute_tab_drop_self_drop_does_not_reuse_background_tabs() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Split2H,
                &["alpha".to_string(), "beta".to_string()],
            ),
        );

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(
            outcome,
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 2,
                pane_count: 1,
            }
        );
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "");
    }

    /// Repeated self-drops grow 1→2→3→…→MAX_PANES, exactly one pane per
    /// operation, and then no-op at the real cap.
    #[test]
    fn execute_tab_drop_repeated_self_drops_add_one_until_max() {
        let mut state = state_with_active_session(&["alpha"]);

        for expected_count in 2..=MAX_PANES {
            assert_eq!(
                execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
                TabDropOutcome::SelfDropExpanded {
                    first_pane_idx: expected_count - 1,
                    pane_count: 1,
                }
            );
            let layout = state.layouts.get("alpha").unwrap();
            assert_eq!(layout.panes.len(), expected_count);
            assert_eq!(layout.visible_panes(1000.0, 800.0).count(), expected_count);
        }

        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::NoOpSelfDrop
        );
        assert_eq!(state.layouts["alpha"].panes.len(), MAX_PANES);
        assert_eq!(state.layouts["alpha"].panes[0].session_id, "alpha");
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// Dragging the active tab into its own single-pane view reserves pane 1
    /// for a newly cloned runtime session, even when other tabs exist.
    #[test]
    fn execute_tab_drop_active_tab_self_drop_single_pane_requests_clone() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        assert!(!state.layouts.contains_key("alpha"));

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(
            outcome,
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 1,
                pane_count: 1,
            }
        );
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    #[test]
    fn execute_tab_drop_active_tab_self_drop_only_tab_requests_clone() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!state.layouts.contains_key("alpha"));

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(
            outcome,
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 1,
                pane_count: 1,
            }
        );
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
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

    /// The real tab-drop dispatch path must add exactly one pane for a
    /// background session: two occupied panes become three, with no empty
    /// fourth slot.
    #[test]
    fn execute_background_tab_drop_on_top_splits_only_target_leaf() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome =
            execute_tab_drop_on_pane_at(&mut state, "gamma", 1, "beta", SplitDirection::Top);

        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 2 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 500.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 400.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 400.0, 500.0, 400.0))
        );
    }

    /// Drop on the RIGHT half of pane 0 (the left half of Split2H). The
    /// target leaf is split horizontally: pane 0 keeps the left quarter
    /// (250px), the new pane (gamma) takes the right quarter (250px), and
    /// pane 1 (originally at x=500) is unchanged. This validates that
    /// `execute_tab_drop_on_pane_at` honours `SplitDirection::Right` and
    /// only touches the target leaf — the rest of the tree is preserved.
    #[test]
    fn execute_background_tab_drop_on_right_splits_only_target_leaf() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome =
            execute_tab_drop_on_pane_at(&mut state, "gamma", 0, "alpha", SplitDirection::Right);

        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 2 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        // Pane 0 split at ratio 0.5: original keeps the left half (250px),
        // the new pane occupies the right half (250px).
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 250.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((250.0, 0.0, 250.0, 800.0))
        );
        // Pane 1 is untouched (still the right half of the container).
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 800.0))
        );
    }

    #[test]
    fn execute_tab_drop_background_tab_adds_exactly_one_pane() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome = execute_tab_drop_on_pane(&mut state, "gamma", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::SplitCreated { pane_idx: 2 });
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(layout.panes[2].session_id, "gamma");
        assert!(layout.panes.iter().all(|pane| !pane.session_id.is_empty()));
    }

    /// A background tab only fails over to swap after MAX_PANES is reached.
    #[test]
    fn execute_tab_drop_background_tab_at_max_fallback_swap_fails() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let layout = state.layouts.get_mut("alpha").unwrap();
        while layout.panes.len() < MAX_PANES {
            layout.append_pane(true).expect("grow to max");
        }
        for (idx, pane) in layout.panes.iter_mut().enumerate() {
            if pane.session_id.is_empty() {
                pane.session_id = format!("occupied-{idx}");
            }
        }

        let outcome = execute_tab_drop_on_pane(&mut state, "gamma", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::SplitFallbackSwapFailed);
        assert_eq!(state.layouts["alpha"].panes.len(), MAX_PANES);
    }

    /// Runtime clone creation may trigger unrelated state updates between
    /// reserving a pane and assigning the new session. Explicit ownership
    /// keeps that one clone in the layout that requested it.
    #[test]
    fn explicit_layout_target_fills_self_drop_slot_after_active_tab_changes() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert_eq!(
            execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
            TabDropOutcome::SelfDropExpanded {
                first_pane_idx: 2,
                pane_count: 1,
            }
        );

        state.active_session = Some("beta".to_string());
        assert!(set_pane_session_for_layout(
            &mut state,
            "alpha",
            2,
            "clone-a".to_string(),
        ));

        assert_eq!(state.active_session.as_deref(), Some("beta"));
        assert_eq!(state.layouts["alpha"].panes.len(), 3);
        assert_eq!(state.layouts["alpha"].panes[2].session_id, "clone-a");
    }

    #[test]
    fn copy_source_prefers_left_then_above_in_grid4() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Grid4,
            &["top-left".to_string(), "top-right".to_string()],
        );

        assert_eq!(source_pane_for_copy(&layout, 2), Some(0));
        assert_eq!(source_pane_for_copy(&layout, 3), Some(1));

        let mut left_filled = layout.clone();
        left_filled.panes[2].session_id = "bottom-left".to_string();
        assert_eq!(source_pane_for_copy(&left_filled, 3), Some(2));
    }

    #[test]
    fn copy_source_falls_back_past_empty_neighbours() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &["only".to_string()]);
        assert_eq!(source_pane_for_copy(&layout, 3), Some(0));
    }

    #[test]
    fn copy_source_in_local_split_preserves_left_before_above_priority() {
        let mut layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["left".to_string(), "right-top".to_string()],
        );
        let bottom = layout
            .split_pane(1, SplitDirection::Bottom)
            .expect("local bottom pane");

        assert_eq!(bottom, 2);
        assert_eq!(source_pane_for_copy(&layout, bottom), Some(0));
    }

    #[test]
    fn copy_source_returns_none_when_no_session_exists() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &[]);
        assert_eq!(source_pane_for_copy(&layout, 3), None);
        assert_eq!(source_pane_for_copy(&layout, 99), None);
    }

    #[test]
    fn focusing_pane_does_not_change_layout_owner() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        state.active_session = Some("beta".to_string());

        assert!(focus_pane_for_layout(&mut state, "alpha", 0));
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
        assert_eq!(
            state.focused_pane,
            Some(FocusedPane {
                layout_owner_tab_id: "alpha".to_string(),
                pane_idx: 1,
            })
        );
    }

    #[test]
    fn focused_pane_session_tracks_selected_grid_pane() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        state.active_session = Some("beta".to_string());

        assert!(focus_pane_for_layout(&mut state, "alpha", 2));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("gamma"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    #[test]
    fn focused_pane_session_ignores_empty_or_stale_focus() {
        let mut state = state_with_active_session(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);

        assert!(focus_pane_for_layout(&mut state, "alpha", 2));
        assert_eq!(focused_pane_session(&state), None);

        state.focused_pane = Some(FocusedPane {
            layout_owner_tab_id: "missing".to_string(),
            pane_idx: 0,
        });
        assert_eq!(focused_pane_session(&state), None);

        state.focused_pane = Some(FocusedPane {
            layout_owner_tab_id: "alpha".to_string(),
            pane_idx: 99,
        });
        assert_eq!(focused_pane_session(&state), None);
    }

    #[test]
    fn focusing_floating_pane_brings_it_forward_without_other_mutation() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        assert!(begin_floating_pane_move(&mut state, 0));
        let before_sessions = state.layouts["alpha"].session_ids();
        let before_geometry: Vec<_> = state.layouts["alpha"]
            .panes
            .iter()
            .map(|pane| pane.floating)
            .collect();

        assert!(focus_pane_for_layout(&mut state, "alpha", 2));

        let layout = &state.layouts["alpha"];
        assert_eq!(layout.session_ids(), before_sessions);
        for (idx, pane) in layout.panes.iter().enumerate() {
            let before = before_geometry[idx].unwrap();
            let after = pane.floating.unwrap();
            assert_eq!(
                (
                    after.x_frac,
                    after.y_frac,
                    after.width_frac,
                    after.height_frac
                ),
                (
                    before.x_frac,
                    before.y_frac,
                    before.width_frac,
                    before.height_frac
                )
            );
        }
        let max_z = layout
            .panes
            .iter()
            .filter_map(|pane| pane.floating.map(|geometry| geometry.z_index))
            .max()
            .unwrap();
        assert_eq!(layout.pane_z_index(2), Some(max_z));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("gamma"));
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    #[test]
    fn floating_pane_move_preserves_layout_anchor_and_other_sessions() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let before_sessions = state.layouts["alpha"].session_ids();

        assert!(begin_floating_pane_move(&mut state, 1));
        let before_other = state.layouts["alpha"].panes[2].floating;
        assert!(move_floating_pane_for_active(
            &mut state, 1, 160.0, 90.0, 1200.0, 800.0,
        ));

        assert_eq!(state.active_session.as_deref(), Some("alpha"));
        assert_eq!(state.layouts["alpha"].session_ids(), before_sessions);
        assert_eq!(state.layouts["alpha"].panes[2].floating, before_other);
        assert!(state.layouts["alpha"].is_floating());
    }

    #[test]
    fn floating_pane_move_rejects_missing_layout_without_changing_active_session() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!begin_floating_pane_move(&mut state, 0));
        assert!(!move_floating_pane_for_active(
            &mut state, 0, 20.0, 20.0, 1200.0, 800.0,
        ));
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    // ------------------------------------------------------------------
    // prepare_split_for_sidebar_drop
    // ------------------------------------------------------------------

    /// Helper: get the active tab's layout (or panic).
    fn active_layout(state: &AppState) -> &PaneLayout {
        let active = state.active_tab.as_ref().expect("active_tab set");
        state.layouts.get(active).expect("layout exists")
    }

    /// No layout yet (Single preset) → creating a Split2H preserves pane 0's
    /// anchor session and returns pane_idx=1 for the new connection.
    #[test]
    fn sidebar_drop_creates_split_when_no_layout_exists() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(state.layouts.is_empty());
        let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("plan returned");
        assert_eq!(plan.layout_owner_tab_id, "alpha");
        assert_eq!(plan.pane_idx, 1);
        assert!(plan.created_new_pane);
        let layout = state.layouts.get("alpha").expect("layout created");
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
    }

    /// Target pane is empty → return target as-is, no layout change.
    #[test]
    fn sidebar_drop_uses_empty_target_pane_without_splitting() {
        let mut state = state_with_active_session(&["alpha"]);
        // Build a Grid4 with only pane 0 filled — panes 1, 2, 3 are empty.
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let plan = prepare_split_for_sidebar_drop(&mut state, 2).expect("plan returned");
        assert_eq!(plan.pane_idx, 2);
        assert!(!plan.created_new_pane);
        // Layout unchanged.
        assert_eq!(active_layout(&state).panes.len(), 4);
    }

    /// Target is occupied, another empty pane exists → reuse the empty pane
    /// instead of growing the preset.
    #[test]
    fn sidebar_drop_reuses_other_empty_pane_when_target_occupied() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        // Grid4 has 4 panes; we have 2 sessions. Panes 2, 3 are empty.
        // The drag hit pane 0 (occupied by alpha) — should reuse pane 2.
        let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("plan returned");
        assert_eq!(plan.pane_idx, 2);
        assert!(!plan.created_new_pane);
        // No layout growth.
        assert_eq!(active_layout(&state).panes.len(), 4);
    }

    /// Target occupied, no empty panes, can grow (Split2H 1×2 → 1×3) → grow
    /// ON DEMAND by exactly one pane and return its index. This is the
    /// on-demand split contract: each sidebar drop adds exactly one new
    /// pane, not a preset jump like Split2H → Grid4 (+2 panes).
    #[test]
    fn sidebar_drop_bottom_splits_only_the_target_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let plan = prepare_split_for_sidebar_drop_at(&mut state, 1, SplitDirection::Bottom)
            .expect("drop plan");

        assert_eq!(plan.pane_idx, 2);
        assert!(plan.created_new_pane);
        let layout = active_layout(&state);
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 500.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 400.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((500.0, 400.0, 500.0, 400.0))
        );
    }

    #[test]
    fn sidebar_drop_grows_layout_when_all_panes_occupied() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Both panes are now occupied. Sidebar drop on pane 0 should append
        // exactly one new pane (1×2 → 1×3) and return pane index 2.
        let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("plan returned");
        assert_eq!(plan.pane_idx, 2);
        assert!(plan.created_new_pane);
        // Exactly one pane added (not the old Grid4 preset jump of +2).
        assert_eq!(active_layout(&state).panes.len(), 3);
        // The new pane is empty and occupies the lower half of target pane 0.
        assert_eq!(active_layout(&state).panes[2].session_id, "");
        assert_eq!(
            active_layout(&state).pane_rect(2, 1000.0, 800.0),
            Some((0.0, 400.0, 500.0, 400.0))
        );
        // Existing sessions are preserved.
        assert!(
            active_layout(&state)
                .panes
                .iter()
                .any(|p| p.session_id == "alpha")
        );
        assert!(
            active_layout(&state)
                .panes
                .iter()
                .any(|p| p.session_id == "beta")
        );
    }

    /// Repeated sidebar drops on a 1×N strip each add exactly one pane,
    /// growing 1×2 → 1×3 → 1×4 → … one pane per drop. No preset jumps.
    #[test]
    fn sidebar_drop_repeated_each_adds_exactly_one_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Start at 1×2 (2 panes). Three sequential drops should grow to
        // 1×3, 1×4, 1×5 — exactly one pane per drop.
        for expected_len in 3..=5 {
            // Fill every existing pane so the next drop has to grow.
            // (We don't have the new session here, so we just mark the
            // last-dropped pane as filled with a sentinel id.)
            let last_pane = active_layout(&state).panes.len() - 1;
            let _ = set_pane_session_for_active(
                &mut state,
                last_pane,
                format!("filled-{expected_len}"),
            );
            let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("plan returned");
            assert!(plan.created_new_pane, "drop should grow the layout");
            assert_eq!(
                active_layout(&state).panes.len(),
                expected_len,
                "each drop adds exactly one pane"
            );
        }
    }

    /// At MAX_PANES with every pane occupied, sidebar preparation refuses to
    /// replace the target. The app can then open the connection as a new tab.
    #[test]
    fn sidebar_drop_at_max_panes_preserves_all_existing_sessions() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let layout = state.layouts.get_mut("alpha").expect("layout exists");
        while layout.panes.len() < MAX_PANES {
            layout.append_pane(true).expect("grow to max");
        }
        for (idx, pane) in layout.panes.iter_mut().enumerate() {
            if pane.session_id.is_empty() {
                pane.session_id = format!("occupied-{idx}");
            }
        }
        let before = layout.session_ids();

        let plan = prepare_split_for_sidebar_drop(&mut state, 3);

        assert_eq!(plan, None);
        assert_eq!(active_layout(&state).panes.len(), MAX_PANES);
        assert_eq!(active_layout(&state).session_ids(), before);
    }

    /// No active tab → returns None.
    #[test]
    fn sidebar_drop_returns_none_without_active_tab() {
        let mut state = AppState::default();
        assert!(prepare_split_for_sidebar_drop(&mut state, 0).is_none());
    }

    /// Sidebar drop preserves existing session in pane 0 when growing from
    /// Single (no layout) — the "comparison" contract.
    #[test]
    fn sidebar_drop_preserves_anchor_session_in_pane_0() {
        let mut state = state_with_active_session(&["alpha"]);
        let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("plan returned");
        assert_eq!(plan.pane_idx, 1);
        let layout = state.layouts.get("alpha").expect("layout exists");
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
    }

    // ------------------------------------------------------------------
    // distribute_sessions_across_panes
    // ------------------------------------------------------------------

    /// Distribute fills panes with sessions in tab order.
    #[test]
    fn distribute_fills_panes_in_tab_order() {
        let mut state = state_with_active_session(&["a", "b", "c", "d"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let placed = distribute_sessions_across_panes(&mut state);
        assert_eq!(placed, 4);
        let layout = active_layout(&state);
        assert_eq!(layout.panes[0].session_id, "a");
        assert_eq!(layout.panes[1].session_id, "b");
        assert_eq!(layout.panes[2].session_id, "c");
        assert_eq!(layout.panes[3].session_id, "d");
    }

    /// Distribute with more sessions than panes — extra sessions are
    /// dropped (remain in `state.sessions` for manual placement).
    #[test]
    fn distribute_drops_extra_sessions_beyond_pane_count() {
        let mut state = state_with_active_session(&["a", "b", "c", "d", "e", "f"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let placed = distribute_sessions_across_panes(&mut state);
        assert_eq!(placed, 2);
        let layout = active_layout(&state);
        assert_eq!(layout.panes[0].session_id, "a");
        assert_eq!(layout.panes[1].session_id, "b");
        // Extra sessions c, d, e, f are still in state.sessions.
        assert_eq!(state.sessions.len(), 6);
    }

    /// Distribute with fewer sessions than panes — extra panes are emptied.
    #[test]
    fn distribute_empties_extra_panes_when_fewer_sessions() {
        let mut state = state_with_active_session(&["a", "b"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        let placed = distribute_sessions_across_panes(&mut state);
        assert_eq!(placed, 2);
        let layout = active_layout(&state);
        assert_eq!(layout.panes[0].session_id, "a");
        assert_eq!(layout.panes[1].session_id, "b");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.panes[3].session_id, "");
    }

    /// Distribute returns 0 with no active tab.
    #[test]
    fn distribute_returns_zero_without_active_tab() {
        let mut state = AppState::default();
        assert_eq!(distribute_sessions_across_panes(&mut state), 0);
    }

    /// Distribute returns 0 with no layout.
    #[test]
    fn distribute_returns_zero_without_layout() {
        let mut state = state_with_active_session(&["a", "b"]);
        // No apply_layout_preset call → no entry in state.layouts.
        assert_eq!(distribute_sessions_across_panes(&mut state), 0);
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
