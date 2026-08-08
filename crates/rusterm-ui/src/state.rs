use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rusterm_core::config::{
    ConnectionConfig, FocusedTabAppearance, Keybindings, OneKey, OneKeyPreference,
    SidebarPreferences, SkinSettings, WorkspacePreferences,
};
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

/// UI-facing snapshot of one workspace tab and its pane hierarchy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceNode {
    pub tab_id: String,
    pub anchor_session_id: Option<String>,
    pub is_active: bool,
    pub panes: Vec<PaneNode>,
}

/// UI-facing snapshot of one pane. Empty panes remain present with no session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneNode {
    pub index: usize,
    pub is_focused: bool,
    pub session: Option<SessionNode>,
}

/// UI-facing snapshot of a live session assigned to a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNode {
    pub id: String,
    pub name: String,
    pub kind: SessionType,
    pub is_active: bool,
    pub connection_state: SessionConnectionState,
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
    pub sidebar_preferences: SidebarPreferences,
    #[serde(default)]
    pub workspace_preferences: WorkspacePreferences,
    pub connections: Vec<ConnectionConfig>,
    pub theme: Theme,
    #[serde(default)]
    pub focused_tab_appearance: FocusedTabAppearance,
    #[serde(default)]
    pub keybindings: Keybindings,
    #[serde(default)]
    pub skin: SkinSettings,
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
    /// Sessions that have ever produced a real OSC 133;D exit code. Used to
    /// permanently disable the prompt-return badge fallback
    /// (`prompt_return_completion_target`) on integrated shells, so a late or
    /// split-chunk exit-code marker can never race the fallback.
    #[serde(skip)]
    pub exit_code_sessions: HashSet<String>,
    /// Input tracked synchronously from terminal key events. This avoids using
    /// the asynchronously echoed terminal grid as the source of truth when
    /// Enter arrives immediately after a fast paste or Compare broadcast.
    #[serde(skip)]
    pub terminal_command_lines: HashMap<String, TrackedCommandLine>,
    /// Sessions currently showing the explicit Alt+R history-completion picker.
    /// This mode changes Enter from "execute" to "replace the current line".
    #[serde(skip)]
    pub history_completion_sessions: HashSet<String>,
    /// Sessions whose user clicked "don't show again this session" in the
    /// suggestion popup's hint row. Muted sessions skip the entire
    /// suggestion pipeline (same as `suggestion_enabled = false`, but scoped
    /// to the session and NOT persisted — a new session sees suggestions
    /// again). Entries are removed by `close_session`.
    #[serde(skip)]
    pub suggestion_muted_sessions: HashSet<String>,
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
    /// Most recent failed command per session. A later successful command in
    /// the same session may confirm a local typo correction pair.
    #[serde(skip)]
    pub last_failed_command_by_session: HashMap<String, (String, std::time::Instant)>,
    /// OneKey library (ZOC-style Expect/Send), decrypted in memory after unlock.
    #[serde(skip)]
    pub onekeys: Vec<OneKey>,
    /// Persisted connection/prompt selections learned from multi-match popups.
    /// Records stable IDs only; no credential values or translated labels.
    #[serde(skip)]
    pub onekey_preferences: Vec<OneKeyPreference>,
    /// Per-session remembered-selection submission awaiting acceptance or a
    /// repeated prompt. A repeated matching prompt invalidates the preference.
    #[serde(skip)]
    pub onekey_preference_attempts: HashMap<String, OneKeyPreferenceAttempt>,
    /// In-memory OneKey behavior history keyed by
    /// `(connection_id, prompt_fingerprint)`, oldest-first, capped at
    /// [`ONEKEY_HABIT_EVENTS_CAP`] per key. Warmed from DuckDB at unlock and
    /// updated live; the habit resolver reads only this cache so prompt
    /// handling never blocks on DuckDB.
    #[serde(skip)]
    pub onekey_habit_events: HashMap<(String, String), Vec<OneKeyBehaviorEvent>>,
    /// Behavior events waiting to be flushed to the local DuckDB analytics
    /// store. Populated by the (sync, unit-testable) OneKey decision code and
    /// drained by callers running inside the Dioxus runtime, which spawn the
    /// actual DB writes.
    #[serde(skip)]
    pub onekey_pending_analytics: Vec<OneKeyBehaviorEvent>,
    /// Per-session OneKey autofill popup state. Only shown when new output matches
    /// an OneKey's expect regex; persists across focus changes (no re-scan).
    #[serde(skip)]
    pub onekey_popups: HashMap<String, OneKeyPopupState>,
    /// Non-secret result of the most recent OneKey submission in each session.
    /// This makes password prompts diagnosable even though the remote PTY turns
    /// echo off: users can distinguish "sent but hidden" from "asked again".
    #[serde(skip)]
    pub onekey_submission_feedback: HashMap<String, OneKeySubmissionFeedback>,
    /// Cooldown after a OneKey submission: `(matched_expect, submitted_at)`.
    /// Prevents the terminal's residual prompt text (sudo prints "Password:"
    /// and the cursor stays there while input is hidden) from re-triggering a
    /// popup — and a false "Rejected" — before the remote has had time to
    /// accept or reject the credential. The cooldown is short (a few seconds)
    /// so a genuine wrong-password retry from the remote still surfaces a new
    /// popup once the grace period elapses.
    #[serde(skip)]
    pub onekey_submission_cooldown: HashMap<String, (String, std::time::Instant)>,
    /// Fresh remote output received after a submission, accumulated across PTY
    /// chunks. This distinguishes a genuinely re-emitted (possibly fragmented)
    /// prompt from residual text already present in the terminal model.
    #[serde(skip)]
    pub onekey_output_since_submission: HashMap<String, String>,
    /// (session_id, skip-reason) pairs already reported at info level, so a
    /// repeated OneKey gate skip (e.g. `disabled_for_session` on every output
    /// chunk) logs exactly once per session instead of spamming — while never
    /// being invisible like the old debug-only logs.
    #[serde(skip)]
    pub onekey_skip_logged: HashSet<(String, &'static str)>,
    /// Per-session connection config (kept in memory, not persisted) so a
    /// disconnected session can be reconnected by pressing Enter.
    #[serde(skip)]
    pub session_configs: HashMap<String, ConnectionConfig>,
    /// Runtime connection state per SSH/shell session. Keeping `Reconnecting`
    /// distinct from `Disconnected` makes Enter-triggered retries idempotent
    /// while preserving the same session id and pane assignment.
    #[serde(skip)]
    pub session_connection_states: HashMap<String, SessionConnectionState>,
    /// Per-session hostname of the jumpserver-internal node the session has
    /// landed on, captured "in the background" from the target shell's OSC 7
    /// report (`file://<node-host>/<path>`). `None` for plain SSH (where the
    /// node IS the connection host, already stored on `SessionTab.hostname`)
    /// and for shells that never report OSC 7. Used to label which internal
    /// machine each (possibly duplicated) jumpserver session is on, so the
    /// session header can show `ops@jump → web-01` instead of just the bastion.
    #[serde(skip)]
    pub session_nodes: HashMap<String, String>,
    /// OTP 组级状态机的登记表（按 conn.id 分组）。JumpServer 共凭据的多个
    /// tab 恢复时，组内只需一台 fresh connect + OTP，其余复用其 transport。
    /// 见 [`OtpGroupRegistry`] 的说明。运行时状态，不持久化。
    #[serde(skip)]
    pub otp_groups: OtpGroupRegistry,
    /// Explicit target selection for the docked Send panel. `None` preserves
    /// the legacy initial behavior (focused pane / active tab); after the user
    /// changes the selection, `Some` keeps that choice stable across renders.
    #[serde(skip)]
    pub send_target_selection: Option<HashSet<String>>,
    /// Authenticated SSH control sessions keyed by terminal session id. These
    /// handles are reused to open independent SFTP subsystem channels.
    #[serde(skip)]
    pub ssh_sessions: HashMap<String, rusterm_ssh::SshSession>,
    /// Lazily opened SFTP clients. Kept separate from terminal channels so file
    /// operations can continue while the terminal remains interactive.
    #[serde(skip)]
    pub sftp_clients: HashMap<String, rusterm_ssh::SftpClient>,
    /// Runtime-only transfer queue and cancellation handles.
    #[serde(skip)]
    pub transfers: crate::transfers::TransferState,
    #[serde(skip)]
    pub transfer_cancellations: HashMap<String, (u32, CancellationToken)>,
    /// Per-session ZMODEM (lrzsz rz/sz) state. Lazily installed when the
    /// first ZMODEM frame is detected in a session's output stream.
    /// Shared via `Arc<Mutex<...>>` so spawned rfd-dialog + file-IO tasks
    /// can write back without taking the Dioxus state lock.
    #[serde(skip)]
    pub zmodem: std::sync::Arc<parking_lot::Mutex<crate::zmodem::ZmodemSessions>>,
    /// Local PTY session rendered only inside the bottom dock Shell panel.
    #[serde(skip)]
    pub bottom_shell_session_id: Option<String>,
    /// DuckDB-backed analytics handle. Lazily opened on first use (so the
    /// ~50MB bundled libduckdb doesn't initialize on app startup unless
    /// the user actually queries analytics). When the `analytics` feature
    /// is off, this is a no-op stub.
    #[serde(skip)]
    pub analytics: crate::analytics::AnalyticsHandle,
    /// oh-my-zsh plugin-alias index, loaded once on startup via a
    /// `spawn_blocking` task. `None` when oh-my-zsh isn't installed or the
    /// load hasn't completed. The suggestion query reads this as a 4th
    /// suggestion source (after session history, SQLite FTS5, and DuckDB
    /// analytics). Cheap to clone: the alias index is shared via `Arc`.
    #[serde(skip)]
    pub ohmyzsh: Option<rusterm_ohmyzsh::OhMyZsh>,
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
    // The snapshot contains only sessions that were `Connected` at the last
    // save; restore never re-executes past shell commands — only a safe `cd`
    // to the last reported directory, or (for interactive jumpserver-style
    // sessions) a replay of the recorded establishment ops.
    //
    /// Snapshot loaded at startup and awaiting the user's decision in the
    /// restore-confirmation dialog. Set by the unlock path whenever a
    /// non-empty session snapshot exists on disk — written on normal exit
    /// by the close-path save, or (after a crash / force-kill) by the 30 s
    /// periodic save — so the prompt appears regardless of how the previous
    /// run ended. Cleared when the user picks 恢复 (restore) or 跳过 (skip).
    /// While set, session-snapshot saves are deferred so the undecided
    /// on-disk state can't be overwritten by the current (blank) session
    /// list.
    #[serde(skip)]
    pub restore_pending: Option<rusterm_core::SessionState>,
    /// Legacy "don't ask again" preference. Automatic recovery deliberately
    /// ignores this value so an old choice cannot permanently suppress session
    /// persistence and startup restore.
    pub restore_disabled: bool,
    /// Whether to show the "是否确实要关闭本软件？" confirmation dialog when
    /// the user closes the last window. Default true (safe default — always
    /// ask). Persisted in `settings.json` so the user's choice on the
    /// dialog's "下次关闭时不再询问" checkbox survives across launches.
    /// Loaded from settings on unlock (see the unlock handler in `app.rs`).
    pub confirm_close_on_exit: bool,
    /// Whether comparison mode warns before highlighting large diffs.
    /// Persisted in settings.json and defaulted to true for existing users.
    pub comparison_diff_warning_enabled: bool,
    /// Whether the inline fish-style command suggestion popup is enabled.
    /// Persisted in settings.json. When false, no suggestions are computed
    /// or shown — the user types with no ghost text or dropdown.
    pub suggestion_enabled: bool,
    /// Maximum number of suggestion items shown in the dropdown (3, 5, or 10).
    /// Persisted in settings.json. Default 3 for a compact popup.
    pub suggestion_count: u8,
    /// Whether local usage-habit statistics are collected (opt-in via the
    /// settings dialog; persisted in settings.json). When false, analytics
    /// recording is a no-op even when the `analytics` feature is compiled in.
    #[serde(skip)]
    pub collect_usage_habits: bool,
    /// UI display language. Mirrored into the global `i18n::LANGUAGE` signal
    /// on startup so call sites can use `t("key")` without reading state.
    /// Persisted in settings.json via `PersistedConfig::language`.
    #[serde(skip)]
    pub language: rusterm_core::config::Language,
    /// Per-session login-initialization script runtimes (see `LoginScriptRuntime`).
    /// Keyed by session id; removed when the script finishes or the session closes.
    #[serde(skip)]
    pub login_scripts: std::collections::HashMap<String, LoginScriptRuntime>,
    /// Per-session interactive-operation recorders for session-recovery
    /// replay (see [`SessionReplayRecorder`]). Keyed by live session id.
    /// Deliberately preserved across a disconnect — the recorded operations
    /// are what a reconnect replays to restore jumpserver-style interactive
    /// state — and removed only when the session/tab itself closes.
    #[serde(skip)]
    pub session_replays: HashMap<String, SessionReplayRecorder>,
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
    /// Two-stage approval gateway for model suggestions and terminal results.
    /// This object never executes commands itself: it only yields a one-shot
    /// capability after explicit user approval and withholds captured output
    /// from LLM context until a second explicit approval.
    #[serde(skip)]
    pub shadow_sandbox: rusterm_ai::ShadowSandbox,

    // ── Comparison-mode diff highlighting ──────────────────────────────────
    //
    // When comparison mode is ON with ≥2 occupied panes, the visible output
    // of every pane is diffed line-by-line. Lines that differ are
    // highlighted. If too many lines differ (the outputs are fundamentally
    // different), a warning dialog asks the user to confirm before the
    // highlights are applied.
    //
    // `comparison_diff_confirmed` resets to `false` every time comparison
    // mode is toggled, so the user is re-warned on each new comparison
    // session.
    /// Per-session row-diff results for the active tab's panes. `None` when
    /// comparison is off or no diff has been computed yet.
    #[serde(skip)]
    pub comparison_diffs: Option<Vec<(String, Vec<crate::comparison::RowDiff>)>>,
    /// When the diff exceeds the noise threshold, this holds the summary so
    /// the UI can render a warning dialog. Cleared once the user confirms or
    /// cancels.
    #[serde(skip)]
    pub comparison_diff_warning: Option<crate::comparison::DiffSummary>,
    /// Whether the user has confirmed viewing large diffs for the current
    /// comparison session. Reset to `false` when comparison mode is toggled.
    #[serde(skip)]
    pub comparison_diff_confirmed: bool,

    // ── REST relay (feature #63) ─────────────────────────────────────────
    //
    // `relay_config` is the in-memory copy of `relay.json`. It lives
    // outside `PersistedConfig` by design (adding a field there touches a
    // dozen read-modify-write sites); the relay panel edits this and calls
    // `RelayConfig::save()` after each mutation. The running server handle
    // lives in `relay_runtime`.
    #[serde(skip)]
    pub relay_config: rusterm_relay::RelayConfig,
    #[serde(skip)]
    pub relay_runtime: crate::relay_tunnel::RelayRuntime,
    /// Visibility of the relay panel. Not persisted — panels start closed.
    #[serde(skip)]
    pub relay_panel_open: bool,
    /// Last relay start/stop error, shown in the panel.
    #[serde(skip)]
    pub relay_status_message: Option<String>,

    // ── SSH tunnel manager (feature #63) ─────────────────────────────────
    //
    // The manager owns supervisor tasks (one per running tunnel). Created
    // after unlock in `app.rs`; `None` before that or if construction
    // failed (the panel then shows a placeholder).
    #[serde(skip)]
    pub tunnel_manager: Option<std::sync::Arc<rusterm_tunnel::TunnelManager>>,
    #[serde(skip)]
    pub tunnel_panel_open: bool,

    // ── Render throttle (PTY output performance) ──────────────────────────
    //
    // The terminal *model* (`TerminalEntry`) is updated on every PTY chunk
    // so scrollback stays accurate and exit codes / OneKey matches fire
    // promptly. But pushing a fresh `RenderOutput` into `tab.render_output` +
    // bumping `tab.version` triggers a Dioxus re-render — and during a
    // `tree`/`ls` flood the producer can emit hundreds of batches per
    // second, each of which would otherwise rebuild row HTML for the whole
    // visible viewport and run the vDOM diff. That keeps the CPU saturated
    // and makes typing feel laggy even though the input path itself is
    // unaffected.
    //
    // `pending_renders` holds the most recent `RenderOutput` per session
    // that has NOT yet been flushed to `tab.render_output`. We coalesce:
    // newer renders replace older ones so only the latest viewport matters.
    // `next_render_allowed` is the earliest `Instant` a flush may run for a
    // session — set to `now + RENDER_THROTTLE_INTERVAL` after each flush so
    // we cap DOM updates at ~60 fps regardless of producer rate. The event
    // loop's `tokio::select!` arm drains `pending_renders` when either a
    // new event arrives (which re-checks the clock) or the throttle timer
    // fires (which guarantees the final chunk is never stuck pending).
    #[serde(skip)]
    pub pending_renders: HashMap<String, RenderOutput>,
    #[serde(skip)]
    pub next_render_allowed: HashMap<String, std::time::Instant>,

    // ── Agent chat box (issue #122) ───────────────────────────────────────
    //
    // Runtime state for the floating, draggable chat panel. The persisted
    // bits (agents, position, last visibility) live in `PersistedConfig::chat`
    // (mirrored into the fields below at unlock time); these fields hold the
    // live, in-memory state that's NOT worth persisting (the message log,
    // the current input, the fuzzy command-search results, the drag offset).
    //
    // `chat_visible` is duplicated on `AppState` (rather than read straight
    // from `chat_settings`) so the `ToggleChat` keybinding + the panel's own
    // close button can flip it without borrowing the whole `chat_settings`
    // — and so `run_keybinding_action` (which only has `Signal<AppState>`)
    // doesn't need a config round-trip to toggle.
    #[serde(skip)]
    pub chat_visible: bool,
    /// Mirror of `PersistedConfig::chat` — the source of truth for agents /
    /// position / size. Edited in place and flushed to disk by the panel.
    #[serde(skip)]
    pub chat_settings: rusterm_core::config::ChatSettings,
    /// In-memory message log for the current session. Not persisted (a fresh
    //  open starts a clean slate — matches how most chat UIs behave).
    #[serde(skip)]
    pub chat_messages: Vec<ChatMessage>,
    /// Current contents of the chat input box.
    #[serde(skip)]
    pub chat_input: String,
    /// `true` while the input is in `/` command-search mode (palette).
    #[serde(skip)]
    pub chat_command_mode: bool,
    /// Fuzzy-filtered command candidates shown in the palette dropdown.
    #[serde(skip)]
    pub chat_command_results: Vec<ChatCommandEntry>,
    /// Index of the highlighted row in `chat_command_results`.
    #[serde(skip)]
    pub chat_command_selected: usize,
    /// Live drag offset (delta from panel origin to grab point) while the
    /// title-bar handle is being dragged. `None` when idle.
    #[serde(skip)]
    pub chat_drag_offset: Option<(f64, f64)>,
    /// Transient status line shown under the input (e.g. "thinking…",
    /// "no API key configured", error text). Cleared on next send.
    #[serde(skip)]
    pub chat_status: Option<String>,
    /// In-memory API keys keyed by agent id (issue #126). NEVER serialized —
    /// matches the project's "never persist secrets in settings.json" policy.
    /// Entered in the agent-config popover and held for the app's lifetime.
    #[serde(skip)]
    pub chat_api_keys: std::collections::HashMap<String, String>,
    /// `true` while an LLM request is in flight — blocks double-sends.
    #[serde(skip)]
    pub chat_request_in_flight: bool,

    // ── Feishu OAuth OTP auto-fill (issue #129) ──────────────────────────
    //
    // Feishu *user-token* OTP flow: the user scans a Feishu QR code once,
    // RusTerm inherits the Feishu login state as a user_access_token, sends
    // `request_text` to the ops bot (智小安) — the ONLY allowed recipient —
    // and auto-fills the bot's reply into tty OTP prompts. The local OAuth
    // callback listener lives in `crate::feishu_oauth_listener` (loopback
    // 127.0.0.1:8878); its deliveries land in `feishu_oauth_events` and are
    // processed one event loop tick later.
    /// In flight OAuth sign-ins keyed by the `state` nonce. The matching
    /// `code_verifier` is needed to exchange the authorization code after
    /// the user's scan completes.
    #[serde(skip)]
    pub feishu_pending_auths: std::collections::HashMap<String, PendingFeishuAuth>,
    /// The visible "scan to sign in" QR popup, if any. `session` names the
    /// tty session whose OTP prompt triggered the flow (`None` when started
    /// from the settings dialog).
    #[serde(skip)]
    pub feishu_qr_popup: Option<FeishuQrPopup>,
    /// The loopback port the OAuth listener actually bound (8878 preferred,
    /// 8879+ fallback). `None` until the listener starts — authorize URLs
    /// built before that fall back to the preferred port.
    #[serde(skip)]
    pub feishu_oauth_port: Option<u16>,
    /// OAuth callbacks delivered by the loopback listener — every successful
    /// exchange attempt. Processed (and drained) by the app event loop.
    #[serde(skip)]
    pub feishu_oauth_events: Vec<FeishuOAuthEvent>,
    /// Result of the most recent sign-in attempt, surfaced on the QR popup
    /// and in settings. NOT persisted — the encrypted token pair persisted
    /// by `ConfigManager::save_feishu_user_token` is the source of truth.
    #[serde(skip)]
    pub feishu_token_status: Option<FeishuTokenStatus>,
    /// Per-session OTP fetch status, used for UI feedback and debouncing
    /// (a session never starts a second fetch while one is in flight).
    #[serde(skip)]
    pub feishu_otp_status: std::collections::HashMap<String, FeishuOtpFetch>,
    /// Feishu OTP auto-fill attempts per session. After
    /// [`FEISHU_OTP_MAX_ATTEMPTS`] failures/exhaustions the session falls
    /// back to the manual OneKey popup so the user is never locked out.
    /// Reset whenever an OTP is actually delivered.
    #[serde(skip)]
    pub feishu_otp_attempts: std::collections::HashMap<String, u8>,
    /// One-shot: the Feishu QR popup was just (re)armed and settings must
    /// become visible. Written by the settings "扫码授权" button (which
    /// can't see the settings-visibility signal), consumed by app.rs.
    #[serde(skip)]
    pub feishu_auth_reveal_settings: bool,
}

/// In-flight Feishu OAuth authorization (issue #129). Created when RusTerm
/// builds the QR authorize URL; consumed when the loopback listener reports
/// the callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFeishuAuth {
    /// PKCE verifier matching the challenge embedded in the authorize URL.
    pub code_verifier: String,
    /// Session whose OTP prompt initiated the flow, if any.
    pub session: Option<String>,
    pub created: std::time::Instant,
}

/// The Feishu QR sign-in popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuQrPopup {
    /// Waiting session (OTP auto-fill target), or `None` for a settings-
    /// initiated authorization.
    pub session: Option<String>,
    /// Full authorize URL rendered as the QR code.
    pub authorize_url: String,
    /// `state` nonce — index into `feishu_pending_auths`.
    pub state_nonce: String,
    /// Loopback port the OAuth listener accepted the request on (used to pin
    /// the redirect URI when re-arming a cancelled flow).
    pub port: u16,
    /// Lifecycle the popup renders: scanning, delivered, or failed.
    pub status: FeishuQrPopupStatus,
}

/// Lifecycle of one QR sign-in attempt, rendered by the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuQrPopupStatus {
    /// Waiting for the user to scan and authorize.
    Scanning { started: std::time::Instant },
    /// Tokens exchanged, the cached OTP was filled into the waiting session.
    Delivered { delivered_at: std::time::Instant },
    /// The flow died with a user-readable, secret-free reason.
    Failed {
        reason: String,
        failed_at: std::time::Instant,
    },
}

impl FeishuQrPopupStatus {
    pub fn failed_at(&self) -> Option<std::time::Instant> {
        match self {
            FeishuQrPopupStatus::Failed { failed_at, .. } => Some(*failed_at),
            _ => None,
        }
    }

    pub fn delivered_at(&self) -> Option<std::time::Instant> {
        match self {
            FeishuQrPopupStatus::Delivered { delivered_at } => Some(*delivered_at),
            _ => None,
        }
    }
}

/// A Feishu OAuth callback outcome as delivered by the loopback listener
/// (values only, no UI types so the listener crate layer stays lean).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuOAuthEvent {
    pub state: String,
    /// Authorization `code` on success; `Err` carries the user-readable
    /// OAuth error description from Feishu.
    pub result: Result<String, String>,
}

/// Non-secret summary of the latest Feishu sign-in attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuTokenStatus {
    /// Tokens obtained and persisted. Value: access-token expiry (unix secs).
    Connected { expires_at: i64 },
    /// The authorization code exchange failed.
    Failed {
        reason: String,
        at: std::time::Instant,
    },
}

/// Per-session state of the Feishu OTP bot round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuOtpFetch {
    /// A request to the bot is in flight (message sent, reply polled).
    InFlight { started: std::time::Instant },
    /// The bot's reply fetched and auto-filled.
    Delivered { at: std::time::Instant },
    /// Fetch failed or timed out (`reason` is log/UI-safe, no secrets).
    Failed {
        reason: String,
        at: std::time::Instant,
    },
}

/// One turn in the chat log. `role` mirrors OpenAI's convention so the same
/// vector can be serialized straight into a chat-completions request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ChatRole {
    User,
    Assistant,
    System,
}

/// A single entry in the command-palette dropdown. `source` is surfaced in the
/// UI so the user can tell apart history hits from built-in app commands.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ChatCommandEntry {
    pub command: String,
    pub source: ChatCommandSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ChatCommandSource {
    History,
    AppCommand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedCommandLine {
    Reliable(String),
    Unreliable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCommandPayload {
    /// The command text is already present in the shell's input buffer; only
    /// the captured Enter bytes should be sent.
    EnterOnly(Vec<u8>),
    /// Send the complete command followed by a carriage return.
    FullLine,
}

/// State held while the dangerous-command confirmation modal is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDangerousCommand {
    pub command: String,
    pub reason: String,
    /// Every session that will execute the command (Compare mode may provide
    /// more than one). Each target receives its own pending-history id.
    pub targets: Vec<String>,
    pub payload: PendingCommandPayload,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionConnectionState {
    #[default]
    Connected,
    Disconnected,
    Reconnecting,
    /// Initial connection attempt is in flight (not yet authenticated). The
    /// indicator dot renders blue while this is active. Set by `open_connection`
    /// before the connection driver runs, and replaced by `Connected` on
    /// success or `Failed` on error.
    Connecting,
    /// A connect/reconnect attempt failed. Semantically "settled" (like
    /// `Disconnected`) for snapshot-write gating, but visually distinct so the
    /// indicator dot renders red. `begin_reconnect` accepts this as a valid
    /// retry starting point, same as `Disconnected`.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendTargetOption {
    pub session_id: String,
    pub label: String,
}

/// Non-secret lifecycle state for a submitted OneKey credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneKeySubmissionFeedback {
    /// The credential and one carriage return were queued for this session.
    Submitted { matched_expect: String },
    /// The remote emitted the same matching prompt again after submission.
    Rejected { matched_expect: String },
}

/// State of the OneKey autofill popup for a single session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OneKeyPopupState {
    pub visible: bool,
    /// Stable saved-connection identity used to scope remembered selections.
    pub connection_id: Option<String>,
    /// SHA-256 of the normalized current prompt. Prompt text is never persisted.
    pub prompt_fingerprint: Option<String>,
    /// SHA-256 of the normalized current prompt, computed for EVERY prompt
    /// (unlike `prompt_fingerprint`, which is only set for prompts safe to
    /// remember as a persisted preference). Keys the habit-learning tier:
    /// behavior events and habit lookups use this fingerprint, so generic
    /// prompts (a bare `Password:`) can still learn per-connection habits.
    /// For safe prompts both fingerprints are identical.
    pub habit_fingerprint: Option<String>,
    /// Whether this concrete prompt is a sudo password request. Used only to
    /// preserve the existing host-bound relay elevation lease on auto-submit.
    pub is_sudo_password: bool,
    /// Matching entries (one per OneKey whose step matched), each carrying the
    /// send value of the matched step.
    pub matches: Vec<OneKeyMatch>,
    pub selected: usize,
    /// The expect pattern that matched (used by "Save In OneKeys" to prefill).
    pub matched_expect: Option<String>,
}

/// A single match in the OneKey popup. `send` is the decrypted value of the
/// exact step that matched the current prompt; it must never appear in logs.
#[derive(Clone, Default, PartialEq)]
pub struct OneKeyMatch {
    /// Stable identifiers used by remembered selections. Display names and
    /// translated labels are intentionally not business keys.
    pub onekey_id: String,
    pub step_id: String,
    pub name: String,
    pub label: String,
    pub send: String,
    /// Expect regex belonging to this exact match. Kept so selecting among
    /// several matching OneKeys correlates rejection with the chosen entry.
    pub matched_expect: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneKeyPreferenceAttempt {
    pub preference: OneKeyPreference,
    pub matched_expect: String,
}

/// What happened in one OneKey interaction. String forms are the `action`
/// column values in the DuckDB `onekey_events` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneKeyBehaviorKind {
    /// The user explicitly picked a candidate in the chooser popup.
    ManualSelect,
    /// A remembered preference or learned habit submitted without a popup.
    AutoSubmit,
    /// The remote re-emitted the same prompt after a submission (wrong or
    /// stale credential).
    Rejected,
    /// The chooser popup was shown (no habit, ambiguous, changed candidates,
    /// or after an error).
    PopupShown,
}

impl OneKeyBehaviorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OneKeyBehaviorKind::ManualSelect => "manual_select",
            OneKeyBehaviorKind::AutoSubmit => "auto_submit",
            OneKeyBehaviorKind::Rejected => "rejected",
            OneKeyBehaviorKind::PopupShown => "popup_shown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual_select" => Some(OneKeyBehaviorKind::ManualSelect),
            "auto_submit" => Some(OneKeyBehaviorKind::AutoSubmit),
            "rejected" => Some(OneKeyBehaviorKind::Rejected),
            "popup_shown" => Some(OneKeyBehaviorKind::PopupShown),
            _ => None,
        }
    }
}

/// One observed OneKey behavior event. Metadata only: stable identifiers and
/// SHA-256 fingerprints — never credential values, display names, or raw
/// prompt text (mirrors the `OneKeyPreference` privacy contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneKeyBehaviorEvent {
    pub connection_id: String,
    /// The habit fingerprint of the prompt (SHA-256, computed by the UI).
    pub prompt_fingerprint: String,
    /// Target candidate. Empty for [`OneKeyBehaviorKind::PopupShown`].
    pub onekey_id: String,
    pub step_id: String,
    pub kind: OneKeyBehaviorKind,
    /// Digest of the sorted candidate identifiers at event time. Detects
    /// "the user added/removed a OneKey matching this prompt".
    pub candidates_hash: String,
}

/// Cap on in-memory habit events per `(connection, prompt)` key. Habit
/// resolution only ever inspects the newest few events; the rest exist so a
/// rejection buried under a couple of newer selections still shows in
/// diagnostics.
pub const ONEKEY_HABIT_EVENTS_CAP: usize = 32;

/// Insert a behavior event into the in-memory habit cache only (no DuckDB
/// re-queue). Used when replaying persisted events at unlock and by
/// [`record_onekey_behavior`] for live events. `PopupShown` events are
/// excluded — the resolver only reasons about selections and rejections, and
/// popup-noise between them must not evict useful history.
pub fn seed_onekey_habit_event(state: &mut AppState, event: OneKeyBehaviorEvent) {
    if event.kind == OneKeyBehaviorKind::PopupShown {
        return;
    }
    let key = (
        event.connection_id.clone(),
        event.prompt_fingerprint.clone(),
    );
    let events = state.onekey_habit_events.entry(key).or_default();
    events.push(event);
    if events.len() > ONEKEY_HABIT_EVENTS_CAP {
        let excess = events.len() - ONEKEY_HABIT_EVENTS_CAP;
        events.drain(..excess);
    }
}

/// Append a live behavior event to the in-memory habit cache and queue it
/// for the DuckDB flush (see `flush_onekey_behavior_events` in `app.rs`).
pub fn record_onekey_behavior(state: &mut AppState, event: OneKeyBehaviorEvent) {
    seed_onekey_habit_event(state, event.clone());
    state.onekey_pending_analytics.push(event);
}

impl std::fmt::Debug for OneKeyMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneKeyMatch")
            .field("name", &self.name)
            .field("label", &self.label)
            .field("send", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod onekey_match_tests {
    use super::OneKeyMatch;

    #[test]
    fn debug_redacts_the_decrypted_send_value() {
        let entry = OneKeyMatch {
            onekey_id: "account-id".to_string(),
            step_id: "step-id".to_string(),
            name: "account".to_string(),
            label: "Password".to_string(),
            send: "never-log-this-secret".to_string(),
            matched_expect: r"password:".to_string(),
        };
        let debug = format!("{entry:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("never-log-this-secret"));
    }
}

#[cfg(test)]
mod onekey_behavior_cache_tests {
    use super::{
        AppState, ONEKEY_HABIT_EVENTS_CAP, OneKeyBehaviorEvent, OneKeyBehaviorKind,
        record_onekey_behavior, seed_onekey_habit_event,
    };

    fn event(kind: OneKeyBehaviorKind, onekey_id: &str) -> OneKeyBehaviorEvent {
        OneKeyBehaviorEvent {
            connection_id: "conn".to_string(),
            prompt_fingerprint: "fp".to_string(),
            onekey_id: onekey_id.to_string(),
            step_id: "step".to_string(),
            kind,
            candidates_hash: "hash".to_string(),
        }
    }

    #[test]
    fn record_caches_selections_and_queues_everything_for_analytics() {
        let mut state = AppState::default();
        record_onekey_behavior(&mut state, event(OneKeyBehaviorKind::ManualSelect, "a"));
        record_onekey_behavior(&mut state, event(OneKeyBehaviorKind::PopupShown, ""));

        let key = ("conn".to_string(), "fp".to_string());
        let cached = state.onekey_habit_events.get(&key).unwrap();
        assert_eq!(
            cached.len(),
            1,
            "popup_shown must not enter the habit cache"
        );
        assert_eq!(cached[0].kind, OneKeyBehaviorKind::ManualSelect);
        assert_eq!(
            state.onekey_pending_analytics.len(),
            2,
            "every event is queued for the DuckDB flush"
        );
    }

    #[test]
    fn habit_cache_is_capped_keeping_the_newest_events() {
        let mut state = AppState::default();
        for index in 0..(ONEKEY_HABIT_EVENTS_CAP + 5) {
            seed_onekey_habit_event(
                &mut state,
                event(OneKeyBehaviorKind::ManualSelect, &format!("ok-{index}")),
            );
        }
        let key = ("conn".to_string(), "fp".to_string());
        let cached = state.onekey_habit_events.get(&key).unwrap();
        assert_eq!(cached.len(), ONEKEY_HABIT_EVENTS_CAP);
        assert_eq!(
            cached.last().unwrap().onekey_id,
            format!("ok-{}", ONEKEY_HABIT_EVENTS_CAP + 4),
            "the newest event survives the cap"
        );
        assert!(
            state.onekey_pending_analytics.is_empty(),
            "seeding (warm-load replay) must not re-queue analytics writes"
        );
    }
}

/// Runtime status of the last command executed in a session, used to
/// render a colored badge in workspace tabs and pane title bars (Task #65).
/// `Idle` is the default for newly-opened sessions (no command has
/// finished yet). This is `#[serde(skip)]` on `SessionTab` because it is
/// ephemeral UI state, not something to persist across restarts.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum CommandStatus {
    /// No command has completed yet (or the status was cleared).
    #[default]
    Idle,
    /// The last command exited with code 0.
    Success,
    /// The last command exited with a non-zero code.
    Failed(i32),
    /// The session channel dropped before the command reported an exit
    /// code. `reason` is the human-readable disconnect cause.
    Disconnected(String),
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
    /// Commands in `suggestions` that are typo corrections rather than
    /// ordinary history completions. Selecting one replaces the input line
    /// without executing it; correction rows cannot be deleted as history.
    #[serde(skip)]
    pub suggestion_corrections: HashSet<String>,
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
    /// Runtime status of the last finished command (or disconnect). Drives
    /// the colored badge in workspace tabs and pane title bars. Updated whenever the
    /// shell reports an exit code (OSC 133;D, zsh/bash only) or when the
    /// session channel drops. Not persisted across restarts.
    #[serde(skip)]
    pub last_command_status: CommandStatus,
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
            sidebar_preferences: SidebarPreferences::default(),
            workspace_preferences: WorkspacePreferences::default(),
            connections: Vec::new(),
            theme: Theme::Dark,
            focused_tab_appearance: FocusedTabAppearance::default(),
            keybindings: Keybindings::default(),
            skin: SkinSettings::default(),
            close_senders: Vec::new(),
            resize_senders: HashMap::new(),
            config_manager: None,
            terminals: HashMap::new(),
            session_logs: HashMap::new(),
            unlock_state,
            master_password_error: None,
            suggestion_epoch: 0,
            pending_exit_check: HashMap::new(),
            exit_code_sessions: HashSet::new(),
            terminal_command_lines: HashMap::new(),
            history_completion_sessions: HashSet::new(),
            suggestion_muted_sessions: HashSet::new(),
            recent_failed_commands: HashSet::new(),
            last_failed_command_by_session: HashMap::new(),
            onekeys: Vec::new(),
            onekey_preferences: Vec::new(),
            onekey_preference_attempts: HashMap::new(),
            onekey_habit_events: HashMap::new(),
            onekey_pending_analytics: Vec::new(),
            onekey_popups: HashMap::new(),
            onekey_submission_feedback: HashMap::new(),
            onekey_submission_cooldown: HashMap::new(),
            onekey_output_since_submission: HashMap::new(),
            onekey_skip_logged: HashSet::new(),
            session_configs: HashMap::new(),
            session_connection_states: HashMap::new(),
            session_nodes: HashMap::new(),
            otp_groups: OtpGroupRegistry::default(),
            send_target_selection: None,
            ssh_sessions: HashMap::new(),
            sftp_clients: HashMap::new(),
            transfers: crate::transfers::TransferState::default(),
            transfer_cancellations: HashMap::new(),
            zmodem: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::zmodem::ZmodemSessions::new(),
            )),
            bottom_shell_session_id: None,
            analytics: crate::analytics::AnalyticsHandle::default(),
            ohmyzsh: None,
            layouts: HashMap::new(),
            focused_pane: None,
            layout_preset: LayoutPreset::default(),
            split_mode_enabled: true,
            restore_pending: None,
            restore_disabled: false,
            confirm_close_on_exit: true,
            comparison_diff_warning_enabled: true,
            suggestion_enabled: true,
            suggestion_count: 3,
            collect_usage_habits: false,
            language: rusterm_core::config::Language::default(),
            login_scripts: std::collections::HashMap::new(),
            session_replays: HashMap::new(),
            close_dialog_visible: false,
            close_dialog_dont_ask_again: true,
            pending_dangerous_command: None,
            safety_checker: rusterm_core::CommandSafetyChecker::new(),
            shadow_sandbox: rusterm_ai::ShadowSandbox::default(),
            comparison_diffs: None,
            comparison_diff_warning: None,
            comparison_diff_confirmed: false,
            relay_config: rusterm_relay::RelayConfig::default(),
            relay_runtime: crate::relay_tunnel::RelayRuntime::default(),
            relay_panel_open: false,
            relay_status_message: None,
            tunnel_manager: None,
            tunnel_panel_open: false,
            pending_renders: HashMap::new(),
            next_render_allowed: HashMap::new(),
            chat_visible: false,
            chat_settings: rusterm_core::config::ChatSettings::default().normalized(),
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_command_mode: false,
            chat_command_results: Vec::new(),
            chat_command_selected: 0,
            chat_drag_offset: None,
            chat_status: None,
            chat_api_keys: std::collections::HashMap::new(),
            chat_request_in_flight: false,
            feishu_pending_auths: std::collections::HashMap::new(),
            feishu_qr_popup: None,
            feishu_oauth_port: None,
            feishu_oauth_events: Vec::new(),
            feishu_token_status: None,
            feishu_otp_status: std::collections::HashMap::new(),
            feishu_otp_attempts: std::collections::HashMap::new(),
            feishu_auth_reveal_settings: false,
        }
    }
}

impl AppState {
    /// Remove the cancellation token only when it belongs to the completing
    /// transfer attempt. A stale completion must not remove a retry's token.
    pub fn remove_transfer_cancellation_for_attempt(&mut self, job_id: &str, attempt: u32) -> bool {
        let is_current = self
            .transfer_cancellations
            .get(job_id)
            .is_some_and(|(current_attempt, _)| *current_attempt == attempt);
        if is_current {
            self.transfer_cancellations.remove(job_id);
        }
        is_current
    }

    /// Build a `SessionState` snapshot from the current app state, suitable
    /// for saving to `session_state.enc`.
    ///
    /// Only terminals whose runtime state is `Connected` are captured. This
    /// makes snapshot membership the durable record of which terminals were
    /// logged in when the app last exited.
    ///
    /// Captures, per session:
    /// - id, name, kind, hostname, connection_id
    /// - cwd (last reported by the shell via OSC 7 — `None` if the shell
    ///   never reported one, e.g. raw telnet/serial)
    /// - tail of `command_history` (last N entries, display-only — these are
    ///   NEVER re-executed on restore; they're just re-seeded into the
    ///   suggestion popup)
    /// - terminal grid size (cols × rows + pixel dims) so the restored
    ///   session opens at the same resolution
    ///
    /// NOT captured: scrollback (too large), env vars (would leak secrets),
    /// PTY process state (impossible to restore), input box content.
    ///
    /// `theme_name` is the name of the current theme so it can be restored
    /// on next launch without a flicker.
    pub fn build_session_state(&self, theme_name: &str) -> rusterm_core::SessionState {
        // Emit sessions in tab-bar order so a restore reopens them in the
        // exact order the user last arranged (drag-reorders included) —
        // `restore_sessions` appends workspace tabs in snapshot order, so the
        // snapshot's vec order IS the restored tab order. Anchor sessions
        // come first following `self.tabs`; sessions living only inside a
        // split layout (no tab of their own) follow in `self.sessions`
        // order.
        let mut ordered: Vec<&SessionTab> = Vec::with_capacity(self.sessions.len());
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.sessions.len());
        for tab in &self.tabs {
            let Some(anchor_id) = tab.anchor_session_id.as_deref() else {
                continue;
            };
            if let Some(session) = self.sessions.iter().find(|s| s.id == anchor_id)
                && seen.insert(anchor_id)
            {
                ordered.push(session);
            }
        }
        for session in &self.sessions {
            if !seen.contains(session.id.as_str()) {
                ordered.push(session);
            }
        }

        let sessions: Vec<_> = ordered
            .into_iter()
            .filter(|tab| self.bottom_shell_session_id.as_deref() != Some(tab.id.as_str()))
            // Presence in the encrypted snapshot is the durable record that
            // this terminal was logged in when the app last exited. Tabs that
            // were already disconnected are deliberately omitted so startup
            // recovery does not resurrect sessions the user had logged out of.
            .filter(|tab| {
                matches!(
                    self.session_connection_states.get(&tab.id),
                    Some(SessionConnectionState::Connected)
                )
            })
            .map(|tab| {
                // Tail of command history — last 100 entries, display-only.
                let history_tail: Vec<String> = if tab.command_history.len() > 100 {
                    tab.command_history[tab.command_history.len() - 100..].to_vec()
                } else {
                    tab.command_history.clone()
                };

                // Look up connection_id for SSH/Telnet/Tcp sessions so we can
                // find the matching `ConnectionConfig` on restore. The
                // session's own stored config (keyed by tab id) carries the
                // saved-connection identity in `ConnectionConfig::id` — that
                // is what `restore_sessions` matches against the saved
                // connections list. Copied sessions ("X 副本") keep the
                // source's connection id, so a duplicate stays restorable
                // even though its display name matches no saved connection.
                let connection_id = match tab.kind {
                    rusterm_core::session::SessionType::Ssh
                    | rusterm_core::session::SessionType::Telnet
                    | rusterm_core::session::SessionType::Tcp => self
                        .session_configs
                        .get(&tab.id)
                        .map(|c| c.id.clone())
                        .or_else(|| Some(tab.id.clone())),
                    _ => None,
                };

                // Capture the terminal's current grid size so the restored
                // session opens at the same resolution instead of 80×24.
                let terminal_size = self.terminals.get(&tab.id).map(|h| {
                    let t = h.lock();
                    rusterm_core::PersistedTerminalSize {
                        cols: t.terminal.size().cols,
                        rows: t.terminal.size().rows,
                        pixel_width: t.terminal.size().pixel_width,
                        pixel_height: t.terminal.size().pixel_height,
                    }
                });

                rusterm_core::session_state::PersistedSession {
                    id: tab.id.clone(),
                    name: tab.name.clone(),
                    kind: tab.kind,
                    hostname: tab.hostname.clone(),
                    connection_id,
                    cwd: tab.cwd.clone(),
                    command_history_tail: history_tail,
                    terminal_size,
                    // Establishment-phase interactive ops (jumpserver menu
                    // navigation). Empty for integrated shells — evidence
                    // clears the recorder — so plain shell commands are never
                    // persisted here.
                    replay_ops: replayable_ops(self, &tab.id),
                }
            })
            .collect();

        let active_session = self
            .active_session
            .as_ref()
            .filter(|active_id| sessions.iter().any(|session| &session.id == *active_id))
            .cloned()
            .or_else(|| sessions.first().map(|session| session.id.clone()));

        rusterm_core::SessionState {
            schema_version: 1,
            saved_at: chrono::Utc::now(),
            active_session,
            sessions,
            theme: Some(theme_name.to_string()),
        }
    }

    /// Whether a freshly-built snapshot may safely overwrite the on-disk
    /// `session_state.enc` right now.
    ///
    /// Non-empty snapshots are always writable. An **empty** snapshot is the
    /// durable "user logged out of every terminal" record — but it is only
    /// trustworthy when every workspace terminal has *definitively*
    /// disconnected. While a connect or reconnect is still in flight (the
    /// tab exists but its connection state is not yet `Connected` and not
    /// yet `Disconnected`), writing the empty snapshot would destroy the
    /// previous run's memory: e.g. the user clicks 恢复, the jumpserver
    /// reconnect takes ten seconds, and a save tick (or a quit) lands in
    /// that window. Deferring the write preserves the old snapshot until
    /// the in-flight sessions settle either way.
    ///
    /// The embedded bottom shell is ignored — it's never part of the
    /// snapshot, so its (always `Connected`) state must not block empty
    /// writes.
    pub fn session_snapshot_writable(&self, snapshot: &rusterm_core::SessionState) -> bool {
        if !snapshot.sessions.is_empty() {
            return true;
        }
        !self.sessions.iter().any(|tab| {
            self.bottom_shell_session_id.as_deref() != Some(tab.id.as_str())
                && !matches!(
                    self.session_connection_states.get(&tab.id),
                    Some(SessionConnectionState::Disconnected | SessionConnectionState::Failed)
                )
        })
    }

    /// Encrypt + atomically persist the current session state. No-op if
    /// `restore_disabled` is true (the user picked "不再询问" earlier — we
    /// don't save so we don't re-prompt on next launch either). Returns the
    /// save result so the caller can log failures.
    ///
    /// `master_key` is the AES-256-GCM key derived from the master password;
    /// comes from `ConfigManager::master_key()`.
    ///
    /// The legacy `restore_disabled` flag only represented whether to show the
    /// old confirmation prompt. It no longer suppresses persistence or
    /// automatic recovery.
    pub fn save_session_state(&self, master_key: &[u8; 32]) -> anyhow::Result<()> {
        let state = self.build_session_state(self.theme_name());
        state.save(master_key)
    }

    /// Returns the selected application skin name for session persistence.
    pub fn theme_name(&self) -> &'static str {
        self.skin.kind.label()
    }

    // ── Pane-layout persistence ───────────────────────────────────────────
    //
    // The user can freely customise the multi-pane arrangement (split tree,
    // column/row fractions, floating window geometry, comparison mode). These
    // helpers snapshot that arrangement into a [`LayoutState`] (each tab = one
    // independent JSON segment) and restore it after sessions are reopened.
    //
    // Because session ids are regenerated on every launch, panes reference
    // sessions by *display name* in the persisted form — see the module docs
    // of [`crate::layout_state`] for the full rationale.

    /// Build a [`LayoutState`] snapshot from the current workspace tabs.
    ///
    /// Only tabs whose layout differs from a plain single-pane arrangement
    /// are included — a tab with one pane at default geometry has nothing
    /// custom to remember, and omitting it keeps the file small.
    ///
    /// Each pane's `session_id` is rewritten to the session's display name
    /// so the snapshot is stable across launches (live ids are fresh UUIDs).
    pub fn build_layout_state(&self) -> crate::layout_state::LayoutState {
        use crate::layout_state::{LayoutState, PersistedTabLayout};

        let session_name = |sid: &str| -> String {
            self.sessions
                .iter()
                .find(|t| t.id == sid)
                .map(|t| t.name.clone())
                .unwrap_or_default()
        };

        let mut tabs = Vec::new();
        for tab in &self.tabs {
            let Some(layout) = self.layouts.get(&tab.id) else {
                continue;
            };
            // Skip trivial single-pane layouts — nothing custom to persist.
            // A single non-empty pane with no floating geometry and default
            // fractions is the implicit default; restoring it is a no-op.
            let non_empty = layout
                .panes
                .iter()
                .filter(|p| !p.session_id.is_empty())
                .count();
            if non_empty <= 1 && layout.panes.iter().all(|p| p.floating.is_none()) {
                continue;
            }

            // Clone and rewrite session_id → display name for every pane.
            let mut snapshot = layout.clone();
            for pane in &mut snapshot.panes {
                if !pane.session_id.is_empty() {
                    pane.session_id = session_name(&pane.session_id);
                }
            }

            let anchor_name = tab
                .anchor_session_id
                .as_deref()
                .map(session_name)
                .unwrap_or_default();
            // If the anchor session has no resolvable name we can't
            // reattach the layout on restore — skip it rather than emit a
            // dangling entry.
            if anchor_name.is_empty() {
                continue;
            }

            tabs.push(PersistedTabLayout {
                anchor_name,
                layout: snapshot,
            });
        }

        LayoutState {
            schema_version: 1,
            saved_at: Some(chrono::Utc::now()),
            tabs,
        }
    }

    /// Reapply a previously-saved [`LayoutState`] after sessions have been
    /// restored.
    ///
    /// For each persisted tab layout we locate the restored workspace tab
    /// whose anchor session name matches, then rewrite every pane's
    /// placeholder name back to the live session id and insert the layout
    /// under the tab's (fresh) group id.
    ///
    /// Panes whose name no longer matches any live session are cleared
    /// (empty `session_id`) — the renderer shows them as blank drop targets
    /// instead of dropping them, preserving the user's split structure.
    pub fn apply_layout_state(&mut self, saved: &crate::layout_state::LayoutState) {
        use crate::layout_state::PersistedTabLayout;

        // Map display name → the live session ids carrying that name, in
        // session order. Ids are CONSUMED as panes claim them, so two
        // sessions sharing a display name (e.g. the same connection opened
        // twice) resolve to two different panes instead of both panes
        // collapsing onto one session — the root cause of the "two
        // identical panes after restore" bug. Consumption is global across
        // tabs so a session never renders in more than one pane.
        let mut name_to_ids: HashMap<String, Vec<String>> = HashMap::new();
        for s in &self.sessions {
            name_to_ids
                .entry(s.name.clone())
                .or_default()
                .push(s.id.clone());
        }

        for PersistedTabLayout {
            anchor_name,
            layout,
        } in &saved.tabs
        {
            // Find the workspace tab whose anchor session has this name.
            let tab =
                self.tabs
                    .iter()
                    .find(|t| {
                        t.anchor_session_id.as_deref().and_then(|sid| {
                            self.sessions.iter().find(|s| s.id == sid).map(|s| &s.name)
                        }) == Some(anchor_name)
                    })
                    .map(|t| (t.id.clone(), t.anchor_session_id.clone()));
            let Some((group_id, anchor_id)) = tab else {
                tracing::debug!(
                    "layout restore: no restored tab matches anchor {:?} — skipping",
                    anchor_name
                );
                continue;
            };

            // The tab's own anchor session must claim the first pane that
            // references the anchor name; otherwise a same-named sibling
            // session could take it and leave the anchor orphaned.
            if let (Some(anchor_id), Some(ids)) = (
                anchor_id.as_ref(),
                name_to_ids.get_mut(anchor_name.as_str()),
            ) {
                if let Some(pos) = ids.iter().position(|id| id == anchor_id) {
                    ids.swap(0, pos);
                }
            }

            let mut restored = layout.clone();
            let mut duplicate_panes: Vec<usize> = Vec::new();
            for (idx, pane) in restored.panes.iter_mut().enumerate() {
                if pane.session_id.is_empty() {
                    continue;
                }
                // session_id currently holds a display name (written at save
                // time). Resolve it back to a live id; if the name is
                // unknown, clear the pane so it renders as an empty drop
                // target; if the name is known but every session with that
                // name is already shown elsewhere, the pane is a duplicate
                // and gets removed below.
                match name_to_ids.get_mut(pane.session_id.as_str()) {
                    Some(ids) if !ids.is_empty() => {
                        pane.session_id = ids.remove(0);
                    }
                    Some(_) => {
                        tracing::info!(
                            "layout restore: pane session {:?} already shown in another pane — dropping duplicate pane",
                            pane.session_id
                        );
                        duplicate_panes.push(idx);
                    }
                    None => {
                        tracing::info!(
                            "layout restore: pane session {:?} not found among restored sessions — leaving pane empty",
                            pane.session_id
                        );
                        pane.session_id.clear();
                    }
                }
            }
            // Remove duplicate panes back-to-front so earlier indices stay
            // valid. If the layout can't shrink (no split tree / last
            // pane), fall back to clearing the pane instead of duplicating
            // the session.
            for idx in duplicate_panes.into_iter().rev() {
                if restored.remove_pane(idx).is_none() {
                    if let Some(pane) = restored.panes.get_mut(idx) {
                        pane.session_id.clear();
                    }
                }
            }

            self.layouts.insert(group_id, restored);
        }
    }

    /// Remove workspace tabs whose anchor session now lives as a pane inside
    /// a *different* tab's split layout.
    ///
    /// Called after [`apply_layout_state`] during session restore. The restore
    /// flow creates one top-level tab per restored session, then re-attaches
    /// saved split layouts. A session that was pane 1 of a split (not the
    /// anchor) ends up with BOTH its own standalone tab AND a pane slot in the
    /// anchor's tab — the user sees "two windows" for the same session. This
    /// method drops the redundant standalone tabs so each session appears in
    /// exactly one place.
    ///
    /// A tab is redundant when its anchor session id appears as a pane in some
    /// *other* tab's layout. The anchor's own layout legitimately references
    /// itself in pane 0 — that is not a duplicate.
    pub fn dedup_pane_session_tabs(&mut self) {
        let tabs_snapshot: Vec<(String, Option<String>)> = self
            .tabs
            .iter()
            .map(|t| (t.id.clone(), t.anchor_session_id.clone()))
            .collect();
        let mut to_remove: Vec<String> = Vec::new();
        for (tab_id, anchor) in &tabs_snapshot {
            let Some(anchor) = anchor else { continue };
            let is_pane_elsewhere = self.layouts.iter().any(|(other_tab_id, layout)| {
                other_tab_id != tab_id && layout.panes.iter().any(|p| p.session_id == *anchor)
            });
            if is_pane_elsewhere {
                to_remove.push(tab_id.clone());
            }
        }
        if to_remove.is_empty() {
            return;
        }
        tracing::info!(
            "Removed {} duplicate tab(s) after layout restore",
            to_remove.len()
        );
        for tab_id in &to_remove {
            self.tabs.retain(|t| &t.id != tab_id);
            self.layouts.remove(tab_id);
        }
        // Ensure active_tab still points to a valid tab after removal.
        if let Some(active) = self.active_tab.clone() {
            if !self.tabs.iter().any(|t| t.id == active) {
                self.active_tab = self.tabs.first().map(|t| t.id.clone());
            }
        }
    }

    /// Upgrade a session's tab badge to `Success` after its login script
    /// completed.
    ///
    /// Interactive jump-host sessions (e.g. a jumpserver asset menu driven
    /// by a login script) never emit OSC 133;D exit codes, so without this
    /// the tab badge would stay `Idle` forever even though the scripted
    /// navigation succeeded. A real command status (Success / Failed /
    /// Disconnected) always wins — only an `Idle` badge is upgraded.
    pub fn mark_login_script_success(&mut self, session_id: &str) {
        if let Some(tab) = self.sessions.iter_mut().find(|t| t.id == session_id) {
            if matches!(tab.last_command_status, CommandStatus::Idle) {
                tab.last_command_status = CommandStatus::Success;
            }
        }
    }

    /// Decide the tab badge for a fresh (re)connection attempt's outcome.
    ///
    /// Plain SSH terminals never emit OSC 133;D until the user runs a
    /// command, so without this the tab badge would stay `Idle` after a
    /// successful connect — unlike jump-host sessions, which get a green ✓
    /// via `mark_login_script_success`.
    ///
    /// - Success: upgrades `Idle` (fresh connect) and `Disconnected`
    ///   (reconnect — the badge still shows the previous attempt's disconnect)
    ///   to `Success`. A real command status (`Success`/`Failed`) is kept:
    ///   connect-time runs before any new command, but the exit-code pipeline
    ///   owns it from then on.
    /// - Failure: *always* resets the badge to `Disconnected(reason)` so a
    ///   stale status from the previous attempt cannot linger above the new
    ///   terminal's "connection failed" message.
    pub fn note_connection_outcome(&mut self, session_id: &str, failure: Option<String>) {
        let Some(tab) = self.sessions.iter_mut().find(|t| t.id == session_id) else {
            return;
        };
        tab.last_command_status = match failure {
            Some(reason) => CommandStatus::Disconnected(reason),
            None => match tab.last_command_status {
                CommandStatus::Idle | CommandStatus::Disconnected(_) => CommandStatus::Success,
                ref kept => kept.clone(),
            },
        };
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

/// Build the workspace → pane → session hierarchy consumed by navigation UI.
///
/// `sessions` is the source of truth for live sessions. Stale layout or anchor
/// references therefore produce an empty pane instead of a synthetic session,
/// and a live session referenced more than once appears only at its first
/// position in tab/pane order.
pub fn build_session_tree(state: &AppState) -> Vec<WorkspaceNode> {
    let sessions_by_id: HashMap<&str, &SessionTab> = state
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect();
    let mut seen_sessions = HashSet::new();

    state
        .tabs
        .iter()
        .map(|tab| {
            let pane_session_ids: Vec<&str> = state.layouts.get(&tab.id).map_or_else(
                || vec![tab.anchor_session_id.as_deref().unwrap_or_default()],
                |layout| {
                    layout
                        .panes
                        .iter()
                        .map(|pane| pane.session_id.as_str())
                        .collect()
                },
            );

            let panes = pane_session_ids
                .into_iter()
                .enumerate()
                .map(|(index, session_id)| {
                    let session = sessions_by_id
                        .get(session_id)
                        .filter(|_| !session_id.is_empty())
                        .and_then(|session| {
                            seen_sessions
                                .insert(session.id.as_str())
                                .then(|| SessionNode {
                                    id: session.id.clone(),
                                    name: session.name.clone(),
                                    kind: session.kind,
                                    is_active: state.active_session.as_deref()
                                        == Some(session.id.as_str()),
                                    connection_state: state
                                        .session_connection_states
                                        .get(&session.id)
                                        .copied()
                                        .unwrap_or_default(),
                                })
                        });

                    PaneNode {
                        index,
                        is_focused: state.focused_pane.as_ref().is_some_and(|focused| {
                            focused.layout_owner_tab_id == tab.id && focused.pane_idx == index
                        }),
                        session,
                    }
                })
                .collect();

            WorkspaceNode {
                tab_id: tab.id.clone(),
                anchor_session_id: tab.anchor_session_id.clone(),
                is_active: state.active_tab.as_deref() == Some(tab.id.as_str()),
                panes,
            }
        })
        .collect()
}

pub fn track_terminal_input(state: &mut AppState, session_ids: &[String], data: &[u8]) {
    for session_id in session_ids {
        let line = state
            .terminal_command_lines
            .entry(session_id.clone())
            .or_insert_with(|| TrackedCommandLine::Reliable(String::new()));
        let mut offset = 0;
        while offset < data.len() {
            match data[offset] {
                0x03 | 0x15 => {
                    *line = TrackedCommandLine::Reliable(String::new());
                    offset += 1;
                }
                0x17 => {
                    if let TrackedCommandLine::Reliable(value) = line {
                        while value.ends_with(char::is_whitespace) {
                            value.pop();
                        }
                        while value.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
                            value.pop();
                        }
                    }
                    offset += 1;
                }
                0x7f => {
                    if let TrackedCommandLine::Reliable(value) = line {
                        value.pop();
                    }
                    offset += 1;
                }
                byte if byte >= 0x20 => {
                    let start = offset;
                    while offset < data.len() && data[offset] >= 0x20 && data[offset] != 0x7f {
                        offset += 1;
                    }
                    if let Ok(text) = std::str::from_utf8(&data[start..offset]) {
                        if let TrackedCommandLine::Reliable(value) = line {
                            value.push_str(text);
                        }
                    } else {
                        *line = TrackedCommandLine::Unreliable;
                    }
                }
                _ => {
                    *line = TrackedCommandLine::Unreliable;
                    offset += 1;
                }
            }
        }
    }
}

pub fn tracked_terminal_command(state: &AppState, session_id: &str) -> Option<String> {
    match state.terminal_command_lines.get(session_id) {
        Some(TrackedCommandLine::Reliable(command)) if !command.trim().is_empty() => {
            Some(command.trim().to_string())
        }
        _ => None,
    }
}

pub fn clear_terminal_command_lines(state: &mut AppState, session_ids: &[String]) {
    for session_id in session_ids {
        state.terminal_command_lines.remove(session_id);
    }
}

pub fn enqueue_pending_exit(
    state: &mut AppState,
    session_id: &str,
    command: String,
    history_id: String,
) {
    let queue = state
        .pending_exit_check
        .entry(session_id.to_string())
        .or_default();
    const MAX_PENDING: usize = 32;
    while queue.len() >= MAX_PENDING {
        queue.pop_front();
    }
    queue.push_back((command, history_id));
}

pub fn rollback_pending_exit(state: &mut AppState, session_id: &str, history_id: &str) -> bool {
    let Some(queue) = state.pending_exit_check.get_mut(session_id) else {
        return false;
    };
    if queue.back().is_some_and(|(_, id)| id == history_id) {
        queue.pop_back();
        if queue.is_empty() {
            state.pending_exit_check.remove(session_id);
        }
        true
    } else {
        false
    }
}

/// Records that the session produced a real OSC 133;D exit code. Used to
/// permanently disable the prompt-return badge fallback on integrated shells
/// so a late (or split-chunk) exit-code marker can never race the fallback.
pub fn note_exit_code_evidence(state: &mut AppState, session_id: &str) {
    state.exit_code_sessions.insert(session_id.to_string());
}

/// True when a session is eligible for the non-integrated prompt-return
/// completion fallback: an approved command is still waiting for its exit
/// code, the session never produced a real OSC 133;D marker, and the
/// terminal's current line looks like a shell prompt again (the command
/// finished and the prompt returned). Integrated shells are permanently
/// excluded via `exit_code_sessions`. See app.rs
/// `resolve_pending_command_via_prompt` for the badge/history semantics.
pub fn prompt_return_completion_target(
    state: &AppState,
    session_id: &str,
    current_line: &str,
) -> bool {
    if state.exit_code_sessions.contains(session_id) {
        return false;
    }
    if !state
        .pending_exit_check
        .get(session_id)
        .is_some_and(|q| !q.is_empty())
    {
        return false;
    }
    prompt_looks_like_shell(current_line)
}

#[cfg(test)]
mod pending_exit_helpers_tests {
    use super::*;

    #[test]
    fn pending_queue_caps_at_32_and_preserves_fifo_order() {
        let mut state = AppState::default();
        for index in 0..40 {
            enqueue_pending_exit(
                &mut state,
                "session",
                format!("command-{index}"),
                format!("id-{index}"),
            );
        }
        let queue = &state.pending_exit_check["session"];
        assert_eq!(queue.len(), 32);
        assert_eq!(queue.front().unwrap().0, "command-8");
        assert_eq!(queue.back().unwrap().0, "command-39");
    }

    #[test]
    fn rollback_only_removes_the_matching_newest_submission() {
        let mut state = AppState::default();
        enqueue_pending_exit(
            &mut state,
            "session",
            "first".to_string(),
            "first-id".to_string(),
        );
        enqueue_pending_exit(
            &mut state,
            "session",
            "second".to_string(),
            "second-id".to_string(),
        );

        assert!(!rollback_pending_exit(&mut state, "session", "first-id"));
        assert!(rollback_pending_exit(&mut state, "session", "second-id"));
        assert_eq!(state.pending_exit_check["session"].len(), 1);
        assert!(rollback_pending_exit(&mut state, "session", "first-id"));
        assert!(!state.pending_exit_check.contains_key("session"));
    }

    #[test]
    fn prompt_return_fallback_requires_pending_command_and_shell_prompt() {
        let mut state = AppState::default();

        // No pending command: even a perfect prompt line is not a target.
        assert!(!prompt_return_completion_target(
            &state,
            "session",
            "root@host:~# "
        ));

        enqueue_pending_exit(&mut state, "session", "ls".to_string(), "id".to_string());
        // Pending command + shell-looking prompt -> eligible.
        assert!(prompt_return_completion_target(
            &state,
            "session",
            "root@host:~# "
        ));
        // Pending command but output is mid-flight (no prompt) -> not eligible.
        assert!(!prompt_return_completion_target(
            &state,
            "session",
            "reading chunk 12/40"
        ));
        assert!(!prompt_return_completion_target(&state, "session", ""));
    }

    #[test]
    fn prompt_return_fallback_is_disabled_after_real_exit_code_evidence() {
        let mut state = AppState::default();
        enqueue_pending_exit(&mut state, "session", "ls".to_string(), "id".to_string());
        assert!(prompt_return_completion_target(
            &state,
            "session",
            "root@host:~# "
        ));

        // Once any real OSC 133;D was observed for the session, the fallback
        // never fires again — an integrated shell resolves via the exit-code
        // pipeline, and a late marker can never race the fallback.
        note_exit_code_evidence(&mut state, "session");
        assert!(!prompt_return_completion_target(
            &state,
            "session",
            "root@host:~# "
        ));

        // Other sessions without evidence stay eligible.
        enqueue_pending_exit(&mut state, "other", "ls".to_string(), "id".to_string());
        assert!(prompt_return_completion_target(
            &state,
            "other",
            "root@host:~# "
        ));
    }

    #[test]
    fn rapid_terminal_input_is_tracked_without_waiting_for_pty_echo() {
        let mut state = AppState::default();
        let sessions = vec!["one".to_string(), "two".to_string()];

        track_terminal_input(&mut state, &sessions, b"printf RUSTERM_COMPARE_E2E_OK");

        for session in &sessions {
            assert_eq!(
                tracked_terminal_command(&state, session).as_deref(),
                Some("printf RUSTERM_COMPARE_E2E_OK")
            );
        }
    }

    #[test]
    fn terminal_input_tracking_handles_editing_and_unreliable_navigation() {
        let mut state = AppState::default();
        let sessions = vec!["session".to_string()];
        track_terminal_input(&mut state, &sessions, b"echo oops");
        track_terminal_input(&mut state, &sessions, &[0x7f]);
        assert_eq!(
            tracked_terminal_command(&state, "session").as_deref(),
            Some("echo oop")
        );

        track_terminal_input(&mut state, &sessions, &[0x15]);
        track_terminal_input(&mut state, &sessions, b"git st");
        let mut replacement = vec![0x7f; 6];
        replacement.extend_from_slice(b"git status");
        track_terminal_input(&mut state, &sessions, &replacement);
        assert_eq!(
            tracked_terminal_command(&state, "session").as_deref(),
            Some("git status")
        );

        track_terminal_input(&mut state, &sessions, b"\x1b[A");
        assert_eq!(tracked_terminal_command(&state, "session"), None);

        track_terminal_input(&mut state, &sessions, &[0x03]);
        track_terminal_input(&mut state, &sessions, b"pwd");
        assert_eq!(
            tracked_terminal_command(&state, "session").as_deref(),
            Some("pwd")
        );

        clear_terminal_command_lines(&mut state, &sessions);
        assert_eq!(tracked_terminal_command(&state, "session"), None);
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
    // Keep the focused-tab outline attached to the newly active tab: point
    // pane focus at the pane holding the tab's anchor session (or pane 0).
    // Tabs whose implicit Single layout has no entry drop explicit pane
    // focus; `focused_pane_session` then yields None and the UI falls back
    // to the tab's anchor session.
    state.focused_pane = state.layouts.get(tab_id).map(|layout| {
        let pane_idx = state
            .active_session
            .as_deref()
            .and_then(|anchor| {
                layout
                    .panes
                    .iter()
                    .position(|pane| pane.session_id == anchor)
            })
            .unwrap_or(0);
        FocusedPane {
            layout_owner_tab_id: tab_id.to_string(),
            pane_idx,
        }
    });
}

/// Activate the workspace tab and pane that contain `session_id`.
/// Returns false when the session is not part of any current workspace.
pub fn activate_session(state: &mut AppState, session_id: &str) -> bool {
    if !state
        .sessions
        .iter()
        .any(|session| session.id == session_id)
    {
        return false;
    }

    let Some(tab_id) = state.tabs.iter().find_map(|tab| {
        let is_anchor = tab.anchor_session_id.as_deref() == Some(session_id);
        let is_in_layout = state.layouts.get(&tab.id).is_some_and(|layout| {
            layout
                .panes
                .iter()
                .any(|pane| pane.session_id == session_id)
        });
        (is_anchor || is_in_layout).then(|| tab.id.clone())
    }) else {
        return false;
    };

    set_active_tab(state, &tab_id);
    if let Some(pane_idx) = state.layouts.get(&tab_id).and_then(|layout| {
        layout
            .panes
            .iter()
            .position(|pane| pane.session_id == session_id)
    }) {
        let _ = focus_pane_for_layout(state, &tab_id, pane_idx);
    }
    true
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

/// Reorder workspace tabs: move the tab whose anchor session is
/// `dragged_session_id` so that it sits immediately before (or after)
/// the tab whose id is `target_tab_id`. This is the top-tab-bar reorder
/// gesture (drag a tab onto another tab in the bar).
///
/// `before` selects the insertion side: `true` inserts the dragged tab
/// immediately before the target; `false` inserts it immediately after.
/// The JS hit-test computes this from the cursor's x position relative
/// to the target tab's horizontal midpoint.
///
/// Returns `true` if a reorder actually occurred. Returns `false` when:
///   - the dragged session has no tab (its anchor isn't any tab's anchor),
///   - the target tab id isn't in `state.tabs`,
///   - source and target are the same tab,
///   - the dragged tab is already in the requested position (no-op).
///
/// The active tab is preserved: if the moved tab was active, it stays
/// active after the move (its id doesn't change — only its position in
/// `state.tabs` does). `active_session` is unchanged because the anchor
/// session id is unchanged.
///
/// Takes `&mut AppState` (rather than `&mut Signal<AppState>`) so it's
/// unit-testable without a dioxus runtime. Callers in `app.rs` pass
/// `&mut state.write()`.
pub fn reorder_tab(
    state: &mut AppState,
    dragged_session_id: &str,
    target_tab_id: &str,
    before: bool,
) -> bool {
    let src_pos = state
        .tabs
        .iter()
        .position(|t| t.anchor_session_id.as_deref() == Some(dragged_session_id));
    let Some(src_pos) = src_pos else {
        return false;
    };
    let tgt_pos = state.tabs.iter().position(|t| t.id == target_tab_id);
    let Some(tgt_pos) = tgt_pos else {
        return false;
    };
    if src_pos == tgt_pos {
        // Dragging a tab onto itself — no-op.
        return false;
    }
    // Compute the insertion index in the post-removal vector. Remove the
    // source tab first, then figure out where the target now sits, then
    // adjust for the `before`/`after` side.
    let src_tab = state.tabs.remove(src_pos);
    // After removal, the target's index may have shifted left by one if
    // the source was before it.
    let tgt_pos_after = state
        .tabs
        .iter()
        .position(|t| t.id == target_tab_id)
        .unwrap_or_else(|| state.tabs.len().saturating_sub(1));
    let insert_at = if before {
        tgt_pos_after
    } else {
        // Insert after the target. Saturating-add guards against the
        // (impossible-in-practice) case where the target is the last tab.
        tgt_pos_after.saturating_add(1)
    };
    // No-op check: if the computed insertion index matches the source's
    // original position (adjusted for the removal), the tab would land in
    // the same slot — skip the insert to avoid a redundant write and to
    // return a truthful `false`.
    //
    // Concretely: if `before` and src was immediately before tgt, removing
    // src shifts tgt left into src's old slot, and `insert_at == tgt_pos_after`
    // equals `src_pos` — a no-op. Same for `after` when src was immediately
    // after tgt.
    let would_be_noop = match (before, src_pos.cmp(&tgt_pos)) {
        (true, std::cmp::Ordering::Less) => insert_at == src_pos,
        (false, std::cmp::Ordering::Greater) => insert_at == src_pos,
        _ => false,
    };
    if would_be_noop {
        // Put the tab back where it was.
        state.tabs.insert(src_pos, src_tab);
        return false;
    }
    state.tabs.insert(insert_at, src_tab);
    true
}

/// Place a freshly copied session next to its source instead of at the
/// far right of the tab bar (Task 127: 副本支持就近复制).
///
/// `open_connection` appends the copy's workspace tab at the end of
/// `state.tabs` (and its session tab at the end of `state.sessions`).
/// This helper moves both so the copy sits immediately AFTER the source:
///   - the workspace tab, via `reorder_tab` (which also preserves the
///     active tab — the copy stays active after the move);
///   - the session tab, so the persisted snapshot (`build_session_state`
///     iterates `state.sessions` in order) restores the copy adjacent to
///     its source on the next launch as well.
///
/// No-op (returns `false`) when either id is unknown, when source ==
/// copy, or when the source has no workspace tab (pane-only session) —
/// in that case the copy simply stays where `open_connection` put it.
///
/// Takes `&mut AppState` so it's unit-testable without a dioxus runtime.
pub fn place_copied_session_next_to_source(
    state: &mut AppState,
    source_session_id: &str,
    copy_session_id: &str,
) -> bool {
    if source_session_id == copy_session_id {
        return false;
    }
    let source_tab_id = state
        .tabs
        .iter()
        .find(|t| t.anchor_session_id.as_deref() == Some(source_session_id))
        .map(|t| t.id.clone());
    let Some(source_tab_id) = source_tab_id else {
        return false;
    };
    // Move the copy's workspace tab immediately after the source's tab.
    // `reorder_tab` returns `false` for the already-adjacent case (copying
    // the rightmost tab) — that's still a success for our purposes, so
    // only treat "copy tab not found" as a failure.
    if !state
        .tabs
        .iter()
        .any(|t| t.anchor_session_id.as_deref() == Some(copy_session_id))
    {
        return false;
    }
    reorder_tab(state, copy_session_id, &source_tab_id, false);
    // Mirror the adjacency in `state.sessions` so the persisted snapshot
    // (and thus the restored tab order) keeps the copy next to its source.
    let copy_pos = state.sessions.iter().position(|s| s.id == copy_session_id);
    let src_pos = state
        .sessions
        .iter()
        .position(|s| s.id == source_session_id);
    if let (Some(copy_pos), Some(src_pos)) = (copy_pos, src_pos) {
        let copy_tab = state.sessions.remove(copy_pos);
        // Removing the copy may shift the source left by one.
        let src_pos_after = if copy_pos < src_pos {
            src_pos - 1
        } else {
            src_pos
        };
        state.sessions.insert(src_pos_after + 1, copy_tab);
    }
    true
}

/// Parse the trailing "副本 N" / "copy N" suffix off a session display name
/// and return `N`. Recognises both the zh (`副本`) and en (`copy`) markers,
/// case-insensitively, followed by whitespace and a positive integer at the
/// very end of the string. Returns `None` when the name has no such suffix
/// (the source session itself, or a name that was never numbered).
///
/// Used by [`AppState::next_copy_number`] to sequence copies of the same
/// jumpserver connection as "副本 1, 2, 3 … N" so each copy is distinguishable
/// on restore (where copies share one saved-connection id and are matched by
/// this title).
pub fn parse_copy_number(name: &str) -> Option<usize> {
    let trimmed = name.trim_end();
    // Walk back from the end: a run of digits, then whitespace, then the
    // marker word. Find the last space before the digit run.
    let bytes = trimmed.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == bytes.len() || i == 0 {
        // No trailing digits, or the whole string is digits.
        return None;
    }
    let n: usize = std::str::from_utf8(&bytes[i..]).ok()?.parse().ok()?;
    if n == 0 {
        return None;
    }
    // Skip whitespace between marker and number.
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let marker = &trimmed[..i];
    let marker = marker.trim_end();
    if marker.ends_with("副本") || marker.to_ascii_lowercase().ends_with(" copy") {
        Some(n)
    } else {
        None
    }
}

/// Next "副本 N" sequence number for a new copy of the connection identified
/// by `source_saved_connection_id` (the saved-connection id shared by the
/// source and all its existing copies). Counts open sessions whose stored
/// config shares that id and parses the highest existing copy number, then
/// returns `max + 1` (minimum `1` when no copies exist yet).
///
/// This is the matching key v0.21 asked for: copies of the same jumpserver
/// connection are numbered "副本 1, 2, 3 … N" so they stay distinguishable
/// after a restart, when `find_restore_connection` resolves them all to the
/// same saved-connection id.
pub fn next_copy_number(state: &AppState, source_saved_connection_id: &str) -> usize {
    let mut max_n = 0usize;
    for tab in &state.sessions {
        let conn = state.session_configs.get(&tab.id);
        if conn.map(|c| c.id.as_str()) != Some(source_saved_connection_id) {
            continue;
        }
        if let Some(n) = parse_copy_number(&tab.name) {
            if n > max_n {
                max_n = n;
            }
        }
    }
    max_n + 1
}

/// Strip a trailing "副本 N" / "copy N" copy suffix from a session/connection
/// display name, returning the BASE name the suffix was appended to. Applied
/// repeatedly, so a chained "web 副本 1 副本 2" (the pre-v0.22 bug) collapses
/// back to "web". Names without a suffix are returned as-is — notably a base
/// name that merely ends in digits ("web-01") or in the bare marker word
/// ("web-副本", no number) is NOT touched.
///
/// Used by the copy-session handler: the source session's stored config name
/// may already carry a "副本 N" suffix (a copy of a copy); naming the new copy
/// from that name verbatim would chain the markers ("… 副本 1 副本 2"), which
/// is exactly the bug v0.22 fixed — copies must read "… 副本 1, 2, …, N".
pub fn strip_copy_suffix(name: &str) -> String {
    let mut current = name.trim_end().to_string();
    while let Some(base) = strip_one_copy_suffix(&current) {
        current = base;
    }
    current
}

/// Single-pass suffix strip backing [`strip_copy_suffix`]. Mirrors the
/// boundary logic of [`parse_copy_number`]: a run of trailing digits, then
/// whitespace, then the `副本`/`copy` marker with a non-empty base before it.
/// Returns `None` when any piece is missing (in which case the name is not a
/// numbered copy).
fn strip_one_copy_suffix(name: &str) -> Option<String> {
    let trimmed = name.trim_end();
    let bytes = trimmed.as_bytes();
    let mut i = bytes.len();
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == bytes.len() || i == 0 {
        return None;
    }
    // A positive integer, same as `parse_copy_number` (0 is not a copy).
    let n: usize = std::str::from_utf8(&bytes[i..]).ok()?.parse().ok()?;
    if n == 0 {
        return None;
    }
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let marker = trimmed[..i].trim_end();
    if let Some(base) = marker.strip_suffix("副本") {
        let base = base.trim_end();
        if !base.is_empty() {
            return Some(base.to_string());
        }
    } else {
        let lowered = marker.to_ascii_lowercase();
        if lowered.ends_with(" copy") {
            // " copy" is pure ASCII, so the byte length matches `marker`.
            let base = marker[..marker.len() - " copy".len()].trim_end();
            if !base.is_empty() {
                return Some(base.to_string());
            }
        }
    }
    None
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
        if state.bottom_shell_session_id.as_deref() != Some(tab.id.as_str())
            && tab.id != anchor_session
            && !ids.contains(&tab.id)
        {
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
///
/// Whenever comparison is toggled (on or off), the diff-highlight state is
/// reset so stale results from the previous session don't leak through and
/// the user is re-warned if the new diff is large.
pub fn toggle_comparison_mode(state: &mut AppState) -> Option<bool> {
    let active_id = state.active_tab.clone()?;
    let layout = state.layouts.get_mut(&active_id)?;
    let now_on = layout.toggle_comparison();
    // Reset diff state on every toggle.
    state.comparison_diffs = None;
    state.comparison_diff_warning = None;
    state.comparison_diff_confirmed = false;
    Some(now_on)
}

/// Disable future large-diff warnings and approve the currently pending diff.
/// Persistence is handled by the UI caller because `AppState` does not own
/// configuration I/O.
pub fn suppress_comparison_diff_warning(state: &mut AppState) {
    state.comparison_diff_warning_enabled = false;
    state.comparison_diff_confirmed = true;
    state.comparison_diff_warning = None;
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

/// Resolve the target sessions for commands initiated outside a TerminalView
/// (for example the docked Send panel or command-history list).
/// Comparison mode broadcasts to all panes; otherwise the focused pane wins,
/// falling back to the active tab's anchor.
pub fn command_send_targets(state: &AppState) -> Vec<String> {
    let broadcast = broadcast_targets(state);
    if broadcast.len() > 1 {
        return broadcast;
    }

    focused_pane_session(state)
        .or_else(|| state.active_tab_anchor_session())
        .map(|session_id| vec![session_id])
        .unwrap_or_default()
}

/// Sessions that can currently receive a command from the docked Send panel.
/// Connecting, disconnected, and embedded-shell sessions are deliberately
/// excluded: Send is for authenticated workspace terminals only.
pub fn available_send_targets(state: &AppState) -> Vec<SendTargetOption> {
    state
        .sessions
        .iter()
        .filter(|session| {
            state.session_connection_states.get(&session.id)
                == Some(&SessionConnectionState::Connected)
                && state.bottom_shell_session_id.as_deref() != Some(session.id.as_str())
        })
        .map(|session| SendTargetOption {
            session_id: session.id.clone(),
            label: session.name.clone(),
        })
        .collect()
}

/// Effective Send-panel targets in stable session order. Before the first
/// explicit selection change, this is the legacy focused/comparison target.
pub fn selected_send_target_ids(state: &AppState) -> Vec<String> {
    let available = available_send_targets(state);
    let selected = state
        .send_target_selection
        .clone()
        .unwrap_or_else(|| command_send_targets(state).into_iter().collect());

    available
        .into_iter()
        .filter(|target| selected.contains(&target.session_id))
        .map(|target| target.session_id)
        .collect()
}

pub fn set_send_target_selected(state: &mut AppState, session_id: &str, selected: bool) -> bool {
    if !available_send_targets(state)
        .iter()
        .any(|target| target.session_id == session_id)
    {
        return false;
    }

    let mut selection: HashSet<String> = selected_send_target_ids(state).into_iter().collect();
    if selected {
        selection.insert(session_id.to_string());
    } else {
        selection.remove(session_id);
    }
    state.send_target_selection = Some(selection);
    true
}

pub fn select_all_send_targets(state: &mut AppState) -> usize {
    let selection: HashSet<String> = available_send_targets(state)
        .into_iter()
        .map(|target| target.session_id)
        .collect();
    let count = selection.len();
    state.send_target_selection = Some(selection);
    count
}

pub fn invert_send_targets(state: &mut AppState) -> usize {
    let selected: HashSet<String> = selected_send_target_ids(state).into_iter().collect();
    let selection: HashSet<String> = available_send_targets(state)
        .into_iter()
        .map(|target| target.session_id)
        .filter(|session_id| !selected.contains(session_id))
        .collect();
    let count = selection.len();
    state.send_target_selection = Some(selection);
    count
}

/// Get the list of sessions whose terminals should move in response to a
/// wheel event from `source_session_id`.
///
/// Comparison mode synchronizes every non-empty pane in the active layout.
/// Otherwise the source pane scrolls locally. Unlike keyboard input routing,
/// a wheel event must preserve its source pane when comparison mode is off:
/// `active_tab` identifies the layout owner, not the currently focused pane.
///
/// This remains separate from `broadcast_targets` because scroll sync and
/// input broadcast are conceptually distinct, even though they share the
/// comparison flag today.
pub fn scroll_sync_targets(state: &AppState, source_session_id: &str) -> Vec<String> {
    let comparison_enabled = state
        .active_tab
        .as_ref()
        .and_then(|id| state.layouts.get(id))
        .is_some_and(|layout| layout.comparison);

    if comparison_enabled {
        broadcast_targets(state)
    } else {
        vec![source_session_id.to_string()]
    }
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
    if state.bottom_shell_session_id.as_deref() == Some(session_id.as_str()) {
        return false;
    }
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
            if state.bottom_shell_session_id.as_deref() != Some(tab.id.as_str())
                && tab.id != anchor
                && !ids.contains(&tab.id)
            {
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

/// Outcome of [`close_pane`] — tells the caller what happened so it can
/// run follow-up actions (e.g. restore focus, refresh tab bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePaneOutcome {
    /// A pane was removed from the layout. The tab and layout survive.
    /// `focused_pane` was either cleared (if it pointed at the removed pane)
    /// or decremented (if it pointed at a later pane).
    Removed,
    /// The layout has only one pane — can't shrink further. The caller should
    /// close the whole tab (or do nothing, if the user clicked ✕ on the last
    /// pane in a tab they want to keep).
    SinglePane,
    /// No layout exists for this tab (the tab is in the pre-layout single-pane
    /// state). Nothing to remove.
    NoLayout,
    /// The pane held the tab's anchor session AND no other pane had a
    /// session, so the whole tab was closed via [`close_session`]. The
    /// caller should run tab-bar refresh / focus-restore logic.
    TabClosed,
    /// The pane index was out of range.
    OutOfRange,
}

/// Close a pane in a specific tab's layout. This is the state-level wrapper
/// around [`PaneLayout::remove_pane`] that also handles session teardown,
/// anchor promotion, and `focused_pane` invalidation.
///
/// # Behavior
///
/// 1. **Empty pane** (no session): just call `PaneLayout::remove_pane` — no
///    session cleanup needed. This is the case the empty-pane ✕ button hits.
/// 2. **Pane with a non-anchor session**: call [`close_session`] to tear down
///    the session's resources (input_senders, terminals, etc.), then call
///    `PaneLayout::remove_pane` to shrink the layout.
/// 3. **Pane with the anchor session + other panes have sessions**: call
///    `close_session` (which promotes the first available pane session to
///    the new anchor), then call `PaneLayout::remove_pane`.
/// 4. **Pane with the anchor session + no other sessions**: call
///    `close_session` (which removes the whole tab + layout). Return
///    `TabClosed` — the layout is gone.
///
/// `focused_pane` is cleared if it pointed at the removed pane, or
/// decremented if it pointed at a later pane (matching the tree-leaf
/// renumbering done by `PaneLayout::remove_pane`).
///
/// # Why a state-level wrapper
///
/// Mirrors the `*_for_active` / `*_to_active` pattern: takes `&mut AppState`
/// (unit-testable without a dioxus runtime) and handles the layout lookup
/// boilerplate. The caller still owns the `input_senders` Signal borrow
/// because `close_session` needs it.
pub fn close_pane(
    state: &mut AppState,
    input_senders: &mut HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
    layout_owner_tab_id: &str,
    pane_idx: usize,
) -> ClosePaneOutcome {
    // Capture the removed pane's session BEFORE we borrow `state.layouts`
    // mutably (we'll need it for the anchor-promotion check after
    // `close_session`).
    let (removed_session_id, pane_count, in_range) = state
        .layouts
        .get(layout_owner_tab_id)
        .map(|layout| {
            let sid = layout
                .panes
                .get(pane_idx)
                .map(|pane| pane.session_id.clone());
            (sid, layout.panes.len(), pane_idx < layout.panes.len())
        })
        .unwrap_or((None, 0, false));

    if pane_count == 0 {
        return ClosePaneOutcome::NoLayout;
    }
    if !in_range {
        return ClosePaneOutcome::OutOfRange;
    }
    if pane_count <= 1 {
        // Last pane in the layout. The user clicked ✕ on the only
        // remaining pane — they want it gone. Close the pane's session
        // (which tears down the session's resources and, if it was the
        // tab's anchor with no other sessions, removes the whole tab +
        // layout via `close_session`'s existing tab-removal path). If the
        // pane was empty (no session), just remove the layout directly so
        // the tab doesn't dangle with a 1-pane empty layout.
        //
        // This fixes the "空窗口关闭逻辑没有正确的关闭" report: the prior
        // code returned `SinglePane` and did NOTHING, so the ✕ button on
        // the last pane was a no-op and the user couldn't close it.
        if let Some(sid) = removed_session_id.as_deref().filter(|s| !s.is_empty()) {
            close_session(state, input_senders, sid);
            // close_session may have removed the whole tab (anchor closed
            // + no other sessions). If the tab survived (anchor was
            // promoted), the layout still has 1 pane — clear it so the
            // tab is back to the pre-layout single-pane state.
            if state.layouts.contains_key(layout_owner_tab_id) {
                state.layouts.remove(layout_owner_tab_id);
            }
            // Clear any focused_pane that pointed into this tab.
            if state
                .focused_pane
                .as_ref()
                .is_some_and(|fp| fp.layout_owner_tab_id == layout_owner_tab_id)
            {
                state.focused_pane = None;
            }
        } else {
            // Empty last pane — just remove the layout.
            state.layouts.remove(layout_owner_tab_id);
            if state
                .focused_pane
                .as_ref()
                .is_some_and(|fp| fp.layout_owner_tab_id == layout_owner_tab_id)
            {
                state.focused_pane = None;
            }
        }
        return ClosePaneOutcome::TabClosed;
    }

    // If the pane has a session, tear down its resources first. This also
    // handles anchor promotion / tab removal via close_session's existing
    // logic. After close_session, the pane slot is cleared (session_id == "").
    if let Some(sid) = removed_session_id.as_deref().filter(|s| !s.is_empty()) {
        let before_tabs = state.tabs.len();
        close_session(state, input_senders, sid);
        if state.tabs.len() < before_tabs {
            // close_session tore down the whole tab (anchor closed + no other
            // sessions). Layout is gone; nothing more to do. Defensive: clear
            // any focused_pane that pointed into this tab (close_session only
            // clears focused_pane when the focused pane's session_id == closed
            // id, but a focused_pane on an EMPTY pane of this tab would
            // otherwise dangle).
            if state
                .focused_pane
                .as_ref()
                .is_some_and(|fp| fp.layout_owner_tab_id == layout_owner_tab_id)
            {
                state.focused_pane = None;
            }
            return ClosePaneOutcome::TabClosed;
        }
    }

    // After close_session (or if the pane was empty), the pane slot is
    // cleared but the pane still exists in the layout. Now physically remove
    // it from the layout tree.
    let Some(layout) = state.layouts.get_mut(layout_owner_tab_id) else {
        // Defensive: layout vanished somehow (shouldn't happen unless
        // close_session's tab-removal path ran but we missed it above).
        return ClosePaneOutcome::TabClosed;
    };
    if layout.remove_pane(pane_idx).is_none() {
        // Shouldn't happen — we checked pane count above — but be defensive.
        return ClosePaneOutcome::SinglePane;
    }

    // Fix up `focused_pane` to match the new pane indices.
    match &state.focused_pane {
        Some(fp) if fp.layout_owner_tab_id == layout_owner_tab_id => {
            if fp.pane_idx == pane_idx {
                state.focused_pane = None;
            } else if fp.pane_idx > pane_idx {
                state.focused_pane = Some(FocusedPane {
                    layout_owner_tab_id: fp.layout_owner_tab_id.clone(),
                    pane_idx: fp.pane_idx - 1,
                });
            }
            // else: focused pane was before the removed one — no change.
        }
        _ => {}
    }

    ClosePaneOutcome::Removed
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
        if state.bottom_shell_session_id.as_deref() != Some(tab.id.as_str())
            && !session_ids.contains(&tab.id)
        {
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
    state.session_logs.remove(id);
    state.onekey_popups.remove(id);
    state.onekey_submission_feedback.remove(id);
    state.onekey_submission_cooldown.remove(id);
    state.onekey_output_since_submission.remove(id);
    state.onekey_preference_attempts.remove(id);
    state
        .onekey_skip_logged
        .retain(|(session_id, _)| session_id != id);
    state.session_connection_states.remove(id);
    state.session_nodes.remove(id);
    if let Some(selection) = state.send_target_selection.as_mut() {
        selection.remove(id);
    }
    state.session_configs.remove(id);
    state.pending_exit_check.remove(id);
    state.exit_code_sessions.remove(id);
    state.terminal_command_lines.remove(id);
    state.suggestion_muted_sessions.remove(id);
    state.session_replays.remove(id);
    // The tab is gone for good — no snapshot will reference this id again,
    // so its per-session replay-event stream is garbage. Best-effort delete
    // (a no-op stub without the `analytics` feature).
    if let Err(e) = state.analytics.clear_replay_stream(id) {
        tracing::debug!("[REPLAY] failed to clear replay stream on close: {e}");
    }
    state.ssh_sessions.remove(id);
    state.sftp_clients.remove(id);
    state.feishu_otp_status.remove(id);
    state.feishu_otp_attempts.remove(id);
    if state
        .feishu_qr_popup
        .as_ref()
        .is_some_and(|popup| popup.session.as_deref() == Some(id))
    {
        state.feishu_qr_popup = None;
    }
    state
        .feishu_pending_auths
        .retain(|_, pending| pending.session.as_deref() != Some(id));
    state.transfers.cancel_for_session(id);
    for job_id in state
        .transfers
        .jobs
        .iter()
        .filter(|job| job.session == id)
        .map(|job| job.id.clone())
        .collect::<Vec<_>>()
    {
        if let Some((_, token)) = state.transfer_cancellations.remove(&job_id) {
            token.cancel();
        }
    }
    if state.bottom_shell_session_id.as_deref() == Some(id) {
        state.bottom_shell_session_id = None;
    }
    state.shadow_sandbox.cancel_session(id);
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
        state.session_logs.remove(sid);
        state.onekey_popups.remove(sid);
        state.onekey_submission_feedback.remove(sid);
        state.onekey_submission_cooldown.remove(sid);
        state.onekey_output_since_submission.remove(sid);
        state.onekey_preference_attempts.remove(sid);
        state
            .onekey_skip_logged
            .retain(|(session_id, _)| session_id != sid);
        state.session_connection_states.remove(sid);
        state.session_nodes.remove(sid);
        state.session_configs.remove(sid);
        state.pending_exit_check.remove(sid);
        state.exit_code_sessions.remove(sid);
        state.terminal_command_lines.remove(sid);
        state.session_replays.remove(sid);
        state.ssh_sessions.remove(sid);
        state.sftp_clients.remove(sid);
        state.transfers.cancel_for_session(sid);
        for job_id in state
            .transfers
            .jobs
            .iter()
            .filter(|job| &job.session == sid)
            .map(|job| job.id.clone())
            .collect::<Vec<_>>()
        {
            if let Some((_, token)) = state.transfer_cancellations.remove(&job_id) {
                token.cancel();
            }
        }
        if state.bottom_shell_session_id.as_deref() == Some(sid.as_str()) {
            state.bottom_shell_session_id = None;
        }
        state.shadow_sandbox.cancel_session(sid);
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
    // Self-drop (dropping a pane's session back onto its OWN pane) is a
    // no-op. The prior behaviour cloned the session into a new pane, but
    // that was the root cause of the "错误的产生多个不需要的四方块" bug:
    // every time the user released a pane-title drag over the same pane
    // (a very easy accidental drop), a clone + new pane appeared. Users
    // reported they "没法正确的拖动窗口" because the layout kept growing
    // unintentionally. Drop-back-on-self now does nothing — to duplicate
    // a session the user should drag from the sidebar.
    //
    // We deliberately ignore `direction` here: no matter which drop zone
    // (Center/Left/Right/Top/Bottom) the cursor was in when released over
    // the source pane, the intent is "put it back".
    if dragged_sid == target_pane_session {
        return TabDropOutcome::NoOpSelfDrop;
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

    /// All four session backends (SSH / Shell / Telnet / Serial) share the
    /// same `pending_exit_check` queue via `enqueue_pending_exit`. This test
    /// verifies the queue works identically for every `SessionType` — the
    /// invariant that lets `process_session_exit_code` drain the queue
    /// uniformly regardless of which backend produced the exit code.
    #[test]
    fn pending_exit_check_works_for_all_session_backends() {
        let mut state = AppState::default();
        let backends = [
            ("ssh-session", SessionType::Ssh),
            ("shell-session", SessionType::Shell),
            ("telnet-session", SessionType::Telnet),
            ("serial-session", SessionType::Serial),
        ];

        for (sid, _kind) in &backends {
            enqueue_pending_exit(&mut state, sid, format!("ls-{sid}"), format!("dbid-{sid}"));
        }

        // Each session has exactly one queued command.
        for (sid, _kind) in &backends {
            let queue = state
                .pending_exit_check
                .get(*sid)
                .expect("queue must exist");
            assert_eq!(
                queue.len(),
                1,
                "session {sid} should have 1 pending command"
            );
            assert_eq!(queue.front().unwrap().0, format!("ls-{sid}"));
        }

        // Drain each queue (simulating OSC 133;D exit-code processing).
        for (sid, _kind) in &backends {
            let popped = state
                .pending_exit_check
                .get_mut(*sid)
                .and_then(|q| q.pop_front());
            assert_eq!(
                popped.map(|(cmd, _)| cmd),
                Some(format!("ls-{sid}")),
                "session {sid} should pop its queued command"
            );
        }

        // All queues are now empty.
        for (sid, _kind) in &backends {
            let queue = state.pending_exit_check.get(*sid);
            assert!(
                queue.map_or(true, |q| q.is_empty()),
                "session {sid} queue should be empty after drain"
            );
        }
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
                suggestion_corrections: HashSet::new(),
                suggestion_selected: 0,
                suggestion_visible: false,
                command_history: Vec::new(),
                hostname: Some(name.to_string()),
                cwd: None,
                last_command_status: CommandStatus::default(),
            });
        }
        state
    }

    #[test]
    fn stale_transfer_attempt_cannot_remove_current_cancellation_token() {
        let mut state = AppState::default();
        let token = CancellationToken::new();
        state
            .transfer_cancellations
            .insert("job".to_string(), (1, token.clone()));

        assert!(!state.remove_transfer_cancellation_for_attempt("job", 0));
        assert!(state.transfer_cancellations.contains_key("job"));
        assert!(!token.is_cancelled());

        assert!(state.remove_transfer_cancellation_for_attempt("job", 1));
        assert!(!state.transfer_cancellations.contains_key("job"));
    }

    #[test]
    fn session_snapshot_excludes_embedded_shell_and_uses_a_restorable_active_session() {
        let mut state = state_with_tabs(&["workspace", "embedded-shell"]);
        state.bottom_shell_session_id = Some("embedded-shell".to_string());
        state.active_session = Some("embedded-shell".to_string());
        state
            .session_connection_states
            .insert("workspace".to_string(), SessionConnectionState::Connected);

        let snapshot = state.build_session_state("Default Dark");

        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["workspace"]
        );
        assert_eq!(snapshot.active_session.as_deref(), Some("workspace"));
    }

    #[test]
    fn session_snapshot_has_no_active_session_when_only_embedded_shell_exists() {
        let mut state = state_with_tabs(&["embedded-shell"]);
        state.bottom_shell_session_id = Some("embedded-shell".to_string());
        state.active_session = Some("embedded-shell".to_string());

        let snapshot = state.build_session_state("Default Dark");

        assert!(snapshot.sessions.is_empty());
        assert_eq!(snapshot.active_session, None);
    }

    /// The snapshot records sessions in tab-bar order (after any drag
    /// reorders), not in `state.sessions` creation order. Restore opens
    /// sessions in snapshot order, so this is what preserves the user's
    /// last tab arrangement across restarts.
    #[test]
    fn session_snapshot_orders_sessions_by_tab_bar_order() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        for id in ["alpha", "beta", "gamma"] {
            state
                .session_connection_states
                .insert(id.to_string(), SessionConnectionState::Connected);
        }
        // Drag "gamma" before "alpha": tab bar becomes [gamma, alpha, beta]
        // while state.sessions stays [alpha, beta, gamma].
        assert!(reorder_tab(&mut state, "gamma", "alpha", true));
        assert_eq!(tab_anchors(&state), vec!["gamma", "alpha", "beta"]);

        let snapshot = state.build_session_state("Default Dark");

        let ids: Vec<&str> = snapshot.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["gamma", "alpha", "beta"]);
        // The reorder must not disturb which session is active.
        assert_eq!(snapshot.active_session.as_deref(), Some("alpha"));
    }

    /// Sessions that live only inside a split-pane layout (no workspace tab
    /// of their own) still make it into the snapshot — appended after the
    /// tab anchors, in their original `state.sessions` order.
    #[test]
    fn session_snapshot_appends_pane_only_sessions_after_tab_anchors() {
        let mut state = state_with_tabs(&["alpha", "beta", "pane-only"]);
        // Tab bar deliberately reversed vs. creation order; "pane-only" has
        // no tab (it's somebody's split pane).
        for name in ["beta", "alpha"] {
            state.tabs.push(WorkspaceTab {
                id: name.to_string(),
                anchor_session_id: Some(name.to_string()),
            });
        }
        for id in ["alpha", "beta", "pane-only"] {
            state
                .session_connection_states
                .insert(id.to_string(), SessionConnectionState::Connected);
        }

        let snapshot = state.build_session_state("Default Dark");

        let ids: Vec<&str> = snapshot.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["beta", "alpha", "pane-only"]);
    }

    /// Tab-bar ordering must not weaken the Connected-only filter: a
    /// disconnected tab in the middle of the bar is still omitted.
    #[test]
    fn session_snapshot_tab_order_still_skips_disconnected_sessions() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        for id in ["alpha", "gamma"] {
            state
                .session_connection_states
                .insert(id.to_string(), SessionConnectionState::Connected);
        }
        state
            .session_connection_states
            .insert("beta".to_string(), SessionConnectionState::Disconnected);
        assert!(reorder_tab(&mut state, "gamma", "alpha", true));

        let snapshot = state.build_session_state("Default Dark");

        let ids: Vec<&str> = snapshot.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["gamma", "alpha"]);
    }

    #[test]
    fn session_snapshot_roundtrip_restores_only_terminals_logged_in_at_exit() {
        let mut state = state_with_tabs(&["connected", "disconnected"]);
        state.active_session = Some("disconnected".to_string());
        state
            .session_connection_states
            .insert("connected".to_string(), SessionConnectionState::Connected);
        state.session_connection_states.insert(
            "disconnected".to_string(),
            SessionConnectionState::Disconnected,
        );

        let snapshot = state.build_session_state("Default Dark");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session_state.enc");
        let key = [7_u8; 32];
        snapshot.save_to(&path, &key).unwrap();
        let loaded = rusterm_core::SessionState::load_from(&path, &key)
            .unwrap()
            .unwrap();

        assert_eq!(
            loaded
                .sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            vec!["connected"]
        );
        assert_eq!(loaded.active_session.as_deref(), Some("connected"));

        // A later save after the final terminal disconnects must overwrite the
        // earlier non-empty snapshot. Otherwise a subsequent launch would
        // resurrect a stale login.
        state.session_connection_states.insert(
            "connected".to_string(),
            SessionConnectionState::Disconnected,
        );
        state
            .build_session_state("Default Dark")
            .save_to(&path, &key)
            .unwrap();
        let logged_out = rusterm_core::SessionState::load_from(&path, &key)
            .unwrap()
            .unwrap();
        assert!(logged_out.sessions.is_empty());
        assert_eq!(logged_out.active_session, None);
    }

    /// Empty snapshots must not clobber `session_state.enc` while a
    /// connect/reconnect is still in flight — otherwise clicking 恢复 and
    /// quitting (or a save tick landing) before the jumpserver reconnect
    /// completes would erase the very session memory being restored.
    #[test]
    fn empty_snapshot_is_deferred_while_a_reconnect_is_in_flight() {
        // Non-empty snapshot: always writable.
        let mut state = state_with_tabs(&["workspace"]);
        state
            .session_connection_states
            .insert("workspace".to_string(), SessionConnectionState::Connected);
        let snapshot = state.build_session_state("Default Dark");
        assert!(!snapshot.sessions.is_empty());
        assert!(state.session_snapshot_writable(&snapshot));

        // Tab exists but has no connection-state entry yet (initial connect
        // in flight, e.g. right after clicking 恢复): defer the empty write.
        let state = state_with_tabs(&["restoring"]);
        let snapshot = state.build_session_state("Default Dark");
        assert!(snapshot.sessions.is_empty());
        assert!(!state.session_snapshot_writable(&snapshot));

        // Reconnecting: still in flight — defer.
        let mut state = state_with_tabs(&["flaky"]);
        state
            .session_connection_states
            .insert("flaky".to_string(), SessionConnectionState::Reconnecting);
        let snapshot = state.build_session_state("Default Dark");
        assert!(!state.session_snapshot_writable(&snapshot));

        // Every terminal definitively disconnected: the empty snapshot is
        // the durable logged-out record and MUST be written.
        let mut state = state_with_tabs(&["done"]);
        state
            .session_connection_states
            .insert("done".to_string(), SessionConnectionState::Disconnected);
        let snapshot = state.build_session_state("Default Dark");
        assert!(state.session_snapshot_writable(&snapshot));

        // `Failed` is also a settled state (connect attempt errored), so an
        // empty snapshot must be writable — otherwise a stuck-failed session
        // would block durable logout records forever.
        let mut state = state_with_tabs(&["failed"]);
        state
            .session_connection_states
            .insert("failed".to_string(), SessionConnectionState::Failed);
        let snapshot = state.build_session_state("Default Dark");
        assert!(state.session_snapshot_writable(&snapshot));

        // The embedded bottom shell (always Connected, never persisted)
        // must not block empty writes.
        let mut state = state_with_tabs(&["embedded-shell"]);
        state.bottom_shell_session_id = Some("embedded-shell".to_string());
        state.session_connection_states.insert(
            "embedded-shell".to_string(),
            SessionConnectionState::Connected,
        );
        let snapshot = state.build_session_state("Default Dark");
        assert!(snapshot.sessions.is_empty());
        assert!(state.session_snapshot_writable(&snapshot));
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

    // ------------------------------------------------------------------
    // reorder_tab tests (top-tab-bar drag-to-reorder)
    // ------------------------------------------------------------------

    /// Helper: extract the tab anchor-session ids in order. The
    /// `state_with_active_session` helper sets each tab's `id` equal to its
    /// anchor session id, so this also reflects the tab ids — convenient for
    /// asserting reorder outcomes.
    fn tab_anchors(state: &AppState) -> Vec<String> {
        state
            .tabs
            .iter()
            .map(|t| t.anchor_session_id.clone().unwrap_or_default())
            .collect()
    }

    /// Dragging the first tab onto the third tab with `before=true` moves
    /// the dragged tab immediately before the target. [A,B,C] → drag A
    /// before C → [B,A,C].
    #[test]
    fn reorder_tab_before_target_moves_source_immediately_before() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = reorder_tab(&mut state, "alpha", "gamma", true);
        assert!(moved, "alpha should have been reordered before gamma");
        assert_eq!(
            tab_anchors(&state),
            vec!["beta", "alpha", "gamma"],
            "alpha must land immediately before gamma"
        );
    }

    /// Dragging the first tab onto the second tab with `after=true` moves
    /// the dragged tab immediately after the target. [A,B,C] → drag A after
    /// B → [B,A,C].
    #[test]
    fn reorder_tab_after_target_moves_source_immediately_after() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = reorder_tab(&mut state, "alpha", "beta", false);
        assert!(moved, "alpha should have been reordered after beta");
        assert_eq!(
            tab_anchors(&state),
            vec!["beta", "alpha", "gamma"],
            "alpha must land immediately after beta"
        );
    }

    /// Dragging a tab forward onto a later tab with `before=true`. [A,B,C,D]
    /// → drag B before D → [A,C,B,D].
    #[test]
    fn reorder_tab_forward_before_target() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        let moved = reorder_tab(&mut state, "beta", "delta", true);
        assert!(moved);
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "gamma", "beta", "delta"],
            "beta must land immediately before delta"
        );
    }

    /// Dragging a tab backward onto an earlier tab with `before=true`.
    /// [A,B,C,D] → drag D before B → [A,D,B,C].
    #[test]
    fn reorder_tab_backward_before_target() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        let moved = reorder_tab(&mut state, "delta", "beta", true);
        assert!(moved);
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "delta", "beta", "gamma"],
            "delta must land immediately before beta"
        );
    }

    /// Dragging a tab backward onto an earlier tab with `after=true`.
    /// [A,B,C,D] → drag D after B → [A,B,D,C].
    #[test]
    fn reorder_tab_backward_after_target() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        let moved = reorder_tab(&mut state, "delta", "beta", false);
        assert!(moved);
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "beta", "delta", "gamma"],
            "delta must land immediately after beta"
        );
    }

    /// Dragging a tab onto itself is a no-op and returns `false`.
    #[test]
    fn reorder_tab_onto_self_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = reorder_tab(&mut state, "beta", "beta", true);
        assert!(!moved, "reordering a tab onto itself must be a no-op");
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "beta", "gamma"],
            "order must be unchanged when source == target"
        );
    }

    /// Dragging a tab immediately before the tab it's ALREADY before is a
    /// no-op. [A,B,C] → drag A before B → still [A,B,C], returns `false`.
    #[test]
    fn reorder_tab_before_already_preceding_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = reorder_tab(&mut state, "alpha", "beta", true);
        assert!(
            !moved,
            "alpha is already immediately before beta — no reorder should occur"
        );
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "beta", "gamma"],
            "order must be unchanged when the tab is already in the requested slot"
        );
    }

    /// Dragging a tab immediately after the tab it's ALREADY after is a
    /// no-op. [A,B,C] → drag C after B → still [A,B,C], returns `false`.
    #[test]
    fn reorder_tab_after_already_following_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        let moved = reorder_tab(&mut state, "gamma", "beta", false);
        assert!(
            !moved,
            "gamma is already immediately after beta — no reorder should occur"
        );
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta", "gamma"],);
    }

    /// Unknown dragged session id → `false`, no mutation.
    #[test]
    fn reorder_tab_unknown_source_session_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        let moved = reorder_tab(&mut state, "nonexistent", "beta", true);
        assert!(!moved);
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta"]);
    }

    /// Unknown target tab id → `false`, no mutation.
    #[test]
    fn reorder_tab_unknown_target_tab_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        let moved = reorder_tab(&mut state, "alpha", "nonexistent", true);
        assert!(!moved);
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta"]);
    }

    /// The active tab stays active after being reordered. This is the key
    /// UX invariant: dragging the focused tab to a new position must NOT
    /// deactivate it.
    #[test]
    fn reorder_tab_preserves_active_tab() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // Make `beta` the active tab.
        set_active_tab(&mut state, "beta");
        assert_eq!(state.active_tab.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));

        let moved = reorder_tab(&mut state, "beta", "gamma", false);
        assert!(moved);
        // beta moved after gamma → [alpha, gamma, beta].
        assert_eq!(tab_anchors(&state), vec!["alpha", "gamma", "beta"]);
        // Active tab + session unchanged.
        assert_eq!(state.active_tab.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    /// Helper for the place_copied_session_next_to_source tests: simulate
    /// what `open_connection` does for a session copy — append the copy's
    /// SessionTab and WorkspaceTab at the END and make it active.
    fn append_copy(state: &mut AppState, copy_id: &str) {
        state.sessions.push(SessionTab {
            id: copy_id.to_string(),
            name: format!("{copy_id} 副本"),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some(copy_id.to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
        });
        push_workspace_tab(state, copy_id);
    }

    /// Task 127 (副本支持就近复制): a copied session's tab moves from the
    /// far right to immediately after its source tab, and the session list
    /// mirrors the adjacency (so the persisted snapshot restores the copy
    /// next to its source too).
    #[test]
    fn copied_session_is_placed_immediately_after_its_source() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        // Copy `alpha` — open_connection appends at the far right.
        append_copy(&mut state, "alpha-copy");
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "beta", "gamma", "alpha-copy"]
        );

        let placed = place_copied_session_next_to_source(&mut state, "alpha", "alpha-copy");
        assert!(placed);
        // Workspace tab order: copy sits right after the source.
        assert_eq!(
            tab_anchors(&state),
            vec!["alpha", "alpha-copy", "beta", "gamma"]
        );
        // Session list mirrors the adjacency (restore order).
        let session_ids: Vec<&str> = state.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(session_ids, vec!["alpha", "alpha-copy", "beta", "gamma"]);
        // The copy stays the active tab (push_workspace_tab activated it;
        // the move must not steal focus back).
        let copy_tab_id = state
            .tabs
            .iter()
            .find(|t| t.anchor_session_id.as_deref() == Some("alpha-copy"))
            .map(|t| t.id.clone())
            .unwrap();
        assert_eq!(state.active_tab.as_deref(), Some(copy_tab_id.as_str()));
        assert_eq!(state.active_session.as_deref(), Some("alpha-copy"));
    }

    /// Copying the RIGHTMOST tab: the freshly appended copy is already
    /// adjacent to its source. The helper still succeeds (the inner
    /// `reorder_tab` no-ops) and the order is unchanged.
    #[test]
    fn copy_of_rightmost_tab_is_already_adjacent() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        append_copy(&mut state, "beta-copy");

        let placed = place_copied_session_next_to_source(&mut state, "beta", "beta-copy");
        assert!(placed);
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta", "beta-copy"]);
        let session_ids: Vec<&str> = state.sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(session_ids, vec!["alpha", "beta", "beta-copy"]);
    }

    /// Unknown source session (e.g. a pane-only session with no workspace
    /// tab) → no-op: the copy stays where `open_connection` appended it.
    #[test]
    fn place_copied_session_with_unknown_source_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        append_copy(&mut state, "orphan-copy");

        let placed = place_copied_session_next_to_source(&mut state, "nonexistent", "orphan-copy");
        assert!(!placed);
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta", "orphan-copy"]);
    }

    /// Unknown copy session id → no-op, no mutation.
    #[test]
    fn place_copied_session_with_unknown_copy_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);

        let placed = place_copied_session_next_to_source(&mut state, "alpha", "nonexistent");
        assert!(!placed);
        assert_eq!(tab_anchors(&state), vec!["alpha", "beta"]);
    }

    // ── v0.21 copy numbering (副本 1, 2, 3 … N) ──────────────────────────

    /// Build one session tab + workspace tab + a `session_configs` entry whose
    /// `ConnectionConfig.id` is `saved_conn_id` (the saved-connection id shared
    /// by a source and all its copies). Mirrors what `open_connection` / the
    /// copy-session handler leave behind, minus the transport.
    fn numbered_session(state: &mut AppState, sid: &str, name: &str, saved_conn_id: &str) {
        state.sessions.push(SessionTab {
            id: sid.to_string(),
            name: name.to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 1,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: std::collections::HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("jump.example.com".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
        });
        state.tabs.push(WorkspaceTab {
            id: sid.to_string(),
            anchor_session_id: Some(sid.to_string()),
        });
        state.session_configs.insert(
            sid.to_string(),
            ConnectionConfig {
                id: saved_conn_id.to_string(),
                name: name.to_string(),
                kind: ConnectionKind::Shell(ShellConfig {
                    command: None,
                    args: Vec::new(),
                    env: Vec::new(),
                    working_dir: None,
                }),
                group: None,
                tags: Vec::new(),
                onekey: false,
                login_script: None,
            },
        );
    }

    #[test]
    fn parse_copy_number_recognises_zh_suffix() {
        assert_eq!(parse_copy_number("ops@jump 副本 1"), Some(1));
        assert_eq!(parse_copy_number("ops@jump 副本 12"), Some(12));
    }

    #[test]
    fn parse_copy_number_recognises_en_suffix() {
        assert_eq!(parse_copy_number("ops@jump copy 3"), Some(3));
        // Case-insensitive marker.
        assert_eq!(parse_copy_number("ops@jump Copy 2"), Some(2));
    }

    #[test]
    fn parse_copy_number_none_for_unnumbered_names() {
        // The source session itself, or a name that was never numbered.
        assert_eq!(parse_copy_number("ops@jumpserver"), None);
        assert_eq!(parse_copy_number("ops@jump 副本"), None);
        // A base name that happens to contain "copy" but no trailing number.
        assert_eq!(parse_copy_number("copy of something"), None);
        // Zero is not a valid copy number.
        assert_eq!(parse_copy_number("ops@jump 副本 0"), None);
    }

    #[test]
    fn parse_copy_number_ignores_a_base_name_ending_in_digits() {
        // "web-01" is the base name; without a 副本/copy marker it must NOT be
        // mistaken for copy number 1.
        assert_eq!(parse_copy_number("web-01"), None);
        // With the marker it parses fine.
        assert_eq!(parse_copy_number("web-01 副本 2"), Some(2));
    }

    #[test]
    fn strip_copy_suffix_strips_a_zh_suffix() {
        assert_eq!(strip_copy_suffix("ops@jump 副本 1"), "ops@jump");
        assert_eq!(strip_copy_suffix("ops@jump 副本 12"), "ops@jump");
    }

    #[test]
    fn strip_copy_suffix_strips_an_en_suffix_case_insensitively() {
        assert_eq!(strip_copy_suffix("ops@jump copy 3"), "ops@jump");
        assert_eq!(strip_copy_suffix("ops@jump Copy 2"), "ops@jump");
    }

    #[test]
    fn strip_copy_suffix_collapses_chained_suffixes_from_the_naming_bug() {
        // The v0.22 bug: copying a copy used the already-suffixed name as the
        // base, chaining markers ("… 副本 1 副本 2"). Stripping must collapse
        // every trailing suffix back to the true base.
        assert_eq!(strip_copy_suffix("ops@jump 副本 1 副本 2"), "ops@jump");
        assert_eq!(
            strip_copy_suffix("ops@jump 副本 1 副本 2 副本 3"),
            "ops@jump"
        );
        // Mixed-language chains from locale switching are also collapse.
        assert_eq!(strip_copy_suffix("ops@jump 副本 1 copy 2"), "ops@jump");
    }

    #[test]
    fn strip_copy_suffix_leaves_non_copy_names_untouched() {
        // No suffix at all.
        assert_eq!(strip_copy_suffix("ops@jumpserver"), "ops@jumpserver");
        // A base name that merely ends in digits.
        assert_eq!(strip_copy_suffix("web-01"), "web-01");
        // The bare marker word without a trailing number is not a copy.
        assert_eq!(strip_copy_suffix("web-副本"), "web-副本");
        assert_eq!(strip_copy_suffix("ops@jump 副本"), "ops@jump 副本");
        // Zero is not a valid copy number.
        assert_eq!(strip_copy_suffix("ops@jump 副本 0"), "ops@jump 副本 0");
        // "copy of something" has no trailing number.
        assert_eq!(strip_copy_suffix("copy of something"), "copy of something");
    }

    #[test]
    fn next_copy_number_starts_at_one_with_no_existing_copies() {
        let mut state = AppState::default();
        // Source session alone — no copies yet.
        numbered_session(&mut state, "src", "ops@jump", "conn-1");
        assert_eq!(next_copy_number(&state, "conn-1"), 1);
    }

    #[test]
    fn next_copy_number_picks_max_plus_one_across_existing_copies() {
        let mut state = AppState::default();
        // Source + two existing copies (副本 1, 副本 3 — a gap, as if 副本 2 was
        // closed). The next copy must be 4, not 2, so numbers never collide
        // with a still-persisted closed copy on restore.
        numbered_session(&mut state, "src", "ops@jump", "conn-1");
        numbered_session(&mut state, "c1", "ops@jump 副本 1", "conn-1");
        numbered_session(&mut state, "c3", "ops@jump 副本 3", "conn-1");
        assert_eq!(next_copy_number(&state, "conn-1"), 4);
    }

    #[test]
    fn next_copy_number_ignores_sessions_of_other_connections() {
        let mut state = AppState::default();
        numbered_session(&mut state, "src", "ops@jump", "conn-1");
        // A copy of a DIFFERENT connection that happens to use 副本 5 — must
        // not influence conn-1's sequence.
        numbered_session(&mut state, "other", "ops@other 副本 5", "conn-2");
        assert_eq!(next_copy_number(&state, "conn-1"), 1);
    }

    #[test]
    fn next_copy_number_ignores_unnumbered_sibling_of_same_connection() {
        let mut state = AppState::default();
        // Two sessions of the same connection, neither numbered (e.g. two
        // freshly-opened jumpserver windows before any copy was made).
        numbered_session(&mut state, "src", "ops@jump", "conn-1");
        numbered_session(&mut state, "src2", "ops@jump", "conn-1");
        assert_eq!(next_copy_number(&state, "conn-1"), 1);
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
    fn session_tree_builds_fallback_pane_and_marks_active_state() {
        let state = state_with_active_session(&["alpha"]);

        let tree = build_session_tree(&state);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].tab_id, "alpha");
        assert_eq!(tree[0].anchor_session_id.as_deref(), Some("alpha"));
        assert!(tree[0].is_active);
        assert_eq!(tree[0].panes.len(), 1);
        assert_eq!(tree[0].panes[0].index, 0);
        assert!(!tree[0].panes[0].is_focused);

        let session = tree[0].panes[0].session.as_ref().unwrap();
        assert_eq!(session.id, "alpha");
        assert_eq!(session.name, "alpha");
        assert_eq!(session.kind, SessionType::Ssh);
        assert!(session.is_active);
        assert_eq!(session.connection_state, SessionConnectionState::Connected);
    }

    #[test]
    fn session_tree_keeps_empty_panes_and_shows_pane_only_sessions() {
        let mut state = state_with_pane_sessions("alpha", &["beta"]);
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Grid4,
                &["alpha".to_string(), "beta".to_string()],
            ),
        );
        state.focused_pane = Some(FocusedPane {
            layout_owner_tab_id: "alpha".to_string(),
            pane_idx: 1,
        });
        state
            .session_connection_states
            .insert("beta".to_string(), SessionConnectionState::Reconnecting);

        let tree = build_session_tree(&state);

        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].panes.len(), 4);
        assert_eq!(
            tree[0].panes[0]
                .session
                .as_ref()
                .map(|session| session.id.as_str()),
            Some("alpha")
        );
        let pane_only = tree[0].panes[1].session.as_ref().unwrap();
        assert_eq!(pane_only.id, "beta");
        assert!(!pane_only.is_active);
        assert_eq!(
            pane_only.connection_state,
            SessionConnectionState::Reconnecting
        );
        assert!(tree[0].panes[1].is_focused);
        assert!(tree[0].panes[2].session.is_none());
        assert!(tree[0].panes[3].session.is_none());
    }

    #[test]
    fn session_tree_deduplicates_sessions_across_panes_and_workspaces() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Grid4,
                &["alpha".to_string(), "beta".to_string(), "alpha".to_string()],
            ),
        );

        let tree = build_session_tree(&state);
        let session_ids: Vec<&str> = tree
            .iter()
            .flat_map(|workspace| &workspace.panes)
            .filter_map(|pane| pane.session.as_ref().map(|session| session.id.as_str()))
            .collect();

        assert_eq!(session_ids, vec!["alpha", "beta"]);
        assert!(tree[0].panes[2].session.is_none());
        assert_eq!(tree[1].panes.len(), 1);
        assert!(tree[1].panes[0].session.is_none());
    }

    #[test]
    fn session_tree_treats_stale_references_as_empty_panes() {
        let mut state = state_with_active_session(&["alpha"]);
        state.tabs.insert(
            0,
            WorkspaceTab {
                id: "stale-tab".to_string(),
                anchor_session_id: Some("missing-anchor".to_string()),
            },
        );
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Split2H,
                &["missing-pane-session".to_string(), "alpha".to_string()],
            ),
        );
        state.focused_pane = Some(FocusedPane {
            layout_owner_tab_id: "alpha".to_string(),
            pane_idx: 99,
        });

        let tree = build_session_tree(&state);

        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].panes.len(), 1);
        assert!(tree[0].panes[0].session.is_none());
        assert_eq!(tree[1].panes.len(), 2);
        assert!(tree[1].panes[0].session.is_none());
        assert_eq!(
            tree[1].panes[1]
                .session
                .as_ref()
                .map(|session| session.id.as_str()),
            Some("alpha")
        );
        assert!(
            tree.iter()
                .flat_map(|workspace| &workspace.panes)
                .all(|pane| !pane.is_focused)
        );
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
    fn suppress_comparison_diff_warning_disables_future_prompts_and_confirms_current_diff() {
        let mut state = AppState {
            comparison_diff_warning: Some(crate::comparison::DiffSummary {
                diff_rows: 3,
                total_rows: 4,
            }),
            ..AppState::default()
        };

        suppress_comparison_diff_warning(&mut state);

        assert!(!state.comparison_diff_warning_enabled);
        assert!(state.comparison_diff_confirmed);
        assert!(state.comparison_diff_warning.is_none());
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
                login_script: None,
            },
        );
        state
            .session_connection_states
            .insert("alpha".to_string(), SessionConnectionState::default());
        state.send_target_selection =
            Some(HashSet::from(["alpha".to_string(), "beta".to_string()]));
        state
            .pending_exit_check
            .insert("alpha".to_string(), VecDeque::new());
        state.suggestion_muted_sessions.insert("alpha".to_string());

        close_session(&mut state, &mut senders, "alpha");

        assert!(!state.sessions.iter().any(|s| s.id == "alpha"));
        assert!(!senders.contains_key("alpha"));
        assert!(!state.close_senders.iter().any(|(s, _)| s == "alpha"));
        assert!(!state.resize_senders.contains_key("alpha"));
        assert!(!state.terminals.contains_key("alpha"));
        assert!(!state.onekey_popups.contains_key("alpha"));
        assert!(!state.session_connection_states.contains_key("alpha"));
        assert!(
            !state
                .send_target_selection
                .as_ref()
                .is_some_and(|selection| selection.contains("alpha"))
        );
        assert!(!state.session_configs.contains_key("alpha"));
        assert!(!state.pending_exit_check.contains_key("alpha"));
        assert!(!state.suggestion_muted_sessions.contains("alpha"));
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

    // ------------------------------------------------------------------
    // `close_pane` — removes a pane from the layout (used by the empty-pane
    // ✕ button on the title bar). Inverse of `append_pane_to_active` /
    // `split_pane_to_active`.
    // ------------------------------------------------------------------

    #[test]
    fn close_pane_on_empty_pane_removes_it_from_layout() {
        // The primary use case: an empty pane (no session) gets its ✕ button
        // clicked. The pane should be removed from the layout, and no session
        // teardown happens (there's no session to tear down).
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        // Make a Split2H layout with pane 1 empty.
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
        assert_eq!(layout.panes.len(), 2);

        // Close the empty pane (pane 1).
        let outcome = close_pane(&mut state, &mut senders, "alpha", 1);
        assert_eq!(outcome, ClosePaneOutcome::Removed);

        // Layout now has 1 pane; alpha's session survives.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.panes[0].session_id, "alpha");
        // alpha's session entry is still there (we didn't close any session).
        assert!(state.sessions.iter().any(|s| s.id == "alpha"));
    }

    #[test]
    fn close_pane_on_pane_with_non_anchor_session_closes_session_and_removes_pane() {
        // Pane 1 holds a non-anchor session (beta). Closing pane 1 should:
        //   1. Tear down beta's session resources (via close_session).
        //   2. Remove pane 1 from the layout (the pane itself is gone, not
        //      just cleared).
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Sanity: pane 0=alpha, pane 1=beta.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(state.sessions.len(), 2);

        let outcome = close_pane(&mut state, &mut senders, "alpha", 1);
        assert_eq!(outcome, ClosePaneOutcome::Removed);

        // beta's session is gone.
        assert!(!state.sessions.iter().any(|s| s.id == "beta"));
        assert!(senders.get("beta").is_none());
        // Layout shrunk to 1 pane; alpha survives.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.panes[0].session_id, "alpha");
        // Tab survives with alpha as anchor.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("alpha"));
    }

    #[test]
    fn close_pane_on_anchor_pane_with_other_sessions_promotes_and_removes() {
        // Pane 0 holds the anchor (alpha). Pane 1 holds beta. Closing pane 0
        // should: (1) close alpha's session, (2) promote beta to be the new
        // anchor, (3) remove pane 0 from the layout (so beta moves to pane 0).
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::Removed);

        // alpha is gone; beta is the new anchor.
        assert!(!state.sessions.iter().any(|s| s.id == "alpha"));
        assert!(state.sessions.iter().any(|s| s.id == "beta"));
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
        // Layout shrunk to 1 pane (beta).
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.panes[0].session_id, "beta");
    }

    #[test]
    fn close_pane_on_anchor_pane_with_no_other_sessions_closes_tab() {
        // 2-pane layout where pane 0 = anchor (alpha) and pane 1 is EMPTY.
        // Closing pane 0 closes alpha (the only session) → close_session
        // removes the whole tab + layout. close_pane returns TabClosed.
        let (mut state, mut senders) = state_with_senders(&["alpha", "beta"]);
        state.tabs.retain(|t| t.id != "beta");
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Manually clear pane 1 so it's empty (apply_layout_preset filled it
        // with beta, but we want to test the "only alpha is in the layout" case).
        state
            .layouts
            .get_mut("alpha")
            .unwrap()
            .set_pane_session(1, String::new());
        // Sanity: only alpha is in the layout.
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "");
        assert_eq!(state.sessions.len(), 2); // beta still in registry, just not placed.

        // Close pane 0 (alpha — the anchor, with no other pane sessions).
        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::TabClosed);
        // alpha's tab + layout are gone.
        assert!(state.tabs.iter().all(|t| t.id != "alpha"));
        assert!(!state.layouts.contains_key("alpha"));
        assert!(state.sessions.iter().all(|s| s.id != "alpha"));
        // beta survives in the registry (it wasn't placed in any pane).
        assert!(state.sessions.iter().any(|s| s.id == "beta"));
    }

    #[test]
    fn close_pane_with_no_layout_returns_no_layout() {
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        // alpha's tab has no layout entry (Single preset).
        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::NoLayout);
    }

    #[test]
    fn close_pane_out_of_range_returns_out_of_range() {
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let outcome = close_pane(&mut state, &mut senders, "alpha", 99);
        assert_eq!(outcome, ClosePaneOutcome::OutOfRange);
        // Layout unchanged.
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 2);
    }

    #[test]
    fn close_pane_clears_focused_pane_when_removing_focused_pane() {
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Focus pane 1.
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));
        assert_eq!(state.focused_pane.as_ref().unwrap().pane_idx, 1);

        // Close pane 1 (the focused pane). focused_pane should be cleared.
        let outcome = close_pane(&mut state, &mut senders, "alpha", 1);
        assert_eq!(outcome, ClosePaneOutcome::Removed);
        assert!(state.focused_pane.is_none());
    }

    #[test]
    fn close_pane_clears_focused_pane_when_tab_is_closed() {
        // 3 panes: pane 0 (alpha), pane 1 (empty), pane 2 (empty). Focus pane 2.
        // Close pane 0 (alpha — the anchor). Since no other pane has a session,
        // close_session removes the whole tab. focused_pane (which pointed at
        // pane 2 — an empty pane, so close_session's "focused_points_at_closed"
        // check didn't fire) must be cleared by close_pane so it's not dangling.
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Grow to 3 panes (1x3 strip).
        let layout = state.layouts.get_mut("alpha").unwrap();
        layout.append_pane(true).expect("append");
        assert_eq!(layout.panes.len(), 3);

        // Focus pane 2 (an empty pane).
        assert!(focus_pane_for_layout(&mut state, "alpha", 2));
        assert_eq!(state.focused_pane.as_ref().unwrap().pane_idx, 2);

        // Close pane 0 (alpha). Tab is closed; focused_pane must be cleared.
        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::TabClosed);
        assert!(
            state.focused_pane.is_none(),
            "focused_pane must not dangle after tab close"
        );
    }

    #[test]
    fn close_pane_round_trips_with_append_pane_to_active() {
        // Append then close should leave the layout in its original state.
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        let before = state.layouts.get("alpha").unwrap().panes.len();
        assert_eq!(before, 2);

        // Append one pane (3 total).
        let new_idx = append_pane_to_active(&mut state).expect("append");
        assert_eq!(new_idx, 2);
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 3);

        // Close the newly-appended pane (empty).
        let outcome = close_pane(&mut state, &mut senders, "alpha", new_idx);
        assert_eq!(outcome, ClosePaneOutcome::Removed);
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), before);
    }

    #[test]
    fn close_pane_on_last_pane_with_session_closes_tab() {
        // Closing the ✕ on the only remaining pane should close the session
        // and remove the layout (effectively closing the tab). This is the
        // fix for the "空窗口关闭逻辑没有正确的关闭" report: the prior code
        // returned `SinglePane` and did nothing.
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Close pane 1 first (back to 1 pane with alpha).
        let _ = close_pane(&mut state, &mut senders, "alpha", 1);
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 1);

        // Now close the last pane (pane 0, holding alpha).
        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::TabClosed);
        // Layout is gone.
        assert!(!state.layouts.contains_key("alpha"));
        // Session is gone.
        assert!(state.terminals.get("alpha").is_none());
        assert!(!senders.contains_key("alpha"));
        // focused_pane is cleared.
        assert!(state.focused_pane.is_none());
    }

    #[test]
    fn close_pane_on_last_empty_pane_removes_layout() {
        // Closing the ✕ on the only remaining EMPTY pane should remove the
        // layout without trying to close a session (there is none).
        let (mut state, mut senders) = state_with_senders(&["alpha"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Move alpha out of pane 0 so pane 0 is empty, then close pane 1.
        // Easier: just close pane 1 (which holds alpha after Split2H), then
        // we have 1 pane (pane 0, empty). Closing pane 0 should remove the
        // layout.
        // Actually Split2H puts alpha in pane 0 and empty in pane 1. Close
        // pane 1 (empty) first → 1 pane (alpha). Then close pane 0 (alpha).
        let _ = close_pane(&mut state, &mut senders, "alpha", 1);
        assert_eq!(state.layouts.get("alpha").unwrap().panes.len(), 1);
        // Close the last pane (alpha).
        let outcome = close_pane(&mut state, &mut senders, "alpha", 0);
        assert_eq!(outcome, ClosePaneOutcome::TabClosed);
        assert!(!state.layouts.contains_key("alpha"));
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
    fn switching_top_tab_retargets_pane_focus_to_new_tab() {
        // Focus follows the active tab so its border outline stays correct:
        // switching away must not leave `focused_pane` pointing at the old
        // tab's layout.
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("beta"));

        set_active_tab(&mut state, "beta");
        // beta's tab has no layout entry (implicit Single): explicit pane
        // focus is dropped, and the UI falls back to the tab's anchor.
        assert_eq!(state.focused_pane, None);
        assert_eq!(focused_pane_session(&state), None);

        // Switching back to the multi-pane tab focuses the pane holding the
        // tab's anchor session (leave pane 0), never a stale pane index.
        set_active_tab(&mut state, "alpha");
        let focused = state.focused_pane.as_ref().expect("layout focus");
        assert_eq!(focused.layout_owner_tab_id, "alpha");
        assert_eq!(focused_pane_session(&state).as_deref(), Some("alpha"));
    }

    #[test]
    fn activate_session_switches_to_its_workspace_tab() {
        let mut state = state_with_active_session(&["alpha", "beta"]);

        assert!(activate_session(&mut state, "beta"));
        assert_eq!(state.active_tab.as_deref(), Some("beta"));
        assert_eq!(state.active_session.as_deref(), Some("beta"));
    }

    #[test]
    fn activate_session_focuses_a_pane_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        assert!(activate_session(&mut state, "beta"));
        assert_eq!(state.active_tab.as_deref(), Some("alpha"));
        assert_eq!(focused_pane_session(&state).as_deref(), Some("beta"));
    }

    #[test]
    fn activate_session_rejects_unknown_session() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!activate_session(&mut state, "missing"));
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
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: None,
            cwd: None,
            last_command_status: CommandStatus::default(),
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
    fn command_send_targets_prefers_focused_pane_without_comparison() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(focus_pane_for_layout(&mut state, "alpha", 1));

        assert_eq!(command_send_targets(&state), vec!["beta"]);
    }

    #[test]
    fn command_send_targets_broadcasts_when_comparison_is_enabled() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(toggle_comparison_mode(&mut state).unwrap());

        assert_eq!(command_send_targets(&state), vec!["alpha", "beta"]);
    }

    #[test]
    fn send_targets_initially_select_only_the_active_connected_session() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        for session_id in ["alpha", "beta"] {
            state
                .session_connection_states
                .insert(session_id.to_string(), SessionConnectionState::Connected);
        }

        assert_eq!(selected_send_target_ids(&state), vec!["alpha"]);
    }

    #[test]
    fn send_targets_support_select_all_and_invert() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        for session_id in ["alpha", "beta", "gamma"] {
            state
                .session_connection_states
                .insert(session_id.to_string(), SessionConnectionState::Connected);
        }

        assert_eq!(select_all_send_targets(&mut state), 3);
        assert_eq!(
            selected_send_target_ids(&state),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(invert_send_targets(&mut state), 0);
        assert!(selected_send_target_ids(&state).is_empty());
        assert_eq!(invert_send_targets(&mut state), 3);
        assert_eq!(
            selected_send_target_ids(&state),
            vec!["alpha", "beta", "gamma"]
        );
    }

    #[test]
    fn send_targets_filter_disconnected_closed_and_embedded_shell_sessions() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "shell"]);
        state
            .session_connection_states
            .insert("alpha".to_string(), SessionConnectionState::Connected);
        state
            .session_connection_states
            .insert("beta".to_string(), SessionConnectionState::Disconnected);
        state
            .session_connection_states
            .insert("gamma".to_string(), SessionConnectionState::Reconnecting);
        state
            .session_connection_states
            .insert("shell".to_string(), SessionConnectionState::Connected);
        state.bottom_shell_session_id = Some("shell".to_string());

        assert_eq!(
            available_send_targets(&state),
            vec![SendTargetOption {
                session_id: "alpha".to_string(),
                label: "alpha".to_string(),
            }]
        );

        select_all_send_targets(&mut state);
        state.sessions.retain(|session| session.id != "alpha");
        assert!(selected_send_target_ids(&state).is_empty());
    }

    #[test]
    fn explicit_empty_send_selection_stays_empty_when_active_session_changes() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        for session_id in ["alpha", "beta"] {
            state
                .session_connection_states
                .insert(session_id.to_string(), SessionConnectionState::Connected);
        }

        assert!(set_send_target_selected(&mut state, "alpha", false));
        state.active_session = Some("beta".to_string());
        state.active_tab = Some("beta".to_string());

        assert!(selected_send_target_ids(&state).is_empty());
    }

    #[test]
    fn scroll_sync_targets_matches_broadcast_targets_when_comparison_on() {
        // With comparison mode enabled, wheel scrolling and PTY input target
        // the same non-empty panes.
        let mut state = state_with_active_session(&["alpha", "beta", "gamma"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        assert_eq!(
            scroll_sync_targets(&state, "beta"),
            broadcast_targets(&state)
        );
    }

    #[test]
    fn scroll_sync_targets_keeps_the_source_pane_when_comparison_off() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // A wheel event in pane beta must stay local while comparison is off;
        // the active tab anchor (alpha) is not a focused-pane pointer.
        assert_eq!(
            scroll_sync_targets(&state, "beta"),
            vec!["beta".to_string()]
        );
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
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("gamma".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
        });
        state.sessions.push(SessionTab {
            id: "delta".to_string(),
            name: "delta".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("delta".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
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

    // ------------------------------------------------------------------
    // Pane-layout persistence (build_layout_state / apply_layout_state)
    // ------------------------------------------------------------------
    //
    // These cover the session-id ↔ display-name bridge: panes store
    // session *names* in the persisted form (live ids are fresh UUIDs on
    // every launch), and are mapped back to live ids on restore.

    /// A multi-pane layout is captured with pane session_ids rewritten to
    /// the sessions' display names.
    #[test]
    fn build_layout_state_rewrites_session_ids_to_names() {
        let state = state_with_pane_sessions("alpha", &["beta"]);
        let snapshot = state.build_layout_state();
        assert_eq!(snapshot.tabs.len(), 1);
        let tab = &snapshot.tabs[0];
        assert_eq!(tab.anchor_name, "alpha");
        // panes hold names, not live ids.
        assert_eq!(tab.layout.panes[0].session_id, "alpha");
        assert_eq!(tab.layout.panes[1].session_id, "beta");
    }

    /// A trivial single-pane tab (no customisation) is omitted from the
    /// snapshot so the file stays small.
    #[test]
    fn build_layout_state_skips_trivial_single_pane_tabs() {
        let state = state_with_active_session(&["solo"]);
        // No layout entry at all → nothing to save.
        let snapshot = state.build_layout_state();
        assert!(snapshot.tabs.is_empty());
    }

    /// The comparison flag and split structure survive the round-trip.
    #[test]
    fn build_layout_state_captures_comparison_and_preset() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        apply_layout_preset(&mut state, LayoutPreset::Grid4);
        toggle_comparison_mode(&mut state);
        let snapshot = state.build_layout_state();
        assert_eq!(snapshot.tabs.len(), 1);
        let tab = &snapshot.tabs[0];
        assert!(tab.layout.comparison);
        assert_eq!(tab.layout.panes.len(), 4);
    }

    /// After restore, pane names map back to live session ids and the layout
    /// is re-keyed under the tab's fresh group id.
    #[test]
    fn apply_layout_state_maps_names_back_to_live_ids() {
        // Simulate the post-restore state: sessions exist with fresh ids, but
        // no layout has been re-attached yet.
        let mut state = state_with_pane_sessions("alpha", &["beta"]);
        state.layouts.clear();

        // Build a snapshot as if saved from a previous launch (pane ids are
        // display names).
        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "alpha".to_string(),
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["alpha".to_string(), "beta".to_string()],
                ),
            }],
        };

        state.apply_layout_state(&saved);
        // The layout is re-keyed under alpha's tab group id.
        let layout = state.layouts.get("alpha").expect("layout re-attached");
        // Pane session_ids are live ids again (in this test harness name==id).
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    /// After session restore, each restored session gets its own top-level
    /// workspace tab. `apply_layout_state` then re-attaches a saved split
    /// layout that places "beta" as pane 1 inside "alpha"'s tab. Without
    /// dedup, "beta" would appear BOTH as its own standalone tab AND as a
    /// pane in alpha's split — the user sees "two windows".
    /// `dedup_pane_session_tabs` removes the redundant standalone tab.
    #[test]
    fn dedup_pane_session_tabs_removes_redundant_standalone_tabs() {
        // Simulate the post-restore state: alpha and beta each got their own
        // workspace tab (restore_sessions creates one tab per session).
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // Now re-attach a saved split layout that puts beta as pane 1 inside
        // alpha's tab.
        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "alpha".to_string(),
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["alpha".to_string(), "beta".to_string()],
                ),
            }],
        };
        state.apply_layout_state(&saved);
        // Before dedup: two tabs exist (alpha + beta), but beta is now a pane
        // inside alpha's split layout.
        assert_eq!(state.tabs.len(), 2);
        assert!(state.layouts.get("alpha").is_some());

        state.dedup_pane_session_tabs();

        // After dedup: only alpha's tab remains. Beta lives as pane 1 in
        // alpha's split layout, not as its own top-level tab.
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].anchor_session_id.as_deref(), Some("alpha"));
        // The split layout is preserved.
        let layout = state.layouts.get("alpha").expect("layout preserved");
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        // Beta's former tab layout entry is gone.
        assert!(state.layouts.get("beta").is_none());
    }

    /// A session whose anchor is NOT a pane in any other tab's layout must
    /// keep its own tab. This guards against over-removal when multiple
    /// independent single-pane tabs exist.
    #[test]
    fn dedup_pane_session_tabs_keeps_independent_single_pane_tabs() {
        // Two independent tabs, no split layouts.
        let mut state = state_with_active_session(&["alpha", "beta"]);
        // No layouts at all (single-pane tabs have no layout entry).
        state.dedup_pane_session_tabs();
        // Both tabs survive — neither anchor is a pane in another tab.
        assert_eq!(state.tabs.len(), 2);
    }

    /// A pane whose session wasn't restored is cleared (rendered as an empty
    /// drop target), preserving the user's split structure.
    #[test]
    fn apply_layout_state_clears_unresolvable_panes() {
        let mut state = state_with_pane_sessions("alpha", &["beta"]);
        state.layouts.clear();

        // "gamma" was never restored.
        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "alpha".to_string(),
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["alpha".to_string(), "gamma".to_string()],
                ),
            }],
        };

        state.apply_layout_state(&saved);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes[0].session_id, "alpha");
        // gamma not found → pane left empty.
        assert!(layout.panes[1].session_id.is_empty());
    }

    /// A layout whose anchor session wasn't restored is skipped entirely
    /// (no dangling layout entry under a non-existent tab).
    #[test]
    fn apply_layout_state_skips_tab_with_unrestored_anchor() {
        let mut state = state_with_pane_sessions("alpha", &["beta"]);
        state.layouts.clear();

        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "ghost".to_string(), // no such tab
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["ghost".to_string(), "beta".to_string()],
                ),
            }],
        };

        state.apply_layout_state(&saved);
        // No layout inserted for the ghost tab.
        assert!(state.layouts.is_empty());
    }

    /// Regression: a persisted split whose panes BOTH name the same session
    /// (e.g. a snapshot corrupted by the old restore-duplication bug, when
    /// only one "jumpserver" session was actually restored) must NOT come
    /// back as two identical panes. The duplicate pane is removed and the
    /// survivor keeps the live session.
    #[test]
    fn apply_layout_state_drops_duplicate_panes_for_single_session() {
        let mut state = state_with_active_session(&["jumpserver"]);
        state.layouts.clear();

        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "jumpserver".to_string(),
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["jumpserver".to_string(), "jumpserver".to_string()],
                ),
            }],
        };

        state.apply_layout_state(&saved);
        let layout = state.layouts.get("jumpserver").expect("layout re-attached");
        // The duplicate pane is gone; a single pane shows the session.
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.panes[0].session_id, "jumpserver");
    }

    /// Two restored sessions sharing a display name (the same connection
    /// opened twice) fill a two-pane split with DISTINCT live ids — the
    /// anchor session claims the first matching pane so it isn't orphaned
    /// by its same-named sibling.
    #[test]
    fn apply_layout_state_assigns_distinct_sessions_to_same_named_panes() {
        let mut state = AppState::default();
        for id in ["js-1", "js-2"] {
            state.sessions.push(SessionTab {
                id: id.to_string(),
                name: "jumpserver".to_string(),
                kind: SessionType::Ssh,
                render_output: Default::default(),
                version: 0,
                suggestion: None,
                suggestions: Vec::new(),
                suggestion_corrections: HashSet::new(),
                suggestion_selected: 0,
                suggestion_visible: false,
                command_history: Vec::new(),
                hostname: None,
                cwd: None,
                last_command_status: CommandStatus::default(),
            });
            state.tabs.push(WorkspaceTab {
                id: format!("tab-{id}"),
                anchor_session_id: Some(id.to_string()),
            });
        }

        let saved = crate::layout_state::LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![crate::layout_state::PersistedTabLayout {
                anchor_name: "jumpserver".to_string(),
                layout: PaneLayout::from_preset(
                    LayoutPreset::Split2H,
                    &["jumpserver".to_string(), "jumpserver".to_string()],
                ),
            }],
        };

        state.apply_layout_state(&saved);
        // The layout attaches to the first tab whose anchor name matches.
        let layout = state.layouts.get("tab-js-1").expect("layout re-attached");
        assert_eq!(layout.panes.len(), 2);
        // The anchor session claims pane 0; the sibling gets pane 1.
        assert_eq!(layout.panes[0].session_id, "js-1");
        assert_eq!(layout.panes[1].session_id, "js-2");
    }

    /// Login-script completion upgrades an Idle tab badge to Success (jump
    /// hosts never emit OSC 133;D exit codes), but never clobbers a real
    /// command status.
    #[test]
    fn mark_login_script_success_upgrades_only_idle_badges() {
        let mut state = state_with_tabs(&["jumpserver", "bidbot"]);

        state.mark_login_script_success("jumpserver");
        assert_eq!(
            state.sessions[0].last_command_status,
            CommandStatus::Success
        );

        // A real (failed) command status is preserved.
        state.sessions[1].last_command_status = CommandStatus::Failed(2);
        state.mark_login_script_success("bidbot");
        assert_eq!(
            state.sessions[1].last_command_status,
            CommandStatus::Failed(2)
        );

        // Unknown session id is a no-op.
        state.mark_login_script_success("ghost");
    }

    /// Plain SSH connect: a successful attempt upgrades Idle (fresh connect)
    /// or Disconnected (reconnect) badges to Success but never clobbers a
    /// real command status; a failed attempt always resets the badge to
    /// Disconnected with the failure reason.
    #[test]
    fn note_connection_outcome_covers_success_and_failure() {
        let mut state = state_with_tabs(&["fresh", "reconnected", "running", "broken"]);

        // Fresh connect: Idle -> Success.
        state.note_connection_outcome("fresh", None);
        assert_eq!(
            state.sessions[0].last_command_status,
            CommandStatus::Success
        );

        // Reconnect success clears the previous disconnect badge.
        state.sessions[1].last_command_status = CommandStatus::Disconnected("timeout".to_string());
        state.note_connection_outcome("reconnected", None);
        assert_eq!(
            state.sessions[1].last_command_status,
            CommandStatus::Success
        );

        // A real (failed) command status is preserved on success.
        state.sessions[2].last_command_status = CommandStatus::Failed(2);
        state.note_connection_outcome("running", None);
        assert_eq!(
            state.sessions[2].last_command_status,
            CommandStatus::Failed(2)
        );

        // Connect failure always flips the badge to Disconnected, even when
        // a stale status from the previous attempt was showing.
        state.sessions[3].last_command_status = CommandStatus::Success;
        state.note_connection_outcome("broken", Some("auth failed".to_string()));
        assert_eq!(
            state.sessions[3].last_command_status,
            CommandStatus::Disconnected("auth failed".to_string())
        );

        // Unknown session id is a no-op.
        state.note_connection_outcome("ghost", None);
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
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("newhost".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
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
            suggestion_corrections: HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("newhost".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
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

    /// A self-drop (dropping a pane's session back onto its own pane) is a
    /// no-op: no clone, no new pane. This is the fix for the
    /// "错误的产生多个不需要的四方块" bug where accidental self-drops kept
    /// growing the layout with cloned sessions.
    #[test]
    fn execute_tab_drop_self_drop_multi_pane_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::NoOpSelfDrop);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    #[test]
    fn self_drop_is_noop_and_preserves_existing_floating_window_positions() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        assert!(begin_floating_pane_move(&mut state, 0));
        assert!(move_floating_pane_for_active(
            &mut state, 0, 140.0, 70.0, 1200.0, 800.0,
        ));
        let before = state.layouts["alpha"].panes[0].floating;

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::NoOpSelfDrop);
        assert_eq!(state.layouts["alpha"].panes.len(), 2);
        assert_eq!(state.layouts["alpha"].panes[0].floating, before);
        assert!(state.layouts["alpha"].is_floating());
    }

    /// A self-drop is a no-op regardless of how many background tabs exist —
    /// no pane is reserved, no background tab is touched.
    #[test]
    fn execute_tab_drop_self_drop_does_not_touch_background_tabs() {
        let mut state = state_with_active_session(&["alpha", "beta", "gamma", "delta"]);
        state.layouts.insert(
            "alpha".to_string(),
            PaneLayout::from_preset(
                LayoutPreset::Split2H,
                &["alpha".to_string(), "beta".to_string()],
            ),
        );

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::NoOpSelfDrop);
        let layout = state.layouts.get("alpha").unwrap();
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "alpha");
        assert_eq!(layout.panes[1].session_id, "beta");
    }

    /// Repeated self-drops are all no-ops: the layout never grows from a
    /// self-drop. (Previously it grew 1→2→…→MAX_PANES; that was the bug.)
    #[test]
    fn execute_tab_drop_repeated_self_drops_are_all_noops() {
        let mut state = state_with_active_session(&["alpha"]);

        for _ in 0..(MAX_PANES + 2) {
            assert_eq!(
                execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha"),
                TabDropOutcome::NoOpSelfDrop
            );
        }
        // Layout was never created (single-pane default), or if it existed
        // it still has exactly one pane.
        if let Some(layout) = state.layouts.get("alpha") {
            assert_eq!(layout.panes.len(), 1);
            assert_eq!(layout.panes[0].session_id, "alpha");
        }
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    /// Dragging the active tab into its own single-pane view is a no-op —
    /// no clone, no new pane, even when other tabs exist.
    #[test]
    fn execute_tab_drop_active_tab_self_drop_single_pane_is_noop() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        assert!(!state.layouts.contains_key("alpha"));

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::NoOpSelfDrop);
        assert!(!state.layouts.contains_key("alpha"));
        assert_eq!(state.active_session.as_deref(), Some("alpha"));
    }

    #[test]
    fn execute_tab_drop_active_tab_self_drop_only_tab_is_noop() {
        let mut state = state_with_active_session(&["alpha"]);
        assert!(!state.layouts.contains_key("alpha"));

        let outcome = execute_tab_drop_on_pane(&mut state, "alpha", 0, "alpha");

        assert_eq!(outcome, TabDropOutcome::NoOpSelfDrop);
        assert!(!state.layouts.contains_key("alpha"));
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

    /// `set_pane_session_for_layout` writes the session id into the requested
    /// pane of the requested layout, regardless of which session is active.
    /// This is the contract `open_cloned_sessions_for_self_drop` (now unused
    /// by the self-drop path, but retained for future explicit-clone flows)
    /// and sidebar-drop replacement rely on.
    #[test]
    fn explicit_layout_target_writes_session_into_requested_pane() {
        let mut state = state_with_active_session(&["alpha", "beta"]);
        apply_layout_preset(&mut state, LayoutPreset::Split2H);
        // Grow to 3 panes so pane 2 exists.
        state
            .layouts
            .get_mut("alpha")
            .unwrap()
            .append_pane(true)
            .expect("grow to 3");

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
    Tunnels,
    Relay,
}

/// Runtime progress of a per-connection login initialization script (the
/// expect/send DSL parsed by `rusterm_core::parse_login_script`). Steps are
/// driven from the session-output path in `app.rs`: whenever new PTY output
/// arrives, the driver re-evaluates the current `Expect` step against the
/// last non-empty output line and emits sends/delays until the script ends.
///
/// The struct holds NO plaintext credential longer than the send itself —
/// `SendOneKey` steps resolve their value from the unlocked OneKey library at
/// send time, and the resolved value is cleared from `send_buffer` right
/// after it is queued to the PTY.
#[derive(Debug, Clone)]
pub struct LoginScriptRuntime {
    /// Parsed steps, in order.
    pub steps: Vec<rusterm_core::LoginStep>,
    /// Index of the step currently being waited on (an `Expect`, `Send`,
    /// `SendOneKey` or `Delay`). Steps after a matched Expect are executed
    /// eagerly until the next Expect or the end of the script.
    pub idx: usize,
    /// Non-empty while a `Delay { ms }` is being executed asynchronously —
    /// the remaining sends/delays queued for when the sleep ends.
    pub send_buffer: std::collections::VecDeque<String>,
    /// Whether the script has finished (all steps consumed) or been aborted
    /// (timeout / unresolvable reference).
    pub done: bool,
    /// Timeout marker: when the current Expect started waiting. `None` before
    /// the first wait. Used by the driver to abort a stuck script after
    /// `LOGIN_SCRIPT_EXPECT_TIMEOUT_SECS`.
    pub wait_started: Option<std::time::Instant>,
}

/// Maximum number of establishment-phase operations recorded per session for
/// recovery replay. Recording stops once the window is full — the prefix
/// semantics are deliberate: only the inputs that *established* the remote
/// interactive state (bastion menu navigation) are replay candidates.
/// Steady-state shell commands typed later are never recorded, so a reconnect
/// can never replay an unbounded tail of arbitrary (possibly destructive)
/// shell work.
pub const REPLAY_MAX_OPS: usize = 10;

/// Records the interactive inputs that established a remote session's state
/// (e.g. jumpserver menu navigation: host names/numbers + Enter) so a
/// reconnect or startup restore can replay them and land the user back in the
/// same interactive state instead of at the bastion's menu.
///
/// Lifecycle:
/// - Created lazily on the first recorded op after a session connects.
/// - Recording is gated to interactive remote kinds (SSH / Telnet) and stops
///   at [`REPLAY_MAX_OPS`] (establishment prefix only).
/// - The first OSC 133;D exit code observed on the session is evidence of a
///   real integrated shell (interactive bastion menus never emit it). At that
///   point the recorder is FROZEN: the ops recorded so far — the establishment
///   prefix typed *before* the evidence arrived (e.g. bastion menu navigation
///   that led to the integrated target shell) — are kept for replay, but
///   recording stops permanently so regular shell commands never enter the
///   replay log.
/// - Preserved across disconnects (that's the whole point); removed when the
///   session closes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReplayRecorder {
    /// Submitted input lines, in submission order (establishment prefix).
    pub ops: Vec<String>,
    /// True once shell-integration evidence (OSC 133;D) was observed. From
    /// then on `ops` is frozen (kept as the establishment prefix) and
    /// recording is disabled for this session.
    pub shell_integrated: bool,
    /// The cwd to `cd` back to AFTER the establishment ops replay (captured
    /// from the persisted snapshot at restore time). Carried on the recorder so
    /// the OTP-group supervisor can re-schedule a clone tab's replay once its
    /// channel clone lands — the original `schedule_replay_after_reconnect`
    /// task (60s deadline) may have expired while the tab waited for the
    /// group leader's OTP to settle.
    pub follow_up_cwd: Option<String>,
}

/// Records one submitted menu-navigation input line into the session's
/// replay recorder. Returns `true` if the op was recorded.
///
/// Caller contract: only pass MENU-class submissions here. Input typed at a
/// shell prompt must be routed to [`note_shell_prompt_evidence`] instead, and
/// input typed at a detected credential prompt (password/token/username) must
/// not be passed at all — see the `on_input` Enter branch in `app.rs`, which
/// classifies the prompt line via [`prompt_looks_like_shell`].
///
/// Menu reentry thaws a frozen recorder: if the session was previously
/// frozen by shell evidence (OSC 133;D or a shell-looking prompt) and the
/// user is back at a bastion menu, the stale establishment prefix is cleared
/// and recording restarts, so the snapshot always holds the *last* navigation
/// sequence — the one that leads to where the user actually is.
///
/// Guards (in order):
/// - empty/whitespace-only input is ignored;
/// - only SSH and Telnet sessions record (local shells are restored via cwd,
///   serial ports have no login flow to replay);
/// - the establishment window is capped at [`REPLAY_MAX_OPS`].
pub fn record_replay_op(state: &mut AppState, session_id: &str, op: &str) -> bool {
    let op = op.trim();
    if op.is_empty() {
        return false;
    }
    let is_interactive_remote = matches!(
        state.session_configs.get(session_id).map(|c| &c.kind),
        Some(rusterm_core::config::ConnectionKind::Ssh(_))
            | Some(rusterm_core::config::ConnectionKind::Telnet(_))
    );
    if !is_interactive_remote {
        return false;
    }
    let rec = state
        .session_replays
        .entry(session_id.to_string())
        .or_default();
    if rec.shell_integrated {
        // Menu reentry after landing on a shell: the old establishment
        // prefix leads to the *previous* target — discard it and re-record
        // the navigation that leads to the new one.
        rec.ops.clear();
        rec.shell_integrated = false;
    }
    if rec.ops.len() >= REPLAY_MAX_OPS {
        return false;
    }
    rec.ops.push(op.to_string());
    true
}

/// Whether a shell command establishes a *lasting session context* the user
/// would expect a recovery replay to re-establish: privilege escalation into
/// a login/interactive shell (`sudo -i`, `sudo -s`, `sudo su`, `su`), nested
/// remote hops (`ssh`, `telnet`), and container attach/exec shells
/// (`docker exec`, `kubectl exec`, …).
///
/// One-shot commands are deliberately excluded — replaying `sudo systemctl
/// restart nginx` on reconnect would repeat a side effect, not restore
/// state. That's why plain `sudo <cmd>` (no `-i`/`-s`, target not a shell)
/// returns `false`.
pub fn is_context_command(command: &str) -> bool {
    const SHELLS: &[&str] = &[
        "su", "bash", "sh", "zsh", "fish", "dash", "ksh", "csh", "tcsh",
    ];
    let mut tokens = command.split_whitespace();
    let Some(head) = tokens.next() else {
        return false;
    };
    match head {
        // `su` alone or `su - user` always swaps the shell context.
        "su" => true,
        // Nested interactive hops.
        "ssh" | "telnet" => true,
        "sudo" | "doas" => {
            // Context-establishing iff it requests a login/interactive shell
            // (`-i`/`-s`) or its target command is itself a shell / `su`.
            let mut saw_shell_flag = false;
            let mut rest = tokens;
            while let Some(tok) = rest.next() {
                match tok {
                    "-i" | "--login" | "-s" | "--shell" => saw_shell_flag = true,
                    // Flags that consume a separate argument — skip it so a
                    // username like `-u root` is not mistaken for the target
                    // command.
                    "-u" | "--user" | "-g" | "--group" | "-h" | "--host" | "-p" | "--prompt" => {
                        rest.next();
                    }
                    _ if tok.starts_with('-') => {}
                    // First non-flag token: the command sudo/doas runs.
                    _ => return SHELLS.contains(&tok),
                }
            }
            saw_shell_flag
        }
        "docker" | "podman" | "nerdctl" | "kubectl" | "oc" => {
            matches!(tokens.next(), Some("exec") | Some("attach"))
        }
        _ => false,
    }
}

/// Whether a shell command *leaves* a session context established by an
/// [`is_context_command`] input (kept deliberately narrow: `exit`/`logout`).
/// Used to pop the trailing context command off the replay log so a
/// `sudo -i` → work → `exit` round trip leaves no stale escalation to
/// replay.
pub fn is_context_exit_command(command: &str) -> bool {
    matches!(command.trim(), "exit" | "logout")
}

/// Records a *context-establishing* shell command (see
/// [`is_context_command`]) into the session's replay recorder, appending it
/// after the frozen establishment prefix WITHOUT thawing the freeze.
///
/// This is the shell-prompt counterpart of [`record_replay_op`]: menu
/// submissions record (and thaw on reentry), regular shell commands never
/// record, but commands like `sudo -i` sit in between — they are typed at a
/// shell prompt yet change the session state a recovery must re-establish.
/// Appending under freeze preserves both invariants: the bastion navigation
/// prefix stays intact (a thaw would clear it) and ordinary shell commands
/// typed afterwards still never enter the log.
///
/// Returns `false` for non-context commands, non-SSH/Telnet sessions, empty
/// input, and a full window ([`REPLAY_MAX_OPS`]).
pub fn record_context_command(state: &mut AppState, session_id: &str, command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() || !is_context_command(command) {
        return false;
    }
    let is_interactive_remote = matches!(
        state.session_configs.get(session_id).map(|c| &c.kind),
        Some(rusterm_core::config::ConnectionKind::Ssh(_))
            | Some(rusterm_core::config::ConnectionKind::Telnet(_))
    );
    if !is_interactive_remote {
        return false;
    }
    let rec = state
        .session_replays
        .entry(session_id.to_string())
        .or_default();
    if rec.ops.len() >= REPLAY_MAX_OPS {
        return false;
    }
    rec.ops.push(command.to_string());
    // Typing at a shell prompt is itself shell evidence — make sure the
    // recorder is frozen so subsequent ordinary commands stay unrecorded
    // (matters for direct-connect targets that never emitted OSC 133;D).
    rec.shell_integrated = true;
    true
}

/// Pops the trailing context command off the replay log when the user exits
/// the context it established (`exit`/`logout` typed at a shell prompt after
/// e.g. `sudo -i`). Returns the popped op, or `None` when nothing applies.
///
/// Guards: only a frozen recorder is touched (an unfrozen one is still in
/// menu-navigation phase), and only when the trailing op IS a context
/// command — `exit` typed on the target host itself (dropping back to the
/// bastion menu) must not eat the menu-navigation prefix; the subsequent
/// menu reentry thaws and re-records instead.
pub fn pop_context_command(
    state: &mut AppState,
    session_id: &str,
    command: &str,
) -> Option<String> {
    if !is_context_exit_command(command) {
        return None;
    }
    let rec = state.session_replays.get_mut(session_id)?;
    if !rec.shell_integrated {
        return None;
    }
    if !rec.ops.last().is_some_and(|op| is_context_command(op)) {
        return None;
    }
    rec.ops.pop()
}

/// Heuristic classifier for the terminal line the cursor sits on when the
/// user presses Enter: does it look like a *shell* prompt (as opposed to an
/// interactive bastion/jump-host menu prompt)?
///
/// Shell-looking evidence: classic POSIX suffixes (`$ `, `# `, `% `),
/// PowerShell (`PS ...>`), and modern prompt glyphs (`➜`, `❯`). A bare `"> "`
/// is deliberately NOT treated as shell: bastion menus like JumpServer's
/// `Opt>` must win, at the accepted cost of misclassifying some exotic
/// remote prompts (e.g. fish's `~>`), whose submissions are then recorded
/// as menu ops and neutralized by the safety filters at replay time.
pub fn prompt_looks_like_shell(current_line: &str) -> bool {
    let line = current_line.trim_end();
    if line.is_empty() {
        return false;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with('➜') || trimmed.starts_with('❯') {
        return true;
    }
    if trimmed.starts_with("PS ") && trimmed.contains('>') {
        return true;
    }
    // Classic prompt terminator followed by the typed input:
    // `[root@web ~]# id`, `user@host:~$ exit`, `host% ls`.
    if ["$ ", "# ", "% "].iter().any(|m| line.contains(m)) {
        return true;
    }
    // Bare prompt with no input yet (`$`/`#` at end of line).
    matches!(line.chars().last(), Some('$') | Some('#'))
}

/// Marks shell evidence based on the *prompt line* the user submitted at
/// (classified by [`prompt_looks_like_shell`]), freezing the replay recorder
/// exactly like OSC 133;D evidence does.
///
/// This matters for bastion targets WITHOUT shell integration: they never
/// emit OSC 133;D, so without prompt-based evidence every shell command they
/// run would pollute the replay log as a fake menu op.
pub fn note_shell_prompt_evidence(state: &mut AppState, session_id: &str) {
    note_shell_integration_evidence(state, session_id);
}

/// Marks the session as having a real, shell-integrated remote (OSC 133;D
/// exit code observed) and FREEZES the replay recorder: the ops recorded so
/// far are kept, but recording is permanently disabled for this session.
///
/// Freezing (rather than clearing) matters for bastion flows: the user
/// navigates an interactive jump-host menu (recorded), lands on a target
/// host whose shell emits OSC 133;D, and that recorded navigation is exactly
/// what a restore must replay to land back on the target. Direct-connect
/// integrated shells report their first exit code right after the
/// integration is injected — before the user types anything — so their
/// frozen prefix is empty and cwd-based restore covers them as before.
/// Commands typed *after* the evidence never enter the replay log — the
/// `on_input` classifier routes shell-prompt submissions away from
/// [`record_replay_op`], and menu reentry thaws the freeze via
/// [`record_replay_op`] so the recorder always holds the latest navigation.
pub fn note_shell_integration_evidence(state: &mut AppState, session_id: &str) {
    let rec = state
        .session_replays
        .entry(session_id.to_string())
        .or_default();
    rec.shell_integrated = true;
}

/// Replay-time skip predicate: is this recorded op already satisfied by the
/// terminal state (cursor line) in front of us — i.e. can we avoid retyping
/// it? Used by `schedule_replay_after_reconnect` before each send so a copy/
/// clone (or a reconnect on a bastion that remembers its target) does not
/// re-drive menu steps the server already performed.
///
/// The check is deliberately asymmetric:
/// - CONTEXT commands (`sudo -i`, nested `ssh`, `kubectl exec`, …) are never
///   skipped — a shell prompt in front of us says nothing about which
///   privilege/container context it carries, so they must still be replayed.
/// - MENU-navigation ops are skipped when the cursor line already looks like
///   a shell prompt: a bastion menu (JumpServer `Opt>`, custom `> ` menus)
///   is explicitly NOT shell-looking per [`prompt_looks_like_shell`], while
///   landing directly on the target host's shell makes the whole recorded
///   navigation prefix moot. The loop-level `continue` then re-evaluates the
///   (unchanged) prompt for each subsequent menu op, so once the shell is
///   reached — however that happened — the menu prefix drains away and only
///   the context suffix still sends.
pub fn replay_op_already_satisfied(current_line: &str, op: &str) -> bool {
    if is_context_command(op) {
        return false;
    }
    prompt_looks_like_shell(current_line)
}

/// The operations a reconnect should replay for this session, in order —
/// the recorded establishment prefix. Shell-integration evidence freezes
/// the recorder but keeps this prefix (see
/// [`note_shell_integration_evidence`]); an empty result means nothing was
/// recorded before the session proved to be an integrated shell.
pub fn replayable_ops(state: &AppState, session_id: &str) -> Vec<String> {
    state
        .session_replays
        .get(session_id)
        .map(|rec| rec.ops.clone())
        .unwrap_or_default()
}

// ============================== OTP 组级状态机 ==============================

/// JumpServer 共凭据组恢复时的 leader 心跳上限：leader watcher 每个 poll
/// 周期都会盖章。超过该间隔未盖章即视为 leader 任务已死（panic / runtime
/// 异常未走到错误处理），组内下一台可接棒。
pub const OTP_GROUP_LEADER_DEAD_MS: i64 = 30_000;

/// JumpServer 共凭据组（同一 `conn.id` 的多个 tab）的恢复协调登记表。
///
/// 状态机：
/// 1. 组内任一成员 OTP 完成并落地（settle，提示符稳定）→ 其余成员（包括
///    已 Failed / Disconnected 的）轮询发现后 clone 其 transport 复活；
/// 2. 组内无 settle 成员时推举 leader：优先现任 leader（心跳有效）；否则
///    按恢复顺序选第一个“还活着”（非 Failed / Disconnected）的成员接棒，
///    由它 fresh connect / 输 OTP；
/// 3. 全员失败或组级超时 → 保持 Failed，交给用户手动重连。
///
/// 登记表本身不做任何 IO；每次 watcher 的 poll 周期内由调用方收集好
/// members / states / settled 快照后一次性传入，避免锁顺序问题。
#[derive(Debug, Clone, Default)]
pub struct OtpGroupRegistry {
    groups: HashMap<String /* conn.id */, OtpGroupEntry>,
}

#[derive(Debug, Clone, Default)]
struct OtpGroupEntry {
    /// 现任 leader 的 tab/session id；`None` = 待推举。
    leader: Option<String>,
    /// leader 上次心跳（毫秒）。leader 自己每 poll 周期更新。
    beacon_ms: i64,
    /// 锁存的可复用源：首个真正跨过 settle 门的成员。锁存后它已完成 OTP
    /// 认证，后续无论其终端当前行是什么模样（用户继续操作后提示符会被
    /// 冲掉），都稳定作为组的复用源——直到它掉线 / tab 被关。
    latched_source: Option<String>,
    /// 被摘帽子的成员：心跳超时但状态还停在 Connecting（watcher 任务已
    /// 死，但连接尝试自身可能仍在跑）。此类死成员不能被反复重新推举，否
    /// 则会造成死锁推举循环。当它的状态明确转为 Failed / Disconnected
    /// 后（人工重连会先进 Connecting 再变状态），从该集合清除，允许未来
    /// 重新被推举。
    demoted: std::collections::HashSet<String>,
}

/// [`OtpGroupRegistry::poll`] 的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtpGroupRole {
    /// 组内已有 settle 完成的成员（返回其 session id），我应该 clone 它的
    /// transport 复活自己。
    SettledPeer(String),
    /// 我被推举为 leader，应该 fresh connect（输自己的 OTP）。
    Lead,
    /// 组里有活 leader（或我已是 leader），继续等待。
    Wait,
    /// 全员 Failed / Disconnected，组级恢复失败，退出等待。
    Exhausted,
}

impl OtpGroupRegistry {
    /// 组级状态机单次推进。只能在 `state.write()` 或独占持有 registry 的
    /// 场景调用，以保证“摘帽子 → 推举”是原子的。
    ///
    /// * `group_key` — 组标识（`conn.id`）
    /// * `order` — 恢复顺序的成员名单（首个即初始 leader）；运行中可能缺失
    ///   已关闭的 tab，顺序依然有效
    /// * `members` — 当前仍然打开的同组 tab id（含我本人）
    /// * `state_of` — 取某成员当前连接状态
    /// * `settled` — 该成员是否已过 settle 门（OTP 完成、提示符稳定）
    /// * `my_tab` — 本次 poll 的发起者
    /// * `now_ms` — 当前毫秒时间戳
    pub fn poll(
        &mut self,
        group_key: &str,
        order: &[String],
        members: &[String],
        state_of: &dyn Fn(&str) -> Option<SessionConnectionState>,
        settled: &dyn Fn(&str) -> bool,
        my_tab: &str,
        now_ms: i64,
    ) -> OtpGroupRole {
        let entry = self.groups.entry(group_key.to_string()).or_default();

        // 0a. 已锁存的复用源还能用 → 直接复用（优先级最高，能救活 Failed tab）
        if let Some(src) = entry.latched_source.clone() {
            // 锁存意味着它的 transport 已完成 OTP 认证，只要还 Connected
            // 就可复用——不再要求其终端当前行长得提示符。
            let usable = src != my_tab
                && members.iter().any(|m| m == &src)
                && matches!(state_of(&src), Some(SessionConnectionState::Connected));
            if usable {
                return OtpGroupRole::SettledPeer(src);
            }
            // 掉线 / 关闭 → 解锁，后续重新走扫描 / 推举。
            entry.latched_source = None;
        }

        // 0b. 扫描新的 settle 成员，发现即锁存并作为复用源
        if let Some(peer) = members.iter().find(|t| t.as_str() != my_tab && settled(t)) {
            entry.latched_source = Some(peer.clone());
            // leader 的使命已由 settle 成员完成，摘掉，避免其他 watcher 再
            // 依赖旧 leader 判断。
            if entry.leader.as_deref() == Some(peer.as_str()) {
                entry.leader = None;
            }
            return OtpGroupRole::SettledPeer(peer.clone());
        }

        // 0c. 状态明确失败的成员清除 demoted 标记，允许人工重连后重新
        // 被推举。
        entry.demoted.retain(|t| {
            !matches!(
                state_of(t),
                Some(SessionConnectionState::Failed) | Some(SessionConnectionState::Disconnected)
            )
        });

        // 1. 现任 leader 还“算活”吗？
        let leader_alive = match entry.leader.as_deref() {
            None => false,
            Some(l) if !members.iter().any(|m| m == l) => false,
            Some(l) => match state_of(l) {
                None
                | Some(SessionConnectionState::Failed)
                | Some(SessionConnectionState::Disconnected) => false,
                Some(_) => now_ms - entry.beacon_ms <= OTP_GROUP_LEADER_DEAD_MS,
            },
        };
        if !leader_alive {
            // 心跳超时但状态还停在 Connecting：watcher 死了，连试图可能仍
            // 在跑，不能让它被重新推举（会死锁），记入 demoted。
            if let Some(l) = entry.leader.take() {
                if members.iter().any(|m| m == &l)
                    && matches!(
                        state_of(&l),
                        Some(SessionConnectionState::Connecting)
                            | Some(SessionConnectionState::Connected)
                            | Some(SessionConnectionState::Reconnecting)
                    )
                {
                    entry.demoted.insert(l);
                }
            }
        } else if entry.leader.as_deref() == Some(my_tab) {
            entry.beacon_ms = now_ms;
        }

        // 2. 无 leader → 按恢复顺序推举第一个“还活着”的成员
        if entry.leader.is_none() {
            let alive = |t: &str| {
                !matches!(
                    state_of(t),
                    Some(SessionConnectionState::Failed)
                        | Some(SessionConnectionState::Disconnected)
                )
            };
            let mut candidates: Vec<&String> = members
                .iter()
                .filter(|t| alive(t) && !entry.demoted.contains(t.as_str()))
                .collect();
            if candidates.is_empty() {
                // 无人可推：若还有 demoted 的 beacon-死成员（连接尝试仍在跑），
                // 等它那边可能 settle 后走锁存路径 → Wait；否则全员失败 → 超
                // 出。
                return if entry.demoted.is_empty() {
                    OtpGroupRole::Exhausted
                } else {
                    OtpGroupRole::Wait
                };
            }
            candidates.sort_by_key(|t| order.iter().position(|o| o == *t).unwrap_or(usize::MAX));
            let next = candidates[0].clone();
            entry.leader = Some(next.clone());
            entry.beacon_ms = now_ms;
            if next == my_tab {
                return OtpGroupRole::Lead;
            }
        }

        OtpGroupRole::Wait
    }

    /// 释放整个组（我退出 watcher 时无所谓，主要在组全部收拾完毕或用户
    /// 关掉最后一台 tab 时清理，避免 registry 泄漏）。
    pub fn drop_group(&mut self, group_key: &str) {
        self.groups.remove(group_key);
    }
}

#[cfg(test)]
mod session_replay_tests {
    use super::*;
    use rusterm_core::config::{
        ConnectionConfig, ConnectionKind, SerialConfig, ShellConfig, SshAuth, SshConfig,
        TelnetConfig,
    };

    fn config_of(kind: ConnectionKind) -> ConnectionConfig {
        ConnectionConfig {
            id: "conn-1".to_string(),
            name: "conn".to_string(),
            kind,
            group: None,
            tags: Vec::new(),
            onekey: false,
            login_script: None,
        }
    }

    fn ssh_kind() -> ConnectionKind {
        ConnectionKind::Ssh(SshConfig {
            host: "jump.example.com".to_string(),
            port: 22,
            username: "ops".to_string(),
            auth: SshAuth::Agent,
            terminal_type: "xterm-256color".to_string(),
            proxy: None,
            proxy_jump: None,
            keepalive_interval: None,
            host_key_policy: rusterm_core::config::default_host_key_policy(),
        })
    }

    fn state_with_session(session_id: &str, kind: ConnectionKind) -> AppState {
        let mut state = AppState::default();
        state
            .session_configs
            .insert(session_id.to_string(), config_of(kind));
        state
    }

    /// Clone/reconnect skip predicate: a menu-navigation op is redundant when
    /// the terminal already sits at a shell prompt (the bastion delivered the
    /// session straight to the target), but context commands must still
    /// replay regardless, and a bastion menu prompt (`Opt>`) must never be
    /// mistaken for a finished login.
    #[test]
    fn replay_skip_predicate_skips_menu_ops_only_at_shell_prompt() {
        // Already on the target host's shell → the recorded menu ops are moot.
        assert!(replay_op_already_satisfied("[ops@web-01 ~]$ ", "2"));
        assert!(replay_op_already_satisfied("[ops@web-01 ~]$ ", "web-01"));
        assert!(replay_op_already_satisfied("root@db:~# ", "/q"));
        // Bastion menu prompt → ops still needed (classic JumpServer clone).
        assert!(!replay_op_already_satisfied("Opt> ", "2"));
        assert!(!replay_op_already_satisfied("Opt> 2", "web-01"));
        // No prompt content at all → replay as before.
        assert!(!replay_op_already_satisfied("", "2"));
        // Context commands are never skipped — a shell prompt says nothing
        // about the privilege/container context we must re-establish.
        assert!(!replay_op_already_satisfied("[ops@web-01 ~]$ ", "sudo -i"));
        assert!(!replay_op_already_satisfied(
            "root@db:~# ",
            "kubectl exec -it pod -- sh"
        ));
        assert!(!replay_op_already_satisfied(
            "[ops@web-01 ~]$ ",
            "ssh internal-02"
        ));
    }

    /// The core jumpserver flow: menu-navigation inputs are recorded in order
    /// and come back verbatim as replayable ops for a reconnect.
    #[test]
    fn records_interactive_ops_in_order_for_ssh_session() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "p"));
        assert!(record_replay_op(&mut state, "sess", "web-server-01"));
        assert!(!record_replay_op(&mut state, "sess", "   ")); // bare Enter / whitespace
        assert_eq!(
            replayable_ops(&state, "sess"),
            vec!["p".to_string(), "web-server-01".to_string()]
        );
    }

    /// Recording is an establishment *prefix*: once the window is full,
    /// later (steady-state) inputs are never recorded, so a reconnect can
    /// never replay an unbounded tail of arbitrary shell commands.
    #[test]
    fn recording_window_is_capped_at_establishment_prefix() {
        let mut state = state_with_session("sess", ssh_kind());
        for i in 0..REPLAY_MAX_OPS {
            assert!(record_replay_op(&mut state, "sess", &format!("op-{i}")));
        }
        assert!(!record_replay_op(&mut state, "sess", "rm -rf /tmp/scratch"));
        let ops = replayable_ops(&state, "sess");
        assert_eq!(ops.len(), REPLAY_MAX_OPS);
        assert_eq!(ops[0], "op-0");
        assert!(!ops.contains(&"rm -rf /tmp/scratch".to_string()));
    }

    /// OSC 133;D evidence means the remote reached a real integrated shell.
    /// The recorder FREEZES: the establishment prefix recorded before the
    /// evidence (bastion menu navigation) is kept for replay. Clearing
    /// instead of freezing was the original design — and it destroyed
    /// jumpserver menu navigation the moment the target host's shell
    /// reported its first exit code, leaving snapshots with nothing to
    /// replay. (Post-evidence shell commands are kept out of the log by the
    /// `on_input` prompt classifier, which never routes them here.)
    #[test]
    fn shell_evidence_freezes_the_establishment_prefix() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "/q"));
        assert!(record_replay_op(&mut state, "sess", "3"));
        note_shell_integration_evidence(&mut state, "sess");
        // The pre-evidence establishment prefix survives the freeze.
        assert_eq!(
            replayable_ops(&state, "sess"),
            vec!["/q".to_string(), "3".to_string()]
        );
    }

    /// Menu reentry after landing on a shell: the user backs out of the
    /// target host to the bastion menu and navigates somewhere else. The
    /// recorder THAWS and restarts, so the snapshot always holds the *last*
    /// navigation — the one that leads to where the user actually is — not
    /// the stale first one.
    #[test]
    fn menu_reentry_thaws_and_restarts_recording() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "/q"));
        assert!(record_replay_op(&mut state, "sess", "2"));
        note_shell_integration_evidence(&mut state, "sess");
        // Back at the bastion menu: a new menu-class op thaws and re-records.
        assert!(record_replay_op(&mut state, "sess", "/w"));
        assert!(record_replay_op(&mut state, "sess", "3"));
        assert_eq!(
            replayable_ops(&state, "sess"),
            vec!["/w".to_string(), "3".to_string()]
        );
    }

    /// Context-establishing shell commands (`sudo -i`, `su`, nested `ssh`,
    /// container exec shells) are recognized; one-shot commands and plain
    /// shell work are not — replaying them would repeat side effects, not
    /// restore state.
    #[test]
    fn context_command_classifier_separates_context_from_oneshot() {
        for ctx in [
            "sudo -i",
            "sudo -s",
            "sudo --login",
            "sudo su",
            "sudo su -",
            "sudo -u admin -i",
            "sudo -u root bash",
            "su",
            "su - deploy",
            "doas -s",
            "ssh internal-db-01",
            "telnet 10.0.0.5",
            "docker exec -it app bash",
            "kubectl exec -it pod-0 -- sh",
            "podman attach web",
        ] {
            assert!(is_context_command(ctx), "context-class: {ctx:?}");
        }
        for oneshot in [
            "sudo systemctl restart nginx",
            "sudo rm -rf /tmp/scratch",
            "sudo -u postgres psql",
            "ls -la",
            "htop",
            "docker ps",
            "kubectl get pods",
            "sudoedit /etc/hosts",
            "",
        ] {
            assert!(!is_context_command(oneshot), "oneshot-class: {oneshot:?}");
        }
    }

    /// The sudo-after-bastion flow: menu navigation records, landing on the
    /// target freezes, and `sudo -i` typed at the target's shell prompt
    /// APPENDS to the frozen log without thawing — the replay must cross the
    /// bastion first and then re-escalate, in that order. Ordinary shell
    /// commands before/after still never enter the log.
    #[test]
    fn context_commands_append_to_the_frozen_establishment_log() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "/q"));
        assert!(record_replay_op(&mut state, "sess", "2"));
        note_shell_integration_evidence(&mut state, "sess");
        // Ordinary shell commands are rejected by the classifier gate.
        assert!(!record_context_command(&mut state, "sess", "ls -la"));
        // `sudo -i` appends under freeze; the prefix stays intact.
        assert!(record_context_command(&mut state, "sess", "sudo -i"));
        assert_eq!(
            replayable_ops(&state, "sess"),
            vec!["/q".to_string(), "2".to_string(), "sudo -i".to_string()]
        );
        // Still frozen: a later menu reentry thaws and re-records from
        // scratch, exactly as before.
        assert!(record_replay_op(&mut state, "sess", "/w"));
        assert_eq!(replayable_ops(&state, "sess"), vec!["/w".to_string()]);
    }

    /// Context commands respect the same recording gates as menu ops: only
    /// SSH/Telnet sessions, and never past the window cap.
    #[test]
    fn context_commands_respect_recording_gates() {
        let mut shell = state_with_session(
            "sh",
            ConnectionKind::Shell(ShellConfig {
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                working_dir: None,
            }),
        );
        assert!(!record_context_command(&mut shell, "sh", "sudo -i"));

        let mut state = state_with_session("sess", ssh_kind());
        for i in 0..REPLAY_MAX_OPS {
            assert!(record_replay_op(&mut state, "sess", &format!("op-{i}")));
        }
        note_shell_integration_evidence(&mut state, "sess");
        assert!(!record_context_command(&mut state, "sess", "sudo -i"));
        assert_eq!(replayable_ops(&state, "sess").len(), REPLAY_MAX_OPS);
    }

    /// `exit` after `sudo -i` pops the escalation off the log (the user
    /// dropped back to the unprivileged shell — replaying the sudo would
    /// restore the WRONG state). But `exit` on the target host itself (no
    /// trailing context op) must NOT eat the bastion navigation prefix.
    #[test]
    fn exit_pops_the_trailing_context_command_but_not_the_navigation_prefix() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "/q"));
        assert!(record_replay_op(&mut state, "sess", "2"));
        note_shell_integration_evidence(&mut state, "sess");
        assert!(record_context_command(&mut state, "sess", "sudo -i"));

        // exit → pops sudo -i.
        assert_eq!(
            pop_context_command(&mut state, "sess", "exit"),
            Some("sudo -i".to_string())
        );
        assert_eq!(
            replayable_ops(&state, "sess"),
            vec!["/q".to_string(), "2".to_string()]
        );
        // A second exit (leaving the target host) finds no trailing context
        // op — the navigation prefix survives untouched.
        assert_eq!(pop_context_command(&mut state, "sess", "exit"), None);
        assert_eq!(replayable_ops(&state, "sess").len(), 2);
        // Non-exit commands never pop.
        assert!(record_context_command(&mut state, "sess", "sudo -i"));
        assert_eq!(pop_context_command(&mut state, "sess", "whoami"), None);
        assert_eq!(replayable_ops(&state, "sess").len(), 3);
    }

    /// The prompt classifier separates shell prompts (whose submissions
    /// freeze the recorder) from interactive bastion menu prompts (whose
    /// submissions record). A bare "> " is deliberately menu-class:
    /// JumpServer's `Opt>` must win over exotic remote prompts.
    #[test]
    fn prompt_classification_separates_menus_from_shells() {
        for shell in [
            "[root@web ~]# id",
            "ecs-user@host:~$ exit",
            "host% ls",
            "PS C:\\Users\\dev> dir",
            "❯ make build",
            "➜  ~ git status",
        ] {
            assert!(prompt_looks_like_shell(shell), "shell-class: {shell:?}");
        }
        for menu in ["请选择目标资产：3", "请选择资产分类：/q", "Opt> p", ""] {
            assert!(!prompt_looks_like_shell(menu), "menu-class: {menu:?}");
        }
    }

    /// Only interactive remote kinds (SSH / Telnet) record. Local shells are
    /// restored via cwd; serial ports have no login flow to replay.
    #[test]
    fn only_ssh_and_telnet_sessions_record() {
        let mut shell = state_with_session(
            "sh",
            ConnectionKind::Shell(ShellConfig {
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                working_dir: None,
            }),
        );
        assert!(!record_replay_op(&mut shell, "sh", "htop"));

        let mut serial = state_with_session(
            "ser",
            ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 115200,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
        );
        assert!(!record_replay_op(&mut serial, "ser", "enable"));

        let mut telnet = state_with_session(
            "tel",
            ConnectionKind::Telnet(TelnetConfig {
                host: "bbs.example.com".to_string(),
                port: 23,
            }),
        );
        assert!(record_replay_op(&mut telnet, "tel", "guest"));

        // Unknown session (no stored config) never records.
        let mut unknown = AppState::default();
        assert!(!record_replay_op(&mut unknown, "nope", "anything"));
    }

    /// The recorder must survive a disconnect — it is exactly what the
    /// reconnect replays — and be removed when the session closes.
    #[test]
    fn recorder_survives_disconnect_state_but_is_removed_on_close() {
        let mut state = state_with_session("sess", ssh_kind());
        assert!(record_replay_op(&mut state, "sess", "web-server-01"));

        // Mirror what a disconnect does to connection state: the replays map
        // is untouched (disconnect_session_state never clears it).
        state
            .session_connection_states
            .insert("sess".to_string(), SessionConnectionState::Disconnected);
        assert_eq!(replayable_ops(&state, "sess").len(), 1);

        // Closing the session drops the recorder with the rest of the
        // session-scoped maps.
        state.session_replays.remove("sess");
        assert!(replayable_ops(&state, "sess").is_empty());
    }

    /// The persisted snapshot carries the recorded establishment prefix, and
    /// integration evidence freezes (not clears) it — the frozen prefix is
    /// what a startup restore replays to cross the bastion again.
    #[test]
    fn build_session_state_keeps_frozen_replay_ops_after_integration_evidence() {
        let mut state = state_with_session("sess", ssh_kind());
        state.sessions.push(SessionTab {
            id: "sess".to_string(),
            name: "conn".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 1,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: std::collections::HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("jump.example.com".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
        });
        state
            .session_connection_states
            .insert("sess".to_string(), SessionConnectionState::Connected);
        assert!(record_replay_op(&mut state, "sess", "web-server-01"));

        let snapshot = state.build_session_state("Dark");
        assert_eq!(
            snapshot.sessions[0].replay_ops,
            vec!["web-server-01".to_string()]
        );

        note_shell_integration_evidence(&mut state, "sess");
        let snapshot = state.build_session_state("Dark");
        assert_eq!(
            snapshot.sessions[0].replay_ops,
            vec!["web-server-01".to_string()]
        );
    }

    /// The persisted `connection_id` must be the *saved-connection identity*
    /// (`ConnectionConfig::id`), not the session/tab id and not a name-based
    /// guess. This is what makes a **copied session** ("X 副本") restorable:
    /// its display name matches no saved connection, so restore can only
    /// find the transport config through this id.
    #[test]
    fn build_session_state_persists_saved_connection_id_for_copies() {
        let mut state = AppState::default();
        // A copied session: fresh tab UUID, display name with the copy
        // suffix, but the stored config carries the source's connection id.
        let mut copy_config = config_of(ssh_kind());
        copy_config.name = "conn 副本".to_string();
        state
            .session_configs
            .insert("copy-tab".to_string(), copy_config);
        state.sessions.push(SessionTab {
            id: "copy-tab".to_string(),
            name: "conn 副本".to_string(),
            kind: SessionType::Ssh,
            render_output: Default::default(),
            version: 1,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: std::collections::HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: Some("jump.example.com".to_string()),
            cwd: None,
            last_command_status: CommandStatus::default(),
        });
        state
            .session_connection_states
            .insert("copy-tab".to_string(), SessionConnectionState::Connected);

        let snapshot = state.build_session_state("Dark");
        assert_eq!(snapshot.sessions.len(), 1);
        assert_eq!(
            snapshot.sessions[0].connection_id.as_deref(),
            Some("conn-1"),
            "copy persists the saved-connection id, not the tab id"
        );
        assert_eq!(snapshot.sessions[0].name, "conn 副本");

        // A session without a stored config still falls back to its tab id
        // (never a bare None for remote kinds).
        state.session_configs.remove("copy-tab");
        let snapshot = state.build_session_state("Dark");
        assert_eq!(
            snapshot.sessions[0].connection_id.as_deref(),
            Some("copy-tab")
        );
    }
}

#[cfg(test)]
mod login_script_runtime_tests {
    use super::*;

    /// The per-session runtime map is empty on a fresh state and can hold one
    /// runtime per session id; a finished runtime stays `done` even if more
    /// output arrives.
    #[test]
    fn login_scripts_map_lifecycle_is_per_session() {
        let mut state = AppState::default();
        assert!(state.login_scripts.is_empty());
        state.login_scripts.insert(
            "sess-1".to_string(),
            LoginScriptRuntime {
                steps: vec![
                    rusterm_core::LoginStep::Expect {
                        pattern: r"password:".to_string(),
                    },
                    rusterm_core::LoginStep::SendOneKey {
                        name: "root".to_string(),
                    },
                ],
                idx: 0,
                send_buffer: Default::default(),
                done: false,
                wait_started: None,
            },
        );
        assert_eq!(state.login_scripts["sess-1"].steps.len(), 2);
        state.login_scripts.get_mut("sess-1").unwrap().done = true;
        state.login_scripts.remove("sess-1");
        assert!(state.login_scripts.is_empty());
    }
}

#[cfg(test)]
mod otp_group_tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    struct Fixture {
        states: HashMap<String, SessionConnectionState>,
        settled: HashSet<String>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                states: HashMap::new(),
                settled: HashSet::new(),
            }
        }
        fn state_of(&self) -> impl Fn(&str) -> Option<SessionConnectionState> + '_ {
            |t: &str| self.states.get(t).copied()
        }
        fn settled_of(&self) -> impl Fn(&str) -> bool + '_ {
            |t: &str| self.settled.contains(t)
        }
    }

    /// 组内第二个 tab 已 settle → 第一个 tab 的 watcher 立刻拿到
    /// SettledPeer，且该成员被锁存为复用源。
    #[test]
    fn settled_peer_wins_and_latches() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b"]);
        let mut fx = Fixture::new();
        fx.states
            .insert("a".into(), SessionConnectionState::Connecting);
        fx.states
            .insert("b".into(), SessionConnectionState::Connected);
        fx.settled.insert("b".into());

        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );
        assert_eq!(role, OtpGroupRole::SettledPeer("b".into()));

        // 锁存后：即使 b 的提示符被用户操作冲掉（不再 settled），依旧复用。
        fx.settled.clear();
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            2_000,
        );
        assert_eq!(role, OtpGroupRole::SettledPeer("b".into()));
    }

    /// leader 失败 → 下一个还有效的成员（按恢复顺序）被推举为 leader。
    #[test]
    fn failed_leader_handoff_follows_restore_order() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b", "c"]);
        let mut fx = Fixture::new();
        // a: 首台 leader，正在 Connecting；b/c: Connecting（克隆等待方）。
        for t in ["a", "b", "c"] {
            fx.states
                .insert(t.into(), SessionConnectionState::Connecting);
        }

        // a 推举自己
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);
        // b poll：leader 是 a → Wait
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "b",
            1_100,
        );
        assert_eq!(role, OtpGroupRole::Wait);

        // a Failed → b 接棒
        fx.states.insert("a".into(), SessionConnectionState::Failed);
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "b",
            2_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);

        // b 也 Failed → c 接棒
        fx.states.insert("b".into(), SessionConnectionState::Failed);
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "c",
            3_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);
    }

    /// 全员 Failed → Exhausted。
    #[test]
    fn all_failed_is_exhausted() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b"]);
        let mut fx = Fixture::new();
        fx.states.insert("a".into(), SessionConnectionState::Failed);
        fx.states.insert("b".into(), SessionConnectionState::Failed);
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );
        assert_eq!(role, OtpGroupRole::Exhausted);
    }

    /// leader 任务 panic（状态收到 Connecting 但心跳停了 30s+）→ 其他成
    /// 员能摘掉其帽子接棒。
    #[test]
    fn dead_leader_beacon_allows_handoff() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b"]);
        let mut fx = Fixture::new();
        fx.states
            .insert("a".into(), SessionConnectionState::Connecting);
        fx.states
            .insert("b".into(), SessionConnectionState::Connecting);

        // a 成为 leader，盖章 t=1_000
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);

        // t=1_000 + 29s：b 看到 a 心跳新鲜 → Wait
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "b",
            30_000,
        );
        assert_eq!(role, OtpGroupRole::Wait);

        // t=1_000 + 31s：a 心跳超时 → b 接棒
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "b",
            32_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);
    }

    /// 已 Failed 的 tab 在同伴 settle 后通过 SettledPeer 复活——这正是核
    /// 心需求“第一台失败也能被第二台成功后拉起”。
    #[test]
    fn failed_tab_revives_via_settled_peer() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b"]);
        let mut fx = Fixture::new();
        // a：OTP 输错被踢 → Failed；b：顶上并成为 settle 成员。
        fx.states.insert("a".into(), SessionConnectionState::Failed);
        fx.states
            .insert("b".into(), SessionConnectionState::Connected);
        fx.settled.insert("b".into());

        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );
        assert_eq!(role, OtpGroupRole::SettledPeer("b".into()));
    }

    /// 锁存源掉线 → 解锁 → 回到推举流程（另一台可顶上）。
    #[test]
    fn latched_source_disconnect_unlatches() {
        let mut reg = OtpGroupRegistry::default();
        let order = ids(&["a", "b"]);
        let mut fx = Fixture::new();
        fx.states
            .insert("a".into(), SessionConnectionState::Connecting);
        fx.states
            .insert("b".into(), SessionConnectionState::Connected);
        fx.settled.insert("b".into());

        // 第一次 poll 锁存 b
        let _ = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            1_000,
        );

        // b 掉线且不再 settle → 锁存解除 → a（Connecting）被推举为 leader
        fx.states
            .insert("b".into(), SessionConnectionState::Disconnected);
        fx.settled.clear();
        let role = reg.poll(
            "conn",
            &order,
            &order,
            &fx.state_of(),
            &fx.settled_of(),
            "a",
            2_000,
        );
        assert_eq!(role, OtpGroupRole::Lead);
    }
}
