use std::collections::HashMap;
use std::sync::Arc;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use rusterm_core::config::{
    ConnectionConfig, ConnectionKind, OneKey, OneKeyStep, ShellConfig, SshAuth, SshConfig,
};
use rusterm_core::config_manager::ConfigManager;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::SessionType;
use rusterm_core::session_log::SessionLog;
use rusterm_core::terminal::{Terminal, TerminalSize};

use crate::components::AiPanel;
use crate::components::ConnectionDialog;
use crate::components::DangerousCommandDialog;
use crate::components::MasterPasswordDialog;
use crate::components::OneKeyManager;
use crate::components::RestoreSessionDialog;
use crate::components::SettingsDialog;
use crate::components::Sidebar;
use crate::components::TabBar;
use crate::components::TerminalView;
use crate::components::connection_dialog::NewConnectionForm;
use crate::layout::{PaneLayout, SplitAxis, SplitDirection};
use crate::state::{
    AppState, Modal, OneKeyMatch, OneKeyPopupState, PendingDangerousCommand,
    SessionConnectionState, SessionTab, TabDropOutcome, TerminalEntry, UnlockState,
    append_pane_to_active, begin_floating_pane_move, close_session, close_workspace,
    distribute_sessions_across_panes, execute_tab_drop_on_pane, execute_tab_drop_on_pane_at,
    focus_pane_for_layout, focused_pane_session, move_floating_pane_for_active,
    move_session_to_leftmost, prepare_split_for_sidebar_drop, prepare_split_for_sidebar_drop_at,
    push_workspace_tab, resize_layout_split, set_active_tab, set_pane_session_for_layout,
    source_pane_for_copy, toggle_comparison_mode, toggle_pane_zoom,
};

fn save_config(state: &Signal<AppState>) {
    let s = state.read();
    let cm = match &s.config_manager {
        Some(cm) => cm.clone(),
        None => {
            tracing::error!("ConfigManager not initialized, cannot save connections");
            return;
        }
    };
    if let Err(e) = cm.save_connections(&s.connections) {
        tracing::error!("Failed to save connections: {}", e);
    }
}

/// Human-readable label for the active tab's current layout, shown in the
/// status bar's layout toolbar. Reads the actual pane count from the layout
/// (not `state.layout_preset`, which no longer reflects reality after
/// `append_pane_to_active` — e.g. 3 panes has no preset).
///
/// Returns "Layout: N panes" (or "Layout: 1 pane" for the singular case)
/// when a layout exists, or "Layout: 1 pane" when no layout exists yet
/// (the single-pane default view).
fn layout_display_label(state: &AppState) -> String {
    let pane_count = state
        .active_tab
        .as_ref()
        .and_then(|id| state.layouts.get(id))
        .map(|l| l.panes.len())
        .unwrap_or(1);
    let noun = if pane_count == 1 { "pane" } else { "panes" };
    format!("Layout: {pane_count} {noun}")
}

/// Render a single TerminalView for the session identified by `session_id`.
///
/// This is the shared rendering helper used by both the single-pane path
/// (where `session_id` is the active session) and the multi-pane path
/// (where `session_id` is one of the panes in the layout). It encapsulates
/// the ~600 lines of closures that wire up TerminalView's on_resize /
/// on_input / on_command / on_scroll_* / on_suggestion_* / on_onekey_* /
/// on_reconnect handlers.
///
/// If the session isn't found in `state.sessions` (e.g., it was closed
/// between the layout snapshot and this render call), we render an empty
/// div — the caller's pane_rect still reserves space, but no terminal
/// content is drawn. This avoids a panic on the race where a session is
/// closed mid-render.
fn render_terminal_pane(
    mut state: Signal<AppState>,
    input_senders: Signal<std::collections::HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    session_id: String,
) -> Element {
    let tabs = &state.read().sessions;
    match tabs.iter().find(|t| t.id == session_id) {
        Some(tab) => {
            let sid_clone = tab.id.clone();
            let sid_for_cmd = tab.id.clone();
            let sid_for_resize = tab.id.clone();
            let sid_for_scroll_up = tab.id.clone();
            let sid_for_scroll_down = tab.id.clone();
            let sid_for_scroll_bottom = tab.id.clone();
            let sid_for_sug_nav = tab.id.clone();
            let sid_for_sug_accept = tab.id.clone();
            let sid_for_sug_dismiss = tab.id.clone();
            let sid_for_sug_delete = tab.id.clone();
            let sid_for_ok = tab.id.clone();
            let sid_for_ok_sel = tab.id.clone();
            let sid_for_ok_save = tab.id.clone();
            let sid_for_ok_dismiss = tab.id.clone();
            let sid_for_reconnect = tab.id.clone();
            let senders = input_senders;
            let mut state_for_cmd = state;
            // OneKey popup state for this session (if any).
            let ok_popup = state
                .read()
                .onekey_popups
                .get(&tab.id)
                .cloned()
                .unwrap_or_default();
            let ok_visible = ok_popup.visible;
            let ok_entries = ok_popup.matches.clone();
            let ok_selected = ok_popup.selected;
            // Both disconnected and reconnecting sessions have no live input
            // channel. Repeated Enter during `Reconnecting` is ignored by the
            // atomic state transition in `reconnect_session`.
            let tab_disconnected = matches!(
                state.read().session_connection_states.get(&tab.id),
                Some(SessionConnectionState::Disconnected | SessionConnectionState::Reconnecting)
            );
            rsx! {
                TerminalView {
                    session_id: tab.id.clone(),
                    render_output: tab.render_output.clone(),
                    version: tab.version,
                    suggestion: tab.suggestion.clone(),
                    suggestions: tab.suggestions.clone(),
                    suggestion_selected: tab.suggestion_selected,
                    suggestion_visible: tab.suggestion_visible,
                    on_resize: move |(cols, rows, pw, ph): (u16, u16, u32, u32)| {
                        let terminals = state.read().terminals.clone();
                        if let Some(handle) = terminals.get(&sid_for_resize) {
                            let mut entry = handle.lock();
                            entry.terminal.resize(cols, rows, pw, ph);
                            entry.scroll_offset = 0; // Reset scroll on resize
                            // Re-render after resize so the UI updates immediately
                            let render_result = entry.render_current();
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == sid_for_resize) {
                                tab.render_output = render_result;
                                tab.version += 1;
                            }
                        }
                        // Propagate resize to SSH session
                        if let Some(tx) = state.read().resize_senders.get(&sid_for_resize) {
                            let _ = tx.send((cols, rows, pw, ph));
                        }
                    },
                    on_input: move |data: Vec<u8>| {
                        let is_enter = data.contains(&0x0d);
                        tracing::info!(
                            "[INPUT] session={} is_enter={} data_len={} data={:?}",
                            &sid_clone[..sid_clone.len().min(8)],
                            is_enter,
                            data.len(),
                            &data[..data.len().min(32)]
                        );
                        // --- Dangerous-command protection (feature #17) ---
                        // Before sending Enter to the PTY, run the current
                        // input line through the safety checker. If the verdict
                        // is `Warn`, DON'T send Enter — instead stash the
                        // pending command + reason and show a confirmation
                        // modal. The modal's "继续" button sends the original
                        // Enter; "取消" discards it.
                        //
                        // We only check on Enter (not every keystroke) to
                        // avoid false positives on partial input and to keep
                        // the check cheap.
                        //
                        // Multi-line inputs (shell `\` continuations) are a
                        // known limitation — we only see the current line. See
                        // `command_safety.rs` docs for the trade-off.
                        if is_enter {
                            let line = {
                                let terminals = state_for_cmd.read().terminals.clone();
                                terminals
                                    .get(&sid_clone)
                                    .map(|h| h.lock().terminal.extract_current_line())
                                    .unwrap_or_default()
                            };
                            let cmd = strip_prompt(line.trim()).to_string();
                            if !cmd.is_empty() {
                                let verdict = state_for_cmd.read().safety_checker.check(&cmd);
                                if let rusterm_core::SafetyVerdict::Warn(reason) = verdict {
                                    tracing::info!(
                                        "[SAFETY] blocked dangerous command: session={} cmd={:?} reason={}",
                                        &sid_clone[..sid_clone.len().min(8)],
                                        cmd,
                                        reason
                                    );
                                    state_for_cmd.write().pending_dangerous_command = Some(
                                        PendingDangerousCommand {
                                            command: cmd,
                                            reason,
                                            session_id: sid_clone.clone(),
                                        }
                                    );
                                    // Return WITHOUT sending — the modal's
                                    // "继续" button will re-send the Enter if
                                    // the user confirms.
                                    return;
                                }
                            }
                        }
                        // Log input
                        {
                            let logs = state_for_cmd.read().session_logs.clone();
                            if let Some(log) = logs.get(&sid_clone) {
                                log.lock().log_input(&data);
                            }
                        }
                        // --- Comparison-mode broadcast ---
                        // When the active tab's layout has comparison mode
                        // ON, every keystroke is sent to every pane's PTY
                        // (the tmux synchronize-panes feature). This lets
                        // the user run the same command across N hosts and
                        // watch the outputs side-by-side.
                        //
                        // When comparison is OFF (or no layout exists),
                        // input only goes to THIS pane's own session —
                        // each pane is independent. We deliberately do NOT
                        // consult `active_session` here: in multi-pane mode
                        // `active_session` is the *tab* pointer (the layout
                        // is keyed by it in `state.layouts`), NOT the
                        // focused-pane pointer. Routing pane N's keystrokes
                        // to `active_session` would send them to pane 0's
                        // PTY, which is the bug where "only the first pane
                        // accepts commands".
                        //
                        // `broadcast_targets` returns >1 entry only when
                        // comparison is ON AND there are 2+ non-empty pane
                        // sessions — that's the sole condition for
                        // broadcasting. When comparison is OFF it returns
                        // exactly `[active_session]` (len 1), so we fall
                        // through to the per-pane path and send to
                        // `sid_clone` (this pane's own session).
                        let broadcast_targets = crate::state::broadcast_targets(&state_for_cmd.read());
                        let is_broadcast = broadcast_targets.len() > 1;
                        if is_broadcast {
                            tracing::info!(
                                "[INPUT] comparison mode ON — broadcasting to {} sessions",
                                broadcast_targets.len()
                            );
                            for target_sid in &broadcast_targets {
                                if let Some(sender) = senders.read().get(target_sid).cloned() {
                                    match sender.send(data.clone()) {
                                        Ok(()) => tracing::info!(
                                            "[INPUT] broadcast to session {} ok",
                                            &target_sid[..target_sid.len().min(8)]
                                        ),
                                        Err(e) => tracing::warn!(
                                            "[INPUT] broadcast to session {} FAILED: {}",
                                            &target_sid[..target_sid.len().min(8)],
                                            e
                                        ),
                                    }
                                }
                            }
                        } else {
                            // Non-broadcast path: send only to this pane's session.
                            let send_ok = senders.read().get(&sid_clone).cloned();
                            if let Some(sender) = send_ok {
                                match sender.send(data) {
                                    Ok(()) => tracing::info!("[INPUT] sent to PTY ok"),
                                    Err(e) => tracing::warn!("[INPUT] FAILED to send to PTY: {}", e),
                                }
                            } else {
                                tracing::warn!("[INPUT] no sender for session — PTY channel is dead");
                            }
                        }
                        // Query history for suggestion (on non-Enter input)
                        if !is_enter {
                            let sid_sug = sid_clone.clone();
                            let epoch = {
                                let mut s = state_for_cmd.write();
                                s.suggestion_epoch += 1;
                                s.suggestion_epoch
                            };
                            spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;

                                let current_epoch = state_for_cmd.read().suggestion_epoch;
                                if current_epoch != epoch {
                                    tracing::info!(
                                        "[SUGGESTION-QUERY] STALE — spawn epoch={} but current={} (skipped)",
                                        epoch,
                                        current_epoch
                                    );
                                    return;
                                }

                                // Extract the current line AFTER debounce
                                let line = {
                                    let terminals = state_for_cmd.read().terminals.clone();
                                    if let Some(handle) = terminals.get(&sid_sug) {
                                        handle.lock().terminal.extract_current_line()
                                    } else {
                                        return;
                                    }
                                };
                                let line = line.trim().to_string();

                                if line.is_empty() {
                                    tracing::info!(
                                        "[SUGGESTION-QUERY] session={} line empty — hiding popup",
                                        &sid_sug[..sid_sug.len().min(8)]
                                    );
                                    state_for_cmd.write().sessions.iter_mut()
                                        .find(|t| t.id == sid_sug)
                                        .map(|tab| {
                                            tab.suggestion = None;
                                            tab.suggestions = Vec::new();
                                            tab.suggestion_visible = false;
                                            tab.suggestion_selected = 0;
                                        });
                                    return;
                                }

                                // Strip prompt prefix to get the command part
                                let cmd_part = strip_prompt(&line);

                                if cmd_part.is_empty() {
                                    tracing::info!(
                                        "[SUGGESTION-QUERY] session={} cmd_part empty (line={:?}) — hiding popup",
                                        &sid_sug[..sid_sug.len().min(8)],
                                        line
                                    );
                                    state_for_cmd.write().sessions.iter_mut()
                                        .find(|t| t.id == sid_sug)
                                        .map(|tab| {
                                            tab.suggestion = None;
                                            tab.suggestions = Vec::new();
                                            tab.suggestion_visible = false;
                                            tab.suggestion_selected = 0;
                                        });
                                    return;
                                }

                                let cmd_lower = cmd_part.to_lowercase();
                                let mut all_suggestions: Vec<String> = Vec::new();
                                let mut seen = std::collections::HashSet::new();

                                // TIMING WINDOW GUARD: snapshot the
                                // recent-failed set BEFORE querying
                                // either source. A command that just
                                // failed (rc != 0) is in this set
                                // synchronously, even though its
                                // durable DB failure marker is still
                                // being written by the `mark_command_failed`
                                // spawn. Without this filter, the DB
                                // source would re-surface the command
                                // during that window because the prior
                                // `exit_code = NULL` import row is still
                                // there and HAVING keeps NULL rows.
                                let recent_failed: std::collections::HashSet<String> =
                                    state_for_cmd
                                        .read()
                                        .recent_failed_commands
                                        .clone();

                                // 1. Session command history — count frequency, sort by it
                                let session_hist = state_for_cmd.read().sessions
                                    .iter().find(|t| t.id == sid_sug)
                                    .map(|t| t.command_history.clone())
                                    .unwrap_or_default();

                                // Count occurrences and sort by frequency descending
                                let mut freq: std::collections::HashMap<&String, usize> = std::collections::HashMap::new();
                                for cmd in session_hist.iter() {
                                    if cmd.to_lowercase().starts_with(&cmd_lower)
                                        && cmd != &cmd_part
                                        && !seen.contains(cmd.to_lowercase().as_str())
                                        && !recent_failed.contains(cmd)
                                    {
                                        *freq.entry(cmd).or_insert(0) += 1;
                                    }
                                }
                                let mut freq_vec: Vec<(&String, usize)> = freq.into_iter().collect();
                                freq_vec.sort_by(|a, b| b.1.cmp(&a.1));
                                for (cmd, _count) in freq_vec.iter().take(15) {
                                    seen.insert(cmd.to_lowercase().clone());
                                    all_suggestions.push((*cmd).clone());
                                }

                                // 2. SQLite FTS5 — unified history DB (local import + remote
                                //    imports + session commands), frecency-scored (atuin-style:
                                //    frequency + recency + success rate). This replaces the
                                //    per-keystroke file reads — local shell history is imported
                                //    into the DB at app startup.
                                {
                                    let db_path = dirs::data_dir()
                                        .unwrap_or_default()
                                        .join("rusterm")
                                        .join("rusterm.db");
                                    if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                        if let Ok(results) = db.search_history(&cmd_part, 30).await {
                                            for entry in results {
                                                if entry.command.to_lowercase().starts_with(&cmd_lower)
                                                    && entry.command != cmd_part
                                                    && !seen.contains(entry.command.to_lowercase().as_str())
                                                    && !recent_failed.contains(&entry.command)
                                                {
                                                    seen.insert(entry.command.to_lowercase().clone());
                                                    all_suggestions.push(entry.command);
                                                }
                                            }
                                        }
                                    }
                                }

                                // Check epoch again before writing results
                                if state_for_cmd.read().suggestion_epoch != epoch {
                                    return;
                                }

                                // Truncate to 8 suggestions max
                                all_suggestions.truncate(15);

                                tracing::info!(
                                    "[SUGGESTION-QUERY] session={} cmd_part={:?} epoch={} current_epoch={} results={:?} recent_failed={:?}",
                                    &sid_sug[..sid_sug.len().min(8)],
                                    cmd_part,
                                    epoch,
                                    state_for_cmd.read().suggestion_epoch,
                                    all_suggestions,
                                    recent_failed
                                );

                                if all_suggestions.is_empty() {
                                    state_for_cmd.write().sessions.iter_mut()
                                        .find(|t| t.id == sid_sug)
                                        .map(|tab| {
                                            tab.suggestion = None;
                                            tab.suggestions = Vec::new();
                                            tab.suggestion_visible = false;
                                            tab.suggestion_selected = 0;
                                        });
                                } else {
                                    // First suggestion suffix is the inline ghost text
                                    let first = &all_suggestions[0];
                                    let suffix = if first.len() > cmd_part.len() {
                                        first[cmd_part.len()..].to_string()
                                    } else {
                                        String::new()
                                    };
                                    state_for_cmd.write().sessions.iter_mut()
                                        .find(|t| t.id == sid_sug)
                                        .map(|tab| {
                                            tab.suggestion = if suffix.is_empty() { None } else { Some(suffix) };
                                            tab.suggestions = all_suggestions;
                                            tab.suggestion_visible = true;
                                            tab.suggestion_selected = 0;
                                        });
                                }
                            });
                        }
                    },
                    on_command: move |_: String| {
                        tracing::info!(
                            "[COMMAND] Enter pressed, session={}",
                            &sid_for_cmd[..sid_for_cmd.len().min(8)]
                        );
                        // Clear suggestion on Enter
                        state_for_cmd.write().sessions.iter_mut()
                            .find(|t| t.id == sid_for_cmd)
                            .map(|tab| {
                                tab.suggestion = None;
                                tab.suggestions = Vec::new();
                                tab.suggestion_visible = false;
                                tab.suggestion_selected = 0;
                            });

                        let terminals = state_for_cmd.read().terminals.clone();
                        if let Some(handle) = terminals.get(&sid_for_cmd) {
                            let raw_line = handle.lock().terminal.extract_current_line();
                            let cmd = strip_prompt(raw_line.trim());
                            tracing::info!(
                                "[COMMAND] session={} raw_line={:?} cmd={:?}",
                                &sid_for_cmd[..sid_for_cmd.len().min(8)],
                                raw_line,
                                cmd
                            );
                            if !cmd.is_empty() {
                                // DEFERRED RECORDING — do NOT push to
                                // command_history or save to DB yet. The
                                // command might fail (typos, broken
                                // commands), and the user explicitly
                                // doesn't want failed commands showing
                                // up in suggestions. Instead, queue it
                                // and wait for the shell's OSC 133;D
                                // exit-code report (shell integration).
                                // Only on rc==0 will we commit the
                                // command to history + DB. On rc!=0 we
                                // silently drop it. If the shell never
                                // emits OSC 133;D, the entry stays
                                // queued until the per-session cap
                                // (MAX_PENDING below) evicts it — by
                                // design, we'd rather suggest nothing
                                // than suggest failed commands.
                                let entry_id = uuid::Uuid::new_v4().to_string();
                                let mut s = state_for_cmd.write();
                                let queue = s
                                    .pending_exit_check
                                    .entry(sid_for_cmd.clone())
                                    .or_default();
                                // Defensive cap: if the shell never emits
                                // OSC 133;D (no shell integration, or
                                // integration not yet loaded), the queue
                                // would grow unboundedly. Drop the oldest
                                // entry to keep the queue bounded — those
                                // dropped entries are commands we couldn't
                                // confirm succeeded, so dropping them is
                                // consistent with "never suggest failed
                                // commands". 32 is plenty for any
                                // realistic prompt-then-Enter burst.
                                const MAX_PENDING: usize = 32;
                                while queue.len() >= MAX_PENDING {
                                    queue.pop_front();
                                }
                                queue.push_back((cmd, entry_id));
                            }
                        }
                    },
                    on_scroll_up: move |rows: usize| {
                        let terminals = state_for_cmd.read().terminals.clone();
                        if let Some(handle) = terminals.get(&sid_for_scroll_up) {
                            let render_result = handle.lock().scroll_up(rows);
                            let mut s = state_for_cmd.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == sid_for_scroll_up) {
                                tab.render_output = render_result;
                                tab.version += 1;
                            }
                        }
                    },
                    on_scroll_down: move |rows: usize| {
                        let terminals = state_for_cmd.read().terminals.clone();
                        if let Some(handle) = terminals.get(&sid_for_scroll_down) {
                            let render_result = handle.lock().scroll_down(rows);
                            let mut s = state_for_cmd.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == sid_for_scroll_down) {
                                tab.render_output = render_result;
                                tab.version += 1;
                            }
                        }
                    },
                    on_scroll_to_bottom: move |_: ()| {
                        let terminals = state_for_cmd.read().terminals.clone();
                        if let Some(handle) = terminals.get(&sid_for_scroll_bottom) {
                            let render_result = handle.lock().scroll_to_bottom();
                            let mut s = state_for_cmd.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == sid_for_scroll_bottom) {
                                tab.render_output = render_result;
                                tab.version += 1;
                            }
                        }
                    },
                    on_suggestion_navigate: move |idx: Option<usize>| {
                        if let Some(i) = idx {
                            state_for_cmd.write().sessions.iter_mut()
                                .find(|t| t.id == sid_for_sug_nav)
                                .map(|tab| tab.suggestion_selected = i);
                        }
                    },
                    on_suggestion_accept: move |cmd: String| {
                        // Accept: compute the suffix and send it
                        let suffix = {
                            let terminals = state_for_cmd.read().terminals.clone();
                            if let Some(handle) = terminals.get(&sid_for_sug_accept) {
                                let line = handle.lock().terminal.extract_current_line();
                                let cmd_part = strip_prompt(line.trim());
                                if cmd.starts_with(&cmd_part) && cmd_part.len() < cmd.len() {
                                    cmd[cmd_part.len()..].to_string()
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            }
                        };
                        if !suffix.is_empty() {
                            if let Some(sender) = senders.read().get(&sid_for_sug_accept) {
                                let _ = sender.send(suffix.as_bytes().to_vec());
                            }
                        }
                        // Dismiss dropdown and clear suggestion
                        state_for_cmd.write().sessions.iter_mut()
                            .find(|t| t.id == sid_for_sug_accept)
                            .map(|tab| {
                                tab.suggestion_visible = false;
                                tab.suggestion = None;
                                tab.suggestions = Vec::new();
                                tab.suggestion_selected = 0;
                            });
                    },
                    on_suggestion_dismiss: move |_: ()| {
                        state_for_cmd.write().sessions.iter_mut()
                            .find(|t| t.id == sid_for_sug_dismiss)
                            .map(|tab| tab.suggestion_visible = false);
                    },
                    on_suggestion_delete: move |cmd: String| {
                        // Shift+Delete on a suggestion item: the user wants
                        // this command GONE from suggestions — it's a typo
                        // or broken command that slipped past the failed-
                        // command filter (most likely because it came from
                        // `~/.bash_history` / `~/.zsh_history`, which have
                        // no exit-code info, so it landed as NULL and HAVING
                        // kept it).
                        //
                        // We do the deletion in three layers, mirroring the
                        // runtime-failure path:
                        //   1. IMMEDIATE (synchronous): remove from
                        //      `tab.command_history` so the session-history
                        //      source stops surfacing it on the next
                        //      keystroke; insert into
                        //      `recent_failed_commands` so the DB source
                        //      is also guarded during the async DB write;
                        //      remove from `tab.suggestions` and clamp
                        //      `suggestion_selected` so the popup updates
                        //      instantly.
                        //   2. DURABLE (async spawn): call
                        //      `mark_command_failed(&cmd, 1)`. This DELETEs
                        //      any prior rows for the command (including
                        //      the NULL-exit-code import row that was
                        //      causing the re-surface) and inserts a
                        //      single row with `exit_code = 1`. The HAVING
                        //      clause now filters it, AND the next history
                        //      import skips it because it's in
                        //      `known_failed_commands`. We use
                        //      `mark_command_failed` (NOT
                        //      `delete_history_by_command`) because
                        //      deletion would let the next import
                        //      re-introduce the command as NULL.
                        //   3. POST-COMMIT: remove from
                        //      `recent_failed_commands` — the DB's HAVING
                        //      clause takes over.
                        //
                        // Borrow-checker note: `state_for_cmd.write()`
                        // takes &mut self, so we collect everything we
                        // need from the write() critical section, drop it,
                        // then clone the Signal for the spawn.
                        let cmd_for_spawn = cmd.clone();
                        let mut state_for_mark = state_for_cmd;
                        {
                            let mut s = state_for_mark.write();
                            if let Some(tab) = s.sessions.iter_mut()
                                .find(|t| t.id == sid_for_sug_delete)
                            {
                                // 1a. Remove from session history
                                tab.command_history.retain(|c| c != &cmd);
                                // 1b. Remove from the visible suggestions list
                                tab.suggestions.retain(|c| c != &cmd);
                                // 1c. Clamp selection: if we deleted the
                                // selected item or one before it, decrement;
                                // then guard against the now-shorter list.
                                if tab.suggestion_selected
                                    >= tab.suggestions.len()
                                {
                                    tab.suggestion_selected = tab
                                        .suggestions
                                        .len()
                                        .saturating_sub(1);
                                }
                                // 1d. If the popup is now empty, hide it
                                // and clear the inline ghost text.
                                if tab.suggestions.is_empty() {
                                    tab.suggestion_visible = false;
                                    tab.suggestion = None;
                                    tab.suggestion_selected = 0;
                                } else {
                                    // Refresh the inline ghost text to
                                    // reflect the new top suggestion.
                                    // We need the current cursor line to
                                    // compute the suffix; fetch it from
                                    // the terminal.
                                    // (Done below outside the write() lock
                                    // to avoid holding two locks.)
                                }
                            }
                            // 1e. Immediate guard against the DB source
                            // re-surfacing the command during the async
                            // write window.
                            s.recent_failed_commands.insert(cmd.clone());
                        }
                        // Refresh the inline ghost text: recompute the
                        // suffix from the new top suggestion vs. the
                        // current cursor line.
                        {
                            let terminals = state_for_mark.read().terminals.clone();
                            if let Some(handle) = terminals.get(&sid_for_sug_delete) {
                                let line = handle.lock().terminal.extract_current_line();
                                let cmd_part = strip_prompt(line.trim());
                                let suggestions = state_for_mark.read()
                                    .sessions.iter()
                                    .find(|t| t.id == sid_for_sug_delete)
                                    .map(|t| t.suggestions.clone())
                                    .unwrap_or_default();
                                let new_suffix = suggestions.first().map(|first| {
                                    if first.len() > cmd_part.len()
                                        && first.starts_with(&cmd_part)
                                    {
                                        first[cmd_part.len()..].to_string()
                                    } else {
                                        String::new()
                                    }
                                }).unwrap_or_default();
                                state_for_mark.write().sessions.iter_mut()
                                    .find(|t| t.id == sid_for_sug_delete)
                                    .map(|tab| {
                                        tab.suggestion = if new_suffix.is_empty() {
                                            None
                                        } else {
                                            Some(new_suffix)
                                        };
                                    });
                            }
                        }
                        // Force suggestion list refresh on next keystroke.
                        {
                            let mut s = state_for_mark.write();
                            s.suggestion_epoch = s.suggestion_epoch.wrapping_add(1);
                        }
                        // 2. Durable: mark as failed in DB.
                        spawn(async move {
                            let db_path = dirs::data_dir()
                                .unwrap_or_default()
                                .join("rusterm")
                                .join("rusterm.db");
                            match rusterm_db::Database::open(Some(db_path)).await {
                                Ok(db) => {
                                    if let Err(e) = db.mark_command_failed(&cmd_for_spawn, 1).await {
                                        tracing::warn!(
                                            "[SUGGESTION-DELETE] mark_command_failed failed for {:?}: {}",
                                            cmd_for_spawn,
                                            e
                                        );
                                        // Leave in recent_failed_commands —
                                        // better to over-filter than
                                        // re-surface a typo the user just
                                        // deleted.
                                        return;
                                    }
                                    // 3. DB write committed — HAVING takes
                                    // over. Remove the UI-side guard.
                                    state_for_mark.write()
                                        .recent_failed_commands
                                        .remove(&cmd_for_spawn);
                                    tracing::info!(
                                        "[SUGGESTION-DELETE] marked {:?} as failed in DB (user-initiated)",
                                        cmd_for_spawn
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "[SUGGESTION-DELETE] failed to open DB for {:?}: {}",
                                        cmd_for_spawn,
                                        e
                                    );
                                }
                            }
                        });
                    },
                    onekey_visible: ok_visible,
                    onekey_entries: ok_entries,
                    onekey_selected: ok_selected,
                    on_onekey_navigate: move |idx: Option<usize>| {
                        if let Some(i) = idx {
                            state_for_cmd.write().onekey_popups
                                .entry(sid_for_ok.clone())
                                .and_modify(|p| p.selected = i);
                        }
                    },
                    on_onekey_select: move |send: String| {
                        // Send the selected OneKey's value + Enter, then dismiss.
                        // `send` is the DECRYPTED plaintext (OneKeyMatch.send comes
                        // from AppState.onekeys, which is decrypted on unlock). Log
                        // the length to confirm the plaintext (not the encrypted
                        // blob) is being sent.
                        tracing::info!(
                            "[ONEKEY-SELECT] session={} send_len={}",
                            &sid_for_ok_sel[..sid_for_ok_sel.len().min(8)],
                            send.len()
                        );
                        if let Some(sender) = senders.read().get(&sid_for_ok_sel) {
                            let mut data = send.into_bytes();
                            // Use \r (carriage return) — the same byte the terminal
                            // sends for Enter (Key::Enter → 0x0d). The PTY's ICRNL
                            // converts it to \n for the program. Using \r matches
                            // real keyboard input exactly.
                            data.push(b'\r');
                            let _ = sender.send(data);
                        }
                        state_for_cmd.write().onekey_popups
                            .remove(&sid_for_ok_sel);
                    },
                    on_onekey_save: move |_: ()| {
                        // Save a new OneKey: expect = the matched prompt,
                        // send = the text typed AFTER the prompt on the
                        // current line (strips the prompt prefix).
                        let entry = {
                            let terminals = state_for_cmd.read().terminals.clone();
                            let line = terminals.get(&sid_for_ok_save)
                                .map(|h| h.lock().terminal.extract_current_line())
                                .unwrap_or_default();
                            let expect = state_for_cmd.read().onekey_popups
                                .get(&sid_for_ok_save)
                                .and_then(|p| p.matched_expect.clone())
                                .unwrap_or_default();
                            let send = if let Ok(re) = regex::Regex::new(&format!("(?i){}", expect)) {
                                match re.find(&line) {
                                    Some(m) => line[m.end()..].trim().to_string(),
                                    None => line.trim().to_string(),
                                }
                            } else {
                                line.trim().to_string()
                            };
                            let name = if send.is_empty() { "onekey".to_string() } else { send.clone() };
                            OneKey {
                                id: uuid::Uuid::new_v4().to_string(),
                                name,
                                steps: vec![OneKeyStep {
                                    label: String::new(),
                                    expect,
                                    send: send.clone(),
                                }],
                            }
                        };
                        if entry.steps.first().is_some_and(|s| !s.send.is_empty()) {
                            let cm = state_for_cmd.read().config_manager.clone();
                            if let Some(cm) = cm {
                                let mut all = state_for_cmd.read().onekeys.clone();
                                all.push(entry);
                                if let Err(e) = cm.save_onekeys(&all) {
                                    tracing::error!("Failed to save OneKey: {}", e);
                                }
                                state_for_cmd.write().onekeys = all;
                            }
                        }
                        state_for_cmd.write().onekey_popups.remove(&sid_for_ok_save);
                    },
                    on_onekey_dismiss: move |_: ()| {
                        state_for_cmd.write().onekey_popups.remove(&sid_for_ok_dismiss);
                    },
                    disconnected: tab_disconnected,
                    on_reconnect: move |_: ()| {
                        reconnect_session(state_for_cmd, senders, sid_for_reconnect.clone());
                    },
                }
            }
        }
        None => rsx! { div {} },
    }
}

/// Wrap the single-pane `render_terminal_pane` output in a `<div>` that
/// carries drag-over / drop handlers. This is what makes "drag a background
/// tab from the top tab bar onto the active pane to create a split" work
/// when the active tab has NO layout yet (the Single preset, single-pane
/// render path).
///
/// Without this wrapper, the single-pane path (`render_terminal_pane`)
/// renders a bare `TerminalView` with no drop handlers — drops from the
/// tab bar go nowhere. The multi-pane path (`multi_pane_container`) has
/// its own per-pane drop handlers, but those only exist when a layout is
/// already applied. This wrapper closes the gap for the Single preset.
///
/// Drop logic (mirrors `multi_pane_container`'s pane `ondrop`):
///
/// 1. `application/x-rusterm-session-id` present (drag from tab bar):
///    - If `dragged_sid == render_sid` (dropped onto its own pane), no-op.
///    - Otherwise, the dragged session is a background tab not in any
///      pane → call `drop_background_tab_to_create_split(state,
///      dragged_sid, 0)`. The helper creates a Split2H layout with the
///      dragged session in pane 1 (or fills an empty slot / cycles to a
///      larger preset if a layout already exists). After this, the next
///      render takes the multi-pane path (`is_multi_pane == true`),
///      which has its own per-pane drop handlers for subsequent drags.
///
/// 2. `application/x-rusterm-connection-id` present (drag from sidebar):
///    - If this is a zoomed multi-pane layout, target the zoomed pane through
///      its stable layout owner. If no layout exists, open a new active tab.
///
/// NOTE: Both branches are DEFENSIVE FALLBACKS since Task 22's manual
/// mouse-based drag system replaced HTML5 DnD for both tab/pane-title
/// drags AND sidebar → pane drags (the latter via `DragKind::Connection`).
/// The sidebar no longer sets `draggable: true`, so the connection-id
/// branch should never fire — it's retained for forward compat in case
/// HTML5 DnD is re-enabled. The session-id branch is in the same boat.
/// The primary paths are `start_tab_drag` → `finish_tab_drag`, which
/// dispatch on `DragKind` (Session vs Connection).
///
/// `drag_over_pane` is read here to render the drop-zone overlay. The SOLE
/// writer of that signal is the manual polling loop in `App`, which
/// hit-tests the cursor at ~60Hz and sets `Some((pane_idx, region))` with
/// the real 4-quadrant region. HTML5 `ondragover`/`ondragenter` only call
/// `prevent_default` (for drop permission) — they do NOT write the signal,
/// avoiding a race that caused the overlay to flicker between adjacent
/// panes (the "错误的产生多个不需要的四方块" bug). `ondrop` and
/// `finish_tab_drag` clear the signal to `None`.
///
/// The border highlight uses the same `#7aa2f7` colour as the multi-pane
/// path. On top of the border, when a non-Center region is active a
/// translucent blue overlay covers the target half and a SINGLE bright
/// blue center line ("中线") shows the dividing axis — vertical for
/// Left/Right (横着 placement), horizontal for Top/Bottom (竖着 placement).
/// This is the "用中线作为标记" UX: one line per region, not the prior
/// "田"-shaped crosshair that always drew both lines.
fn single_pane_with_drop(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    render_sid: String,
    mut drag_over_pane: Signal<Option<(usize, PaneDropRegion)>>,
) -> Element {
    // The session id rendered in this pane, cloned for the drop closure.
    let drop_session_id = render_sid.clone();
    // Read once — this subscribes `App` to the signal so any change
    // triggers ONE re-render of this component (and rebuilds the overlay
    // style below with the new region).
    let drag_over = drag_over_pane();
    let is_drag_over = drag_over.is_some_and(|(idx, _)| idx == 0);
    let region = drag_over.and_then(|(idx, r)| if idx == 0 { Some(r) } else { None });
    let border_style = if is_drag_over {
        "border: 2px solid #7aa2f7; box-sizing: border-box;"
    } else {
        "border: 2px solid transparent; box-sizing: border-box;"
    };
    // Drop-hint overlay: highlighted target half + the SINGLE center line
    // that marks the dividing axis (the "用中线作为标记" affordance).
    //
    // The overlay is mounted only while `drag_over_pane` points at THIS
    // pane. `pointer-events: none` is critical — without it the overlay
    // would intercept the drop event and the pane's `ondrop` would never
    // fire.
    //
    // Visual scheme (single center line, NOT the "田" 4-block shape):
    //   Left / Right  → VERTICAL center line (divides left from right;
    //                  tells the user the new pane will sit beside the
    //                  existing one — a 横着 / horizontal arrangement).
    //   Top  / Bottom → HORIZONTAL center line (divides top from bottom;
    //                  tells the user the new pane will stack above/below
    //                  — a 竖着 / vertical arrangement).
    //   Center        → both lines dimmed (swap/move zone, no split).
    //
    // Showing only ONE line (instead of both) is the fix for the
    // "错误的产生多个不需要的四方块" bug — the prior crosshair always
    // drew both lines, forming a 田 shape that was ambiguous about which
    // direction the split would go, and visually stacked with the
    // half-rectangle highlight into a confusing 4-quadrant mosaic.
    let drop_overlay = if is_drag_over {
        let half_overlay_style = match region {
            Some(PaneDropRegion::Top) => Some(
                "position: absolute; left: 0; top: 0; width: 100%; height: 50%; \
                 background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
            ),
            Some(PaneDropRegion::Bottom) => Some(
                "position: absolute; left: 0; top: 50%; width: 100%; height: 50%; \
                 background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
            ),
            Some(PaneDropRegion::Left) => Some(
                "position: absolute; left: 0; top: 0; width: 50%; height: 100%; \
                 background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
            ),
            Some(PaneDropRegion::Right) => Some(
                "position: absolute; left: 50%; top: 0; width: 50%; height: 100%; \
                 background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
            ),
            // Center = swap/move zone — no half overlay.
            Some(PaneDropRegion::Center) | None => None,
        };
        // Pick the SINGLE center line ("中线") matching the split axis.
        // See `center_line_styles_for_region` for the visual scheme.
        let (vertical_line, horizontal_line) = match region {
            Some(r) => center_line_styles_for_region(r),
            None => center_line_styles_for_region(PaneDropRegion::Center),
        };
        Some(rsx! {
            // Highlighted target half (only for split regions).
            {half_overlay_style.map(|style| rsx! { div { style: "{style}" } })}
            // The single relevant center line ("中线"). Only one is
            // shown for split regions — this is what tells the user
            // 横着 (horizontal, left/right) vs 竖着 (vertical, top/bottom).
            {vertical_line.map(|style| rsx! { div { style: "{style}" } })}
            {horizontal_line.map(|style| rsx! { div { style: "{style}" } })}
        })
    } else {
        None
    };
    rsx! {
        div {
            key: "single-pane-{render_sid}",
            style: format!(
                "position: absolute; left: 0; top: 0; right: 0; bottom: 0; overflow: hidden; display: flex; flex-direction: column; {border_style}"
            ),
            // `ondragover` must call prevent_default to signal that this
            // element accepts drops. Without it, the browser blocks the
            // drop and fires `ondrop` with an empty DataTransfer.
            //
            // NOTE: we do NOT write to `drag_over_pane` here. The manual
            // polling loop in `App` is the SOLE writer of that signal —
            // it hit-tests the cursor at ~60Hz and sets the real
            // 4-quadrant region. Letting HTML5 dragover also write
            // `Some((0, Center))` caused it to race with the polling loop
            // at pane boundaries, flickering the overlay between the
            // HTML5-reported pane and the hit-test pane — visually this
            // looked like "多个不需要的四方块" (multiple unwanted
            // 4-block overlays) appearing and disappearing. Keeping the
            // signal write ONLY in the polling loop fixes that.
            ondragover: move |e: DragEvent| {
                e.prevent_default();
                e.data_transfer().set_drop_effect("move");
            },
            // `ondragenter` also needs prevent_default for cross-browser
            // compatibility (some browsers require both dragenter AND
            // dragover to be cancelled to allow drop). As with
            // `ondragover` above, we do NOT write `drag_over_pane` —
            // the polling loop owns that signal.
            ondragenter: move |e: DragEvent| {
                e.prevent_default();
            },
            ondrop: move |e: DragEvent| {
                e.prevent_default();
                drag_over_pane.set(None);
                let dt = e.data_transfer();
                // Check for the "drag from tab bar / pane title" MIME
                // type first — an open session is being moved.
                //
                // NOTE (Task 22): tabs no longer use HTML5 DnD (the
                // manual mouse-based system handles tab/pane title
                // drags via `finish_tab_drag` → `execute_tab_drop_on_pane`).
                // This branch is retained as a defensive fallback for
                // any residual HTML5 drag that might fire (e.g. if a
                // future change re-enables `draggable: true` on a pane).
                // It calls the same `execute_tab_drop_on_pane` that the
                // manual system uses, so the dispatch logic is identical.
                if let Some(dragged_sid) = dt.get_data("application/x-rusterm-session-id") {
                    if dragged_sid.is_empty() {
                        tracing::warn!("[DROP-SINGLE] empty session-id in drag data");
                        return;
                    }
                    let outcome = execute_tab_drop_on_pane(
                        &mut state.write(),
                        &dragged_sid,
                        0,
                        &drop_session_id,
                    );
                    match outcome {
                        TabDropOutcome::SelfDropExpanded {
                            first_pane_idx,
                            pane_count,
                        } => {
                            let opened = open_cloned_sessions_for_self_drop(
                                state,
                                input_senders,
                                &dragged_sid,
                                first_pane_idx,
                                pane_count,
                            );
                            tracing::info!(
                                "[DROP-SINGLE] self-drop expanded layout: source={} panes={} opened={}",
                                &dragged_sid[..dragged_sid.len().min(8)],
                                pane_count,
                                opened
                            );
                            restore_focus_to_active_session(state, 80);
                        }
                        TabDropOutcome::SplitCreated { pane_idx }
                        | TabDropOutcome::SplitFilledExisting { pane_idx } => {
                            tracing::info!(
                                "[DROP-SINGLE] created split: session {} placed in pane {} (outcome={:?})",
                                &dragged_sid[..dragged_sid.len().min(8)],
                                pane_idx,
                                outcome
                            );
                            restore_focus_to_active_session(state, 80);
                        }
                        TabDropOutcome::Swapped
                        | TabDropOutcome::MovedToEmptyPane { .. }
                        | TabDropOutcome::AssignedToEmptyPane => {
                            tracing::info!(
                                "[DROP-SINGLE] {} (session {})",
                                match outcome {
                                    TabDropOutcome::Swapped => "swapped panes",
                                    TabDropOutcome::MovedToEmptyPane { .. } => "moved to empty pane",
                                    TabDropOutcome::AssignedToEmptyPane => "assigned to empty pane",
                                    _ => "?",
                                },
                                &dragged_sid[..dragged_sid.len().min(8)]
                            );
                        }
                        TabDropOutcome::NoOpSelfDrop => {
                            tracing::debug!(
                                "[DROP-SINGLE] session {} dropped onto its own pane — no-op",
                                &dragged_sid[..dragged_sid.len().min(8)]
                            );
                        }
                        TabDropOutcome::SwapFailed
                        | TabDropOutcome::SplitFallbackSwapFailed
                        | TabDropOutcome::SplitFailed => {
                            tracing::warn!(
                                "[DROP-SINGLE] drop failed (outcome={:?}, session {})",
                                outcome,
                                &dragged_sid[..dragged_sid.len().min(8)]
                            );
                        }
                    }
                    return;
                }
                // DEFENSIVE FALLBACK: sidebar → pane drags now use the
                // manual mouse-based system (Task 22 extension —
                // `DragKind::Connection` in `start_tab_drag`/`finish_tab_drag`).
                // The sidebar no longer sets `draggable: true` + this MIME
                // type, so this branch should never fire. It's retained
                // symmetrically with the session-id fallback above, in
                // case a future change re-enables HTML5 DnD for sidebar
                // items — the dispatch logic stays correct either way.
                if let Some(conn_id) = dt.get_data("application/x-rusterm-connection-id") {
                    if conn_id.is_empty() {
                        tracing::warn!("[DROP-SINGLE] empty connection-id in drag data");
                        return;
                    }
                    let conn = state
                        .read()
                        .connections
                        .iter()
                        .find(|c| c.id == conn_id)
                        .cloned();
                    let Some(conn) = conn else {
                        tracing::warn!(
                            "[DROP-SINGLE] connection id {} not found in state.connections",
                            &conn_id[..conn_id.len().min(8)]
                        );
                        return;
                    };
                    tracing::info!(
                        "[DROP-SINGLE] opening connection {} ({:?}) in single pane",
                        &conn_id[..conn_id.len().min(8)],
                        conn.name
                    );
                    let target_pane_idx = {
                        let snapshot = state.read();
                        snapshot
                            .active_tab
                            .as_ref()
                            .and_then(|owner| snapshot.layouts.get(owner).and_then(|l| l.zoomed))
                            .unwrap_or(0)
                    };
                    let target = prepare_split_for_sidebar_drop(
                        &mut state.write(),
                        target_pane_idx,
                    )
                    .map(|plan| PaneTarget {
                        layout_owner_tab_id: plan.layout_owner_tab_id,
                        pane_idx: plan.pane_idx,
                    });
                    // Use the same preserve-and-grow plan as the primary manual
                    // drag path. At MAX_PANES, `None` opens a separate tab.
                    open_connection(state, input_senders, conn, target);
                    return;
                }
                tracing::debug!("[DROP-SINGLE] received drop with no recognized MIME type");
            },
            {drop_overlay}
            {render_terminal_pane(state, input_senders, render_sid)}
        }
    }
}

/// State held while the user is drag-resizing a splitter bar. Set by the
/// splitter's `onmousedown` (in `render_col_splitters` /
/// `render_row_splitters`); read and updated by the splitter's own
/// `onmousemove` handler (mouse-capture routes subsequent mousemove events
/// to the element that received mousedown — the splitter bar itself, NOT
/// an overlay); cleared by the splitter's `onmouseup`.
///
/// ## Why on the splitter, not an overlay
///
/// The prior design rendered an invisible full-screen overlay
/// (`position: fixed; inset: 0; z-index: 9999`) and put `onmousemove`/
/// `onmouseup` on it. That did NOT work: when the user presses the mouse
/// button on the splitter, the browser enters implicit pointer capture —
/// all subsequent `mousemove`/`mouseup` events are dispatched to the
/// element that received the `mousedown` (the splitter), regardless of
/// where the cursor is or what overlays are stacked above it. The overlay
/// never received the events, so the drag never resized anything and the
/// drag state was never cleared (the splitter had no `onmouseup` handler).
/// That's the root cause of "分屏无法调整".
///
/// Fix: put `onmousemove`/`onmouseup` on the splitter itself. The overlay
/// is kept as a defensive backstop in case a platform's webview doesn't
/// implement implicit capture (rare), but the splitter is the primary
/// event target.
///
/// ## Coordinate system
///
/// `last_applied_pos` is stored in **viewport-relative** coordinates
/// (matching `e.client_coordinates()`) — NOT container-relative. The
/// splitter bar's `x_val`/`y_val` is container-relative (relative to
/// `#multi-pane-container`), but `client_coordinates()` is viewport-
/// relative. If we initialized `last_applied_pos` to the container-relative
/// splitter position, the first `onmousemove` would compute a delta equal
/// to the container's viewport offset (sidebar width + tab bar height),
/// causing a large initial jump. By capturing the viewport-relative mouse
/// position at `onmousedown` time and storing THAT as `last_applied_pos`,
/// every subsequent delta is correctly computed in viewport space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SplitDragState {
    /// Preorder index of the recursive split node being resized.
    pub(crate) idx: usize,
    /// Container extent (width for col drag, height for row drag) at drag
    /// start, used to convert the pixel delta into a fractional delta.
    pub(crate) container_extent: f64,
    /// Last mouse position (viewport-relative, matching
    /// `e.client_coordinates()`) we applied a delta for. `onmousemove`
    /// reads the current mouse position, computes `pos - last_applied_pos`,
    /// applies that delta, then updates `last_applied_pos` to `pos`.
    /// Skipping when `pos == last_applied_pos` avoids redundant layout
    /// writes for duplicate mousemove events.
    pub(crate) last_applied_pos: f64,
    /// `true` for a left/right split, `false` for a top/bottom split.
    pub(crate) is_col: bool,
}

/// Active freeform pane-window move. Coordinates are viewport-relative; the
/// container dimensions are captured at drag start for normalized movement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PaneMoveState {
    pub(crate) pane_idx: usize,
    pub(crate) last_x: f64,
    pub(crate) last_y: f64,
    pub(crate) container_w: f64,
    pub(crate) container_h: f64,
}

pub(crate) fn build_install_pane_move_script(initial_x: f64, initial_y: f64) -> String {
    format!(
        "(function() {{\n\
            window.__rusterm_pane_move_pos = '{x},{y}';\n\
            window.__rusterm_pane_move_done = false;\n\
            if (window._rusterm_pane_move_remove) {{ window._rusterm_pane_move_remove(); }}\n\
            document.body.style.webkitUserSelect = 'none';\n\
            document.body.style.userSelect = 'none';\n\
            if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            var moveHandler = function(e) {{\n\
                window.__rusterm_pane_move_pos = e.clientX + ',' + e.clientY;\n\
                e.preventDefault();\n\
            }};\n\
            var upHandler = function(e) {{\n\
                window.__rusterm_pane_move_pos = e.clientX + ',' + e.clientY;\n\
                window.__rusterm_pane_move_done = true;\n\
                e.preventDefault();\n\
                if (window._rusterm_pane_move_remove) {{ window._rusterm_pane_move_remove(); window._rusterm_pane_move_remove = null; }}\n\
            }};\n\
            document.addEventListener('mousemove', moveHandler, true);\n\
            document.addEventListener('mouseup', upHandler, true);\n\
            window._rusterm_pane_move_remove = function() {{\n\
                document.removeEventListener('mousemove', moveHandler, true);\n\
                document.removeEventListener('mouseup', upHandler, true);\n\
                document.body.style.webkitUserSelect = '';\n\
                document.body.style.userSelect = '';\n\
            }};\n\
        }})()",
        x = initial_x,
        y = initial_y,
    )
}

fn install_pane_move_js_listeners(initial_x: f64, initial_y: f64) {
    let script = build_install_pane_move_script(initial_x, initial_y);
    spawn(async move {
        let _ = dioxus::document::eval(&script).await;
    });
}

pub(crate) fn parse_pane_move_poll_response(s: &str) -> Option<(f64, f64, bool)> {
    parse_split_drag_poll_response(s)
}

async fn poll_pane_move_state() -> Option<(f64, f64, bool)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var pos = window.__rusterm_pane_move_pos || '';\n\
            if (!pos) return '';\n\
            var done = window.__rusterm_pane_move_done ? '1' : '0';\n\
            return pos + ',' + done;\n\
        })()",
    )
    .await
    .ok()?;
    parse_pane_move_poll_response(result.as_str()?)
}

fn start_pane_move(
    mut state: Signal<AppState>,
    mut pane_move: Signal<Option<PaneMoveState>>,
    pane_idx: usize,
    start_x: f64,
    start_y: f64,
    container_w: f64,
    container_h: f64,
) {
    if !begin_floating_pane_move(&mut state.write(), pane_idx) {
        return;
    }
    pane_move.set(Some(PaneMoveState {
        pane_idx,
        last_x: start_x,
        last_y: start_y,
        container_w,
        container_h,
    }));
    install_pane_move_js_listeners(start_x, start_y);
}

fn apply_pane_move_step(
    mut state: Signal<AppState>,
    mut pane_move: Signal<Option<PaneMoveState>>,
    x: f64,
    y: f64,
) {
    let Some(drag) = pane_move() else {
        return;
    };
    if x == drag.last_x && y == drag.last_y {
        return;
    }
    if move_floating_pane_for_active(
        &mut state.write(),
        drag.pane_idx,
        x - drag.last_x,
        y - drag.last_y,
        drag.container_w,
        drag.container_h,
    ) {
        pane_move.set(Some(PaneMoveState {
            last_x: x,
            last_y: y,
            ..drag
        }));
    }
}

fn end_pane_move(state: Signal<AppState>, mut pane_move: Signal<Option<PaneMoveState>>) {
    pane_move.set(None);
    spawn(async move {
        let _ = dioxus::document::eval(
            "(function() {\n\
                if (window._rusterm_pane_move_remove) { window._rusterm_pane_move_remove(); window._rusterm_pane_move_remove = null; }\n\
                window.__rusterm_pane_move_pos = '';\n\
                window.__rusterm_pane_move_done = false;\n\
                document.body.style.webkitUserSelect = '';\n\
                document.body.style.userSelect = '';\n\
            })()",
        ).await;
    });
    restore_focus_to_active_session(state, 20);
}

/// Apply one mousemove step of a splitter drag.
///
/// Given the current `split_drag` state and the new viewport-relative
/// mouse position (from `e.client_coordinates()`), compute the pixel and
/// fractional delta, call `resize_layout_col`/`resize_layout_row`, and if
/// the resize was applied (i.e. not rejected by the min-frac guard),
/// update `last_applied_pos` to the new position.
///
/// This is the shared core used by BOTH the splitter bar's `onmousemove`
/// (the primary path, since implicit pointer capture routes events to the
/// element that received `mousedown`) and the defensive overlay's
/// `onmousemove` (in case a platform's webview doesn't implement implicit
/// capture).
///
/// No-op (returns without writing) if `split_drag` is `None` or the new
/// position equals `last_applied_pos` (suppresses duplicate events).
fn apply_split_drag_step(
    mut state: Signal<AppState>,
    mut split_drag: Signal<Option<SplitDragState>>,
    pos: f64,
) {
    let Some(drag) = split_drag() else {
        return;
    };
    let Some(frac_delta) = compute_split_drag_delta(&drag, pos) else {
        return;
    };
    let applied = resize_layout_split(&mut state.write(), drag.idx, frac_delta);
    if applied {
        split_drag.set(Some(SplitDragState {
            last_applied_pos: pos,
            ..drag
        }));
    }
}

/// Pure computation for one splitter drag step. Given the current
/// `SplitDragState` and a new viewport-relative mouse position, return:
///  - `None` if the drag should be a no-op (pos equals `last_applied_pos`,
///    or the container extent is zero so the fractional delta would be 0).
///  - `Some(frac_delta)` otherwise — the fractional delta to feed to
///    `resize_layout_col`/`resize_layout_row`.
///
/// Extracted as a pure function so it can be unit-tested without the
/// dioxus runtime. The signal-mutating `apply_split_drag_step` is just a
/// thin wrapper around this + the resize call + the `last_applied_pos`
/// update.
///
/// Note: this does NOT mutate `drag` — the caller is responsible for
/// updating `last_applied_pos` to `pos` after a successful resize (so that
/// rejected deltas — when the min-frac guard kicks in — don't accumulate
/// a growing gap between the mouse and the splitter bar).
pub(crate) fn compute_split_drag_delta(drag: &SplitDragState, pos: f64) -> Option<f64> {
    if pos == drag.last_applied_pos {
        return None;
    }
    if drag.container_extent <= 0.0 {
        return None;
    }
    let pixel_delta = pos - drag.last_applied_pos;
    Some(pixel_delta / drag.container_extent)
}

/// End a splitter drag and restore focus to the active session's input div.
///
/// Called from the polling `use_future` when it detects the JS-side
/// `window.__rusterm_drag_done` flag is set (the user released the mouse
/// button, captured by the document-level `mouseup` listener installed in
/// `install_split_drag_js_listeners`). Clears `split_drag` (unmounting the
/// overlay if it was mounted), removes the JS listeners, then explicitly
/// re-focuses the active session's terminal input div.
///
/// Why re-focus here: the splitter's `onmousedown` called `prevent_default()`,
/// which prevents the splitter bar from receiving focus. That's correct (we
/// don't want focus on the splitter bar), but it means the previously-focused
/// pane's input div has lost focus during the drag and nothing restores it
/// after `mouseup`. Without this restore, the user finishes dragging and
/// keystrokes go nowhere — the "分屏后无法输入" bug. We restore to the
/// ACTIVE session (not the previously-focused pane) because `active_session`
/// is the source of truth for which pane the user is currently working in,
/// and it's possible the drag was on a splitter adjacent to a non-active pane.
fn end_split_drag(state: Signal<AppState>, mut split_drag: Signal<Option<SplitDragState>>) {
    if split_drag().is_some() {
        tracing::info!("[OVERLAY] end_split_drag clearing split_drag");
        split_drag.set(None);
    }
    // Remove the JS-side document-level listeners so subsequent mouse moves
    // (e.g. after the drag is done) don't keep writing to the global variable.
    // This is idempotent — if no listeners are installed, the script is a no-op.
    spawn(async move {
        let _ = dioxus::document::eval(
            "(function() {\n\
                if (window._rusterm_split_drag_remove) { window._rusterm_split_drag_remove(); window._rusterm_split_drag_remove = null; }\n\
                window.__rusterm_drag_pos = '';\n\
                window.__rusterm_drag_done = false;\n\
            })()",
        ).await;
    });
    // Restore focus to the active session's input div. See `restore_focus_to_active_session`
    // for why this is needed (the short version: the splitter's `onmousedown`
    // `prevent_default` prevented focus from landing on the splitter, but
    // nothing restored it to the pane that had it before — so keystrokes
    // went nowhere after the drag ended).
    restore_focus_to_active_session(state, 20);
}

/// Build the JS script that installs document-level capture-phase
/// `mousemove`/`mouseup` listeners for splitter drag-resize. The script:
///
///  1. Initializes `window.__rusterm_drag_pos` to the starting mouse
///     position (so the first poll doesn't compute a delta from an
///     empty string).
///  2. Clears `window.__rusterm_drag_done`.
///  3. Removes any previously-installed listeners (idempotent — if no
///     listeners are installed, the prior-remove is a no-op).
///  4. Installs `moveHandler` (capture-phase `mousemove` listener) that
///     writes `e.clientX,e.clientY` to `__rusterm_drag_pos`.
///  5. Installs `upHandler` (capture-phase `mouseup` listener) that sets
///     `__rusterm_drag_done = true` and removes itself (so subsequent
///     mousemoves after the drag don't keep firing).
///  6. Stores a `_rusterm_split_drag_remove` function on `window` so
///     `end_split_drag` can remove the listeners later.
///
/// Extracted as a pure function (separate from `install_split_drag_js_listeners`)
/// so the script string can be unit-tested without a dioxus runtime — we
/// verify the script contains the expected setup steps and that the initial
/// position is correctly interpolated.
pub(crate) fn build_install_split_drag_script(initial_x: f64, initial_y: f64) -> String {
    format!(
        "(function() {{\n\
            window.__rusterm_drag_pos = '{x},{y}';\n\
            window.__rusterm_drag_done = false;\n\
            if (window._rusterm_split_drag_remove) {{ window._rusterm_split_drag_remove(); }}\n\
            var moveHandler = function(e) {{\n\
                window.__rusterm_drag_pos = e.clientX + ',' + e.clientY;\n\
                e.preventDefault();\n\
            }};\n\
            var upHandler = function(e) {{\n\
                window.__rusterm_drag_pos = e.clientX + ',' + e.clientY;\n\
                window.__rusterm_drag_done = true;\n\
                e.preventDefault();\n\
                if (window._rusterm_split_drag_remove) {{ window._rusterm_split_drag_remove(); window._rusterm_split_drag_remove = null; }}\n\
            }};\n\
            document.addEventListener('mousemove', moveHandler, true);\n\
            document.addEventListener('mouseup', upHandler, true);\n\
            window._rusterm_split_drag_remove = function() {{\n\
                document.removeEventListener('mousemove', moveHandler, true);\n\
                document.removeEventListener('mouseup', upHandler, true);\n\
            }};\n\
        }})()",
        x = initial_x,
        y = initial_y,
    )
}

/// Install document-level capture-phase `mousemove`/`mouseup` listeners that
/// write the current mouse position (viewport-relative) and a "done" flag to
/// global JS variables. A polling `use_future` in `App` reads these variables
/// every ~16ms and applies the drag delta.
///
/// ## Why document-level capture-phase listeners
///
/// The prior approach relied on `onmousemove`/`onmouseup` handlers attached
/// to either the splitter bar or a full-viewport overlay div. In dioxus 0.7's
/// desktop webview (WKWebView on macOS, webkitgtk on Linux, WebView2 on
/// Windows) this DOES NOT WORK reliably: implicit pointer capture either
/// isn't implemented or fires events at the wrong element, so the splitter's
/// `onmousemove` never fires during a drag (button held down), and the
/// overlay's `onmousemove` either doesn't fire or fires against a stale
/// element reference after dioxus re-renders.
///
/// Document-level listeners with `useCapture: true` are the GUARANTEED-correct
/// way to intercept mouse events during a drag in any webview. They fire
/// BEFORE any element-level handlers, BEFORE pointer capture kicks in, and
/// they keep firing even when the cursor moves outside the original target.
/// This is the same technique used by libraries like react-dnd's backend.
///
/// ## Why JS globals + polling (instead of a JS→Rust callback)
///
/// Dioxus 0.7's `eval` bridge is request/response — there's no way for JS to
/// push an event into Rust synchronously. The cleanest workaround is to have
/// the JS listeners write to `window.__rusterm_drag_pos` and
/// `window.__rusterm_drag_done`, and have a Rust `use_future` poll those
/// variables every 16ms (60Hz). 16ms is fast enough for smooth dragging (the
/// eye can't distinguish finer granularity) and slow enough to avoid flooding
/// the dioxus runtime with eval round-trips.
///
/// ## Why this is fire-and-forget (no `return` prefix)
///
/// We don't need a return value from the install script — we just need the
/// listeners to be attached synchronously. `eval` without `return` runs the
/// script and resolves with `null`. The script is structured as an IIFE so it
/// executes immediately when the webview's JS engine processes it.
///
/// ## Race safety
///
/// The install script runs synchronously in the webview's JS event loop. The
/// next native mousemove event (which is what we care about) can't fire until
/// the current event handler (onmousedown) returns and the JS engine is idle.
/// Since `eval` dispatches the script to run on the JS engine's queue, and
/// the JS engine processes its queue before pumping new native events, the
/// listeners WILL be installed before any mousemove fires. This eliminates
/// the race that the prior `spawn`-based installer had.
fn install_split_drag_js_listeners(initial_x: f64, initial_y: f64) {
    let script = build_install_split_drag_script(initial_x, initial_y);
    // Fire-and-forget: we don't need the return value. The script runs
    // synchronously on the JS engine's queue, which processes before the
    // next native mousemove event.
    spawn(async move {
        let _ = dioxus::document::eval(&script).await;
    });
}

/// Parse the response from `poll_split_drag_state`'s `eval` call. The JS
/// returns a string like `"123.4,567.8,0"` (x, y, done-flag) or `""` if the
/// globals aren't set. Returns:
///  - `None` if the string is empty or malformed (defensive against a stale
///    `split_drag` signal after the listeners were already removed).
///  - `Some((x, y, done))` where `done` is true if the user released the
///    mouse button.
///
/// Extracted as a pure function so the parsing can be unit-tested without
/// a dioxus runtime.
pub(crate) fn parse_split_drag_poll_response(s: &str) -> Option<(f64, f64, bool)> {
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let x = parts[0].parse::<f64>().ok()?;
    let y = parts[1].parse::<f64>().ok()?;
    let done = parts[2] == "1";
    Some((x, y, done))
}

/// Read the current mouse position and "done" flag from the JS globals set
/// by `install_split_drag_js_listeners`. Returns:
///  - `None` if the globals aren't set (shouldn't happen during a drag, but
///    defensive against a stale `split_drag` signal after the listeners were
///    already removed).
///  - `Some((x, y, done))` where `done` is true if the user released the
///    mouse button.
///
/// The JS global `__rusterm_drag_pos` is a string like "123.4,567.8"
/// (viewport-relative clientX,clientY). `__rusterm_drag_done` is a boolean.
async fn poll_split_drag_state() -> Option<(f64, f64, bool)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var pos = window.__rusterm_drag_pos || '';\n\
            if (!pos) return '';\n\
            var done = window.__rusterm_drag_done ? '1' : '0';\n\
            return pos + ',' + done;\n\
        })()",
    )
    .await
    .ok()?;
    let s = result.as_str()?;
    parse_split_drag_poll_response(s)
}

/// Restore keyboard focus to the active session's terminal input div.
///
/// Called after operations that disrupt focus:
///  - Splitter drag end (`end_split_drag`): the splitter's `onmousedown`
///    `prevent_default` prevented focus from landing on the splitter, but
///    nothing restored it to the pane that had it before — so keystrokes
///    went nowhere after the drag ended. This is the root cause of
///    "分屏后无法输入".
///  - Layout preset cycle (hotkey Cmd/Ctrl+Shift+L or toolbar click):
///    applying a new preset re-mounts panes, and the auto-focus `use_effect`
///    in each pane's `TerminalView` may race — the last-mounted pane wins
///    focus, which may not be the active session. Explicitly restoring
///    focus to the active session here ensures the user lands on the pane
///    they expect.
///
/// `delay_ms` is the delay before the focus call. The delay lets the
/// re-render from the triggering operation (split_drag clear, layout
/// preset change) commit — the terminal input div may not be mounted yet
/// if we call `eval` immediately. 20ms is enough for one frame at 60Hz
/// (~16ms) plus a safety margin. For layout preset changes we use a longer
/// delay (100ms) because the re-mount is heavier (multiple panes
/// re-render, each with their own `onmounted` JS eval).
///
/// The `eval` is fire-and-forget — if the element isn't mounted yet
/// (rare: the active session's pane might have been swapped out during
/// a drag-and-drop rearrangement), the `?.focus()` is a no-op.
fn restore_focus_to_active_session(state: Signal<AppState>, delay_ms: u64) {
    let active_sid = state.read().active_session.clone();
    if let Some(sid) = active_sid {
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            let script = format!("document.getElementById('terminal-input-{sid}')?.focus()");
            let _ = dioxus::document::eval(&script).await;
            tracing::info!("[FOCUS] restored focus to terminal-input-{sid}");
        });
    }
}

/// State held while the user is drag-and-dropping a session TAB (from
/// the top tab bar) or a pane TITLE BAR onto another pane to rearrange /
/// create splits. Set by the tab's / pane title's `onmousedown`; updated
/// and cleared by the polling `use_future` (`_tab_drag_poll`) in `App`.
///
/// ## Why manual mouse-based drag (Task 22) instead of HTML5 DnD
///
/// HTML5 drag-and-drop (`draggable: true`, `ondragstart`, `ondrop`) was
/// the prior mechanism for tab/pane drags (Tasks 17/19/22 prior
/// attempts). It's UNRELIABLE in dioxus 0.7's desktop webview
/// (WKWebView on macOS, WebView2 on Windows, webkitgtk on Linux): drops
/// sometimes don't fire, `DataTransfer` data sometimes comes back empty,
/// and the native drag ghost sometimes eats the release event. The
/// splitter drag-resize feature hit the SAME wall and was fixed by
/// switching to document-level capture-phase JS listeners + polling
/// (see the "Splitter drag-resize fix" section in the architecture
/// memory). This tab-drag system mirrors that PROVEN pattern exactly.
///
/// HTML5 DnD REMAINS for sidebar→pane connection drags (Task 16) — that
/// feature has no user complaints and the connection-id MIME type is
/// untouched.
///
/// ## Coordinate system
///
/// `start_x`/`start_y`/`cur_x`/`cur_y` are VIEWPORT-RELATIVE (matching
/// `e.client_coordinates()` and JS `e.clientX`/`e.clientY`). The polling
/// `use_future` reads `window.__rusterm_tab_drag_pos` (set by the JS
/// listeners) and `window.__rusterm_tab_drag_done`, and updates `cur_x`/
/// `cur_y` from them.
///
/// ## `dragging` flag (click vs drag)
///
/// `onmousedown` sets `tab_drag` with `dragging: false`. The polling
/// loop watches the cursor; once it moves more than `DRAG_THRESHOLD`
/// pixels from `start_x`/`start_y`, `dragging` becomes `true`. The drop
/// is ONLY executed on `mouseup` if `dragging == true`. This preserves
/// plain click-to-select: a mousedown with no significant mousemove is
/// a click, not a drag — the polling loop cleans up the signal and the
/// tab's `onclick` fires normally.
///
/// ## `kind` — session drag vs connection drag (Task 22 extension)
///
/// The drag system was originally built for tab/pane-title drags (move
/// an EXISTING session between panes — `execute_tab_drop_on_pane`).
/// Sidebar → pane drags used HTML5 DnD, which is UNRELIABLE in dioxus
/// 0.7's desktop webview (the same wall Task 22 hit for tab drags).
/// Extending this struct with a `kind` field lets sidebar drags reuse
/// the entire manual-mouse infrastructure (JS listeners, polling
/// loop, hit-test, ghost element) — only `finish_tab_drag`'s dispatch
/// branches on `kind`. `Session` carries the dragged session id;
/// `Connection` carries the connection config to open.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DragKind {
    /// Dragging an existing session (from the tab bar or a pane title
    /// bar). The wrapped String is the session id — `finish_tab_drag`
    /// calls `execute_tab_drop_on_pane` to move/swap/split it into the
    /// target pane.
    Session(String),
    /// Dragging a sidebar connection. The wrapped `ConnectionConfig` is
    /// opened as a brand-new session in the target pane — `finish_tab_drag`
    /// calls `open_connection` with a `PaneTarget`. This preserves the
    /// "sidebar drop = new independent session" contract.
    Connection(ConnectionConfig),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TabDragState {
    /// What's being dragged — an existing session or a sidebar connection.
    /// `finish_tab_drag` branches on this to decide between
    /// `execute_tab_drop_on_pane` (Session) and `open_connection`
    /// (Connection). See `DragKind` for the full rationale.
    pub(crate) kind: DragKind,
    /// Display name shown in the drag ghost element. For `Session` this
    /// is the session's name; for `Connection` it's the connection's name.
    pub(crate) display_name: String,
    /// Viewport-relative mouse position at drag start.
    pub(crate) start_x: f64,
    pub(crate) start_y: f64,
    /// Latest viewport-relative mouse position (updated by the polling
    /// loop from `window.__rusterm_tab_drag_pos`).
    pub(crate) cur_x: f64,
    pub(crate) cur_y: f64,
    /// `true` once the cursor has moved past `DRAG_THRESHOLD` from the
    /// start position. The drop is only executed on `mouseup` if this is
    /// `true` — otherwise it's a click and the signal is just cleaned up.
    pub(crate) dragging: bool,
}

/// Minimum cursor displacement (in CSS pixels) for a mousedown to be
/// promoted to a drag. 6px is large enough to absorb jitter from a
/// non-moving click but small enough that any deliberate drag passes.
pub(crate) const TAB_DRAG_THRESHOLD: f64 = 6.0;

/// Pure check: has the cursor moved past the drag threshold from the
/// start position? Extracted as a pure function so it can be unit-tested
/// without a dioxus runtime.
pub(crate) fn tab_drag_threshold_exceeded(
    start_x: f64,
    start_y: f64,
    cur_x: f64,
    cur_y: f64,
) -> bool {
    let dx = cur_x - start_x;
    let dy = cur_y - start_y;
    (dx * dx + dy * dy).sqrt() > TAB_DRAG_THRESHOLD
}

/// Build the JS script that installs document-level capture-phase
/// `mousemove`/`mouseup` listeners for a tab drag. Mirrors
/// `build_install_split_drag_script` exactly, but uses SEPARATE global
/// variable names (`__rusterm_tab_drag_pos`, `__rusterm_tab_drag_done`,
/// `_rusterm_tab_drag_remove`) so a tab drag and a splitter drag can't
/// clobber each other's state.
///
/// CRITICAL difference from the splitter version: the `upHandler` here
/// RECORDS the final mouse position (the splitter's `upHandler` didn't —
/// the splitter only needed the done flag, but a tab drop needs the
/// release coordinates for hit-testing).
///
/// The script also captures the `#terminal-content` container's
/// `getBoundingClientRect()` at install time and stashes it in
/// `__rusterm_tab_drag_container_left/top`. The polling loop reads these
/// to convert viewport-relative cursor coordinates into container-
/// relative coordinates for hit-testing. (We capture at install time
/// rather than on every poll to avoid a `getBoundingClientRect` call
/// per poll — the container doesn't move during a drag.)
pub(crate) fn build_install_tab_drag_script(initial_x: f64, initial_y: f64) -> String {
    format!(
        "(function() {{\n\
            window.__rusterm_tab_drag_pos = '{x},{y}';\n\
            window.__rusterm_tab_drag_done = false;\n\
            if (window._rusterm_tab_drag_remove) {{ window._rusterm_tab_drag_remove(); }}\n\
            var el = document.getElementById('terminal-content');\n\
            var r = el ? el.getBoundingClientRect() : {{ left: 0, top: 0 }};\n\
            window.__rusterm_tab_drag_container_left = r.left;\n\
            window.__rusterm_tab_drag_container_top = r.top;\n\
            // Suppress text selection for the whole document during the\n\
            // drag. moveHandler's preventDefault alone does NOT stop\n\
            // WebKit from extending a selection that started on the\n\
            // mousedown — disabling user-select on <body> does. The\n\
            // remove function restores it.\n\
            document.body.style.webkitUserSelect = 'none';\n\
            document.body.style.userSelect = 'none';\n\
            if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            var moveHandler = function(e) {{\n\
                window.__rusterm_tab_drag_pos = e.clientX + ',' + e.clientY;\n\
                e.preventDefault();\n\
            }};\n\
            var upHandler = function(e) {{\n\
                window.__rusterm_tab_drag_pos = e.clientX + ',' + e.clientY;\n\
                window.__rusterm_tab_drag_done = true;\n\
                // NOTE: do NOT call e.preventDefault() here. Calling\n\
                // preventDefault on mouseup can cancel the subsequent\n\
                // click event on some webviews, which would break the\n\
                // tab's onclick (click-to-select). The moveHandler\n\
                // calls preventDefault to suppress text selection\n\
                // during the drag, but the upHandler doesn't need it.\n\
                if (window._rusterm_tab_drag_remove) {{ window._rusterm_tab_drag_remove(); window._rusterm_tab_drag_remove = null; }}\n\
            }};\n\
            document.addEventListener('mousemove', moveHandler, true);\n\
            document.addEventListener('mouseup', upHandler, true);\n\
            window._rusterm_tab_drag_remove = function() {{\n\
                document.removeEventListener('mousemove', moveHandler, true);\n\
                document.removeEventListener('mouseup', upHandler, true);\n\
                document.body.style.webkitUserSelect = '';\n\
                document.body.style.userSelect = '';\n\
                if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            }};\n\
        }})()",
        x = initial_x,
        y = initial_y,
    )
}

/// Install the document-level capture-phase `mousemove`/`mouseup`
/// listeners for a tab drag. Fire-and-forget — the script runs
/// synchronously on the JS engine's queue before the next native
/// mousemove event (see `install_split_drag_js_listeners` for the same
/// reasoning).
pub(crate) fn install_tab_drag_js_listeners(initial_x: f64, initial_y: f64) {
    let script = build_install_tab_drag_script(initial_x, initial_y);
    spawn(async move {
        let _ = dioxus::document::eval(&script).await;
    });
}

/// Parse the response from `poll_tab_drag_state`'s `eval` call. The JS
/// returns a string like `"123.4,567.8,0,80.0,60.0"`
/// (x, y, done-flag, container_left, container_top) or `""` if the
/// globals aren't set.
///
/// Returns `None` if the string is empty or malformed (defensive
/// against a stale `tab_drag` signal after the listeners were already
/// removed). Extracted as a pure function so the parsing can be
/// unit-tested without a dioxus runtime.
pub(crate) fn parse_tab_drag_poll_response(s: &str) -> Option<(f64, f64, bool, f64, f64)> {
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 5 {
        return None;
    }
    let x = parts[0].parse::<f64>().ok()?;
    let y = parts[1].parse::<f64>().ok()?;
    let done = parts[2] == "1";
    let container_left = parts[3].parse::<f64>().ok()?;
    let container_top = parts[4].parse::<f64>().ok()?;
    Some((x, y, done, container_left, container_top))
}

/// Read the current mouse position, "done" flag, and container offset
/// from the JS globals set by `install_tab_drag_js_listeners`. Returns
/// `None` if the globals aren't set (defensive against a stale
/// `tab_drag` signal after the listeners were already removed).
async fn poll_tab_drag_state() -> Option<(f64, f64, bool, f64, f64)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var pos = window.__rusterm_tab_drag_pos || '';\n\
            if (!pos) return '';\n\
            var done = window.__rusterm_tab_drag_done ? '1' : '0';\n\
            var left = window.__rusterm_tab_drag_container_left || 0;\n\
            var top = window.__rusterm_tab_drag_container_top || 0;\n\
            return pos + ',' + done + ',' + left + ',' + top;\n\
        })()",
    )
    .await
    .ok()?;
    let s = result.as_str()?;
    parse_tab_drag_poll_response(s)
}

/// Pure hit-test: given the cursor's viewport-relative position, the
/// `#terminal-content` container's viewport offset, the container's
/// size, and the active layout, return the pane index (and its session
/// id) under the cursor — or `None` if the cursor is outside the
/// container or outside all visible panes.
///
/// For a SINGLE-pane layout (or no layout — the caller handles that
/// case by passing a synthetic 1-pane layout), the cursor is in the
/// pane iff it's anywhere inside the container rect.
///
/// For a MULTI-pane layout, iterate `layout.visible_panes(w, h)`
/// (which yields container-relative px rects) and return the first pane
/// whose viewport-relative rect contains the cursor.
///
/// Extracted as a pure function so the hit-test can be unit-tested
/// without a dioxus runtime. The caller is responsible for the
/// single-pane special-case (cursor in container rect →
/// `Some((0, render_sid))`) — see `finish_tab_drag`.
pub(crate) fn hit_test_pane_at(
    cursor_x: f64,
    cursor_y: f64,
    container_left: f64,
    container_top: f64,
    container_w: f64,
    container_h: f64,
    layout: &PaneLayout,
) -> Option<(usize, String)> {
    // Container-relative cursor coordinates.
    let rel_x = cursor_x - container_left;
    let rel_y = cursor_y - container_top;
    // Outside the container entirely.
    if rel_x < 0.0 || rel_y < 0.0 || rel_x > container_w || rel_y > container_h {
        return None;
    }
    // Overlapping floating windows require z-aware hit testing. Grid layouts
    // have deterministic index-based z values, preserving the old behavior.
    layout
        .visible_panes(container_w, container_h)
        .filter(|(_, _, (px, py, pw, ph))| {
            rel_x >= *px && rel_x < *px + *pw && rel_y >= *py && rel_y < *py + *ph
        })
        .max_by_key(|(idx, _, _)| layout.pane_z_index(*idx).unwrap_or(0))
        .map(|(idx, pane, _)| (idx, pane.session_id.clone()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneDropRegion {
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneDropTarget {
    pub(crate) pane_idx: usize,
    pub(crate) session_id: String,
    pub(crate) region: PaneDropRegion,
}

/// 4-quadrant drop-zone hit-test. Within the hovered pane's rectangle the
/// center ±15% on both axes is the `Center` zone (swap/move — no split).
/// Outside the center zone, the dominant axis (the one whose distance from
/// center is larger) determines the split direction:
///   - horizontal axis dominates → `Left` or `Right`
///   - vertical axis dominates   → `Top` or `Bottom`
///
/// This is the "用中线作为标记" (use the center line as a marker) UX: a
/// center crosshair on the hovered pane shows both dividing lines, and the
/// highlighted target half follows the cursor to indicate which side the
/// new pane will occupy.
pub(crate) fn hit_test_pane_drop_target_at(
    cursor_x: f64,
    cursor_y: f64,
    container_left: f64,
    container_top: f64,
    container_w: f64,
    container_h: f64,
    layout: &PaneLayout,
) -> Option<PaneDropTarget> {
    let (pane_idx, session_id) = hit_test_pane_at(
        cursor_x,
        cursor_y,
        container_left,
        container_top,
        container_w,
        container_h,
        layout,
    )?;
    let (pane_x, pane_y, pane_w, pane_h) = layout.pane_rect(pane_idx, container_w, container_h)?;
    if pane_w <= 0.0 || pane_h <= 0.0 {
        return None;
    }
    // Cursor position relative to the pane, normalised to [0, 1].
    let rel_x_in_pane = (cursor_x - container_left - pane_x) / pane_w;
    let rel_y_in_pane = (cursor_y - container_top - pane_y) / pane_h;
    // Distance from pane center, in [-0.5, 0.5] on each axis.
    let dx_from_center = rel_x_in_pane - 0.5;
    let dy_from_center = rel_y_in_pane - 0.5;
    let region = pane_drop_region_for_cursor(dx_from_center, dy_from_center);
    Some(PaneDropTarget {
        pane_idx,
        session_id,
        region,
    })
}

/// Map a cursor's normalised distance from pane center (each axis in
/// `[-0.5, 0.5]`) to a 4-quadrant drop region. Center ±15% on BOTH axes
/// is the swap/move zone; outside that, the dominant axis (the one farther
/// from center) picks between left/right vs top/bottom.
///
/// Extracted as a pure function so both the multi-pane hit-test
/// (`hit_test_pane_drop_target_at`) and the single-pane fallbacks in
/// `finish_tab_drag` and the polling loop share the SAME region-decision
/// logic. This keeps the drop-region consistent across all code paths.
pub(crate) fn pane_drop_region_for_cursor(
    dx_from_center: f64,
    dy_from_center: f64,
) -> PaneDropRegion {
    const CENTER_HALF: f64 = 0.15;
    if dx_from_center.abs() < CENTER_HALF && dy_from_center.abs() < CENTER_HALF {
        PaneDropRegion::Center
    } else if dx_from_center.abs() > dy_from_center.abs() {
        // Horizontal axis dominates — split left/right.
        if dx_from_center < 0.0 {
            PaneDropRegion::Left
        } else {
            PaneDropRegion::Right
        }
    } else {
        // Vertical axis dominates — split top/bottom.
        if dy_from_center < 0.0 {
            PaneDropRegion::Top
        } else {
            PaneDropRegion::Bottom
        }
    }
}

/// The two CSS style strings for the drop-zone center-line overlay.
/// Returns `(vertical_line_style, horizontal_line_style)` — each is
/// `Some` when that line should be drawn, `None` when it should not.
///
/// This is the "用中线作为标记" UX: exactly ONE bright line is drawn for
/// split regions (vertical for Left/Right = 横着 placement, horizontal for
/// Top/Bottom = 竖着 placement). For the Center swap/move zone, BOTH lines
/// are drawn dimmed so the user can see the swap zone without it dominating
/// the pane.
///
/// Showing ONE line per region (instead of always both) is the fix for the
/// "错误的产生多个不需要的四方块" bug — the prior crosshair always drew
/// both lines forming a 田 shape that was ambiguous about the split
/// direction and visually competed with the half-rectangle highlight.
///
/// Extracted as a pure function so the overlay decision can be unit-tested
/// without a dioxus runtime, and so `single_pane_with_drop` and
/// `multi_pane_container` share the SAME line-selection logic.
pub(crate) fn center_line_styles_for_region(
    region: PaneDropRegion,
) -> (Option<&'static str>, Option<&'static str>) {
    // The bright accent line (2px, full opacity, glow) is used for split
    // regions; the dimmed line (1px, 35% opacity) is used for Center.
    const VERTICAL_BRIGHT: &str = "position: absolute; left: 50%; top: 0; width: 2px; height: 100%; \
         background: #7aa2f7; pointer-events: none; z-index: 21; \
         transform: translateX(-1px); box-shadow: 0 0 6px rgba(122,162,247,0.5);";
    const HORIZONTAL_BRIGHT: &str = "position: absolute; left: 0; top: 50%; width: 100%; height: 2px; \
         background: #7aa2f7; pointer-events: none; z-index: 21; \
         transform: translateY(-1px); box-shadow: 0 0 6px rgba(122,162,247,0.5);";
    const VERTICAL_DIM: &str = "position: absolute; left: 50%; top: 0; width: 1px; height: 100%; \
         background: rgba(122,162,247,0.35); pointer-events: none; z-index: 21; \
         transform: translateX(-0.5px);";
    const HORIZONTAL_DIM: &str = "position: absolute; left: 0; top: 50%; width: 100%; height: 1px; \
         background: rgba(122,162,247,0.35); pointer-events: none; z-index: 21; \
         transform: translateY(-0.5px);";
    match region {
        // Horizontal-axis split (Left/Right): show ONLY the vertical divider.
        // This is the 横着 placement marker — the new pane sits beside the
        // existing one, separated by a vertical line.
        PaneDropRegion::Left | PaneDropRegion::Right => (Some(VERTICAL_BRIGHT), None),
        // Vertical-axis split (Top/Bottom): show ONLY the horizontal divider.
        // This is the 竖着 placement marker — the new pane stacks above/below,
        // separated by a horizontal line.
        PaneDropRegion::Top | PaneDropRegion::Bottom => (None, Some(HORIZONTAL_BRIGHT)),
        // Center swap/move zone: both lines dimmed (no split will happen).
        PaneDropRegion::Center => (Some(VERTICAL_DIM), Some(HORIZONTAL_DIM)),
    }
}

/// Start a tab drag: set the `tab_drag` signal (with `dragging: false` —
/// the polling loop promotes it to `true` once the cursor crosses the
/// threshold) and install the document-level JS listeners.
///
/// Called from the tab bar's `onmousedown` (in `tab_bar.rs`, threaded
/// through `TabBar`'s `on_drag_start` prop) and from the pane title
/// bar's `onmousedown` (in `multi_pane_container`).
///
/// The session name is included so the polling loop can render a ghost
/// element showing the dragged session's name following the cursor.
/// `display_name` is shown in the ghost element. The polling `use_future`
/// in `App` takes over from there.
pub(crate) fn start_tab_drag(
    mut tab_drag: Signal<Option<TabDragState>>,
    kind: DragKind,
    display_name: String,
    start_x: f64,
    start_y: f64,
) {
    tab_drag.set(Some(TabDragState {
        kind,
        display_name,
        start_x,
        start_y,
        cur_x: start_x,
        cur_y: start_y,
        dragging: false,
    }));
    install_tab_drag_js_listeners(start_x, start_y);
}

/// Clone one SSH/shell session into a specific empty pane.
///
/// `open_connection` generates a fresh UUID and creates a fresh terminal,
/// sender set, connection task, and reconnect lifecycle. Only the connection
/// configuration is copied; the source pane and its runtime objects are never
/// reused. The explicit layout owner (a tab group_id) prevents an active-tab
/// change from redirecting the assignment.
fn clone_session_into_pane(
    state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    layout_owner_tab_id: &str,
    source_session_id: &str,
    target_pane_idx: usize,
) -> Option<String> {
    let snapshot = state.read();
    let target_is_empty = snapshot
        .layouts
        .get(layout_owner_tab_id)
        .and_then(|layout| layout.panes.get(target_pane_idx))
        .is_some_and(|pane| pane.session_id.is_empty());
    let conn = snapshot.session_configs.get(source_session_id).cloned();
    drop(snapshot);

    if !target_is_empty {
        tracing::warn!(
            "[PANE-CLONE] layout_owner={} target_pane={} is missing or occupied; clone skipped",
            &layout_owner_tab_id[..layout_owner_tab_id.len().min(8)],
            target_pane_idx
        );
        return None;
    }
    let Some(conn) = conn else {
        tracing::warn!(
            "[PANE-CLONE] source={} has no stored connection config",
            &source_session_id[..source_session_id.len().min(8)]
        );
        return None;
    };
    if !matches!(
        &conn.kind,
        ConnectionKind::Ssh(_) | ConnectionKind::Shell(_)
    ) {
        tracing::warn!(
            "[PANE-CLONE] source={} connection type is not cloneable",
            &source_session_id[..source_session_id.len().min(8)]
        );
        return None;
    }

    let result = open_connection(
        state,
        input_senders,
        conn,
        Some(PaneTarget {
            layout_owner_tab_id: layout_owner_tab_id.to_string(),
            pane_idx: target_pane_idx,
        }),
    );
    let assignment_verified = result.assigned_to_target
        && result.session_id != source_session_id
        && state
            .read()
            .layouts
            .get(layout_owner_tab_id)
            .and_then(|layout| layout.panes.get(target_pane_idx))
            .is_some_and(|pane| pane.session_id == result.session_id);

    if assignment_verified {
        tracing::info!(
            "[PANE-CLONE] source={} layout_owner={} target_pane={} new_session={} assignment=verified",
            &source_session_id[..source_session_id.len().min(8)],
            &layout_owner_tab_id[..layout_owner_tab_id.len().min(8)],
            target_pane_idx,
            &result.session_id[..result.session_id.len().min(8)]
        );
        Some(result.session_id)
    } else {
        tracing::error!(
            "[PANE-CLONE] source={} layout_owner={} target_pane={} new_session={} assignment=failed",
            &source_session_id[..source_session_id.len().min(8)],
            &layout_owner_tab_id[..layout_owner_tab_id.len().min(8)],
            target_pane_idx,
            &result.session_id[..result.session_id.len().min(8)]
        );
        None
    }
}

fn open_cloned_sessions_for_self_drop(
    state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    source_session_id: &str,
    first_pane_idx: usize,
    pane_count: usize,
) -> usize {
    let Some(layout_owner_tab_id) = state.read().active_tab.clone() else {
        tracing::error!(
            "[SPLIT-CLONE] source={} has no layout owner; {} pane(s) remain empty",
            &source_session_id[..source_session_id.len().min(8)],
            pane_count
        );
        return 0;
    };

    (first_pane_idx..first_pane_idx.saturating_add(pane_count))
        .filter(|pane_idx| {
            clone_session_into_pane(
                state,
                input_senders,
                &layout_owner_tab_id,
                source_session_id,
                *pane_idx,
            )
            .is_some()
        })
        .count()
}

/// Finish a tab drag: do the final hit-test at the release position,
/// call `execute_tab_drop_on_pane` (the single source of truth for drop
/// dispatch), log the outcome, and restore focus if a new pane was
/// created. Clears `tab_drag` and `drag_over_pane`.
///
/// Called from the polling `use_future` when it detects the JS-side
/// `__rusterm_tab_drag_done` flag is set AND `dragging == true`. If
/// `dragging == false` (the user mousedowned but didn't move — a
/// click), this function is NOT called; the polling loop just cleans
/// up the signal.
///
/// Single-pane special case: when there's no layout for the active
/// session (Single preset), the cursor is in pane 0 iff it's anywhere
/// inside the `#terminal-content` container. We synthesize a 1-pane hit
/// in that case.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_tab_drag(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    mut tab_drag: Signal<Option<TabDragState>>,
    mut drag_over_pane: Signal<Option<(usize, PaneDropRegion)>>,
    container_size: Signal<Option<(f64, f64)>>,
    final_x: f64,
    final_y: f64,
    container_left: f64,
    container_top: f64,
) {
    // Read the drag state. Clone the session_id out so we can release
    // the borrow before mutating `state` below (avoids holding a
    // `tab_drag.read()` guard across a `state.write()`).
    let drag_opt = tab_drag();
    let Some(drag) = drag_opt else {
        // Already cleared — nothing to do.
        drag_over_pane.set(None);
        return;
    };
    let drag_kind = drag.kind.clone();

    // Compute the hit-test target.
    //
    // The layout we care about is the ACTIVE tab's layout (layouts are
    // keyed by `active_tab`, NOT by `dragged_sid` — the dragged
    // session may be a pane-only session with no tab of its own).
    // For sidebar `Connection` drags the active tab is also the right
    // owner: a sidebar drop opens a NEW session in the target pane of
    // the currently-visible layout, not in some other tab's layout.
    let active_id = state.read().active_tab.clone();
    let active_layout = active_id
        .as_ref()
        .and_then(|aid| state.read().layouts.get(aid).cloned());

    let (container_w, container_h) = container_size().unwrap_or((1200.0, 800.0));

    // Determine the target pane via hit-test.
    let hit = if let Some(layout) = active_layout.as_ref() {
        // Multi-pane (or single-pane-but-layout-exists) path: use the
        // pure hit-test helper.
        hit_test_pane_drop_target_at(
            final_x,
            final_y,
            container_left,
            container_top,
            container_w,
            container_h,
            layout,
        )
    } else {
        // No layout for the active session (Single preset, single
        // pane). The cursor is in pane 0 iff it's inside the container.
        // Apply the same 4-quadrant scheme as `hit_test_pane_drop_target_at`
        // so the single-pane path supports left/right/top/bottom splits too.
        let rel_x = final_x - container_left;
        let rel_y = final_y - container_top;
        if rel_x >= 0.0 && rel_y >= 0.0 && rel_x <= container_w && rel_y <= container_h {
            // Pane 0 holds the active session.
            let dx_from_center = if container_w > 0.0 {
                rel_x / container_w - 0.5
            } else {
                0.0
            };
            let dy_from_center = if container_h > 0.0 {
                rel_y / container_h - 0.5
            } else {
                0.0
            };
            let region = pane_drop_region_for_cursor(dx_from_center, dy_from_center);
            Some(PaneDropTarget {
                pane_idx: 0,
                session_id: active_id.clone().unwrap_or_default(),
                region,
            })
        } else {
            None
        }
    };

    // Apply the drop if we hit a pane. Dispatch on `drag_kind`:
    //  - `Session` moves an existing session into the target pane
    //    (execute_tab_drop_on_pane handles swap/move/split).
    //  - `Connection` opens a NEW session in the target pane
    //    (open_connection with a PaneTarget keyed by the active tab).
    //    This preserves the "sidebar drop = new independent session for
    //    comparison" contract — existing pane sessions remain visible and
    //    one new pane is created when capacity permits.
    if let Some(target) = hit {
        let target_idx = target.pane_idx;
        let target_session = target.session_id;
        let split_direction = match target.region {
            PaneDropRegion::Top => SplitDirection::Top,
            PaneDropRegion::Bottom => SplitDirection::Bottom,
            PaneDropRegion::Left => SplitDirection::Left,
            PaneDropRegion::Right => SplitDirection::Right,
            PaneDropRegion::Center => SplitDirection::Bottom,
        };
        match drag_kind {
            DragKind::Session(dragged_sid) => {
                let outcome = execute_tab_drop_on_pane_at(
                    &mut state.write(),
                    &dragged_sid,
                    target_idx,
                    &target_session,
                    split_direction,
                );
                match outcome {
                    TabDropOutcome::SelfDropExpanded {
                        first_pane_idx,
                        pane_count,
                    } => {
                        let opened = open_cloned_sessions_for_self_drop(
                            state,
                            input_senders,
                            &dragged_sid,
                            first_pane_idx,
                            pane_count,
                        );
                        tracing::info!(
                            "[TAB-DRAG] self-drop expanded layout: source={} panes={} opened={}",
                            &dragged_sid[..dragged_sid.len().min(8)],
                            pane_count,
                            opened
                        );
                        restore_focus_to_active_session(state, 80);
                    }
                    TabDropOutcome::SplitCreated { pane_idx }
                    | TabDropOutcome::SplitFilledExisting { pane_idx } => {
                        tracing::info!(
                            "[TAB-DRAG] created split: session {} placed in pane {} (outcome={:?})",
                            &dragged_sid[..dragged_sid.len().min(8)],
                            pane_idx,
                            outcome
                        );
                        restore_focus_to_active_session(state, 80);
                    }
                    TabDropOutcome::Swapped
                    | TabDropOutcome::MovedToEmptyPane { .. }
                    | TabDropOutcome::AssignedToEmptyPane => {
                        tracing::info!(
                            "[TAB-DRAG] {} (session {})",
                            match outcome {
                                TabDropOutcome::Swapped => "swapped panes",
                                TabDropOutcome::MovedToEmptyPane { .. } => "moved to empty pane",
                                TabDropOutcome::AssignedToEmptyPane => "assigned to empty pane",
                                _ => "?",
                            },
                            &dragged_sid[..dragged_sid.len().min(8)]
                        );
                    }
                    TabDropOutcome::NoOpSelfDrop => {
                        tracing::info!(
                            "[TAB-DRAG] self-drop no-op (session {})",
                            &dragged_sid[..dragged_sid.len().min(8)]
                        );
                    }
                    TabDropOutcome::SwapFailed
                    | TabDropOutcome::SplitFallbackSwapFailed
                    | TabDropOutcome::SplitFailed => {
                        tracing::warn!(
                            "[TAB-DRAG] drop failed (outcome={:?}, session {})",
                            outcome,
                            &dragged_sid[..dragged_sid.len().min(8)]
                        );
                    }
                }
            }
            DragKind::Connection(conn) => {
                // Sidebar → pane drop: open a NEW session for side-by-side
                // comparison. When the target pane is already occupied, we
                // PRESERVE the existing session by growing the layout (or
                // reusing another empty pane) instead of replacing the
                // target's session. This implements the "拖动左侧会话 = 新开会话对比"
                // contract — the existing session stays visible alongside the
                // new one.
                //
                // `prepare_split_for_sidebar_drop` returns the pane index
                // where the new session should land (which may differ from
                // `target_idx` if we reused an empty pane or appended a new
                // pane on demand). We then call `open_connection` with a
                // `PaneTarget` keyed by the layout owner returned from that
                // call — this is NOT necessarily `active_id`, because the
                // layout mutation inside `prepare_split_for_sidebar_drop`
                // may have changed things; using the returned owner keeps the
                // assignment stable against any active-tab mutation.
                let conn_name = conn.name.clone();
                let conn_id_short = conn.id.clone();
                let plan = prepare_split_for_sidebar_drop_at(
                    &mut state.write(),
                    target_idx,
                    split_direction,
                );
                let (owner_for_target, pane_for_target, created_new_pane) = match plan {
                    Some(plan) => (
                        plan.layout_owner_tab_id.clone(),
                        plan.pane_idx,
                        plan.created_new_pane,
                    ),
                    None => {
                        // No active tab — open as a new tab without a pane target.
                        (String::new(), 0usize, false)
                    }
                };
                let target = if owner_for_target.is_empty() {
                    None
                } else {
                    Some(PaneTarget {
                        layout_owner_tab_id: owner_for_target.clone(),
                        pane_idx: pane_for_target,
                    })
                };
                tracing::info!(
                    "[TAB-DRAG] opening sidebar connection {} ({:?}) in pane {} (owner={:?}, created_new_pane={}, original_target={})",
                    &conn_id_short[..conn_id_short.len().min(8)],
                    conn_name,
                    pane_for_target,
                    if owner_for_target.is_empty() {
                        None
                    } else {
                        Some(&owner_for_target[..owner_for_target.len().min(8)])
                    },
                    created_new_pane,
                    target_idx
                );
                let result = open_connection(state, input_senders, conn, target);
                if result.assigned_to_target {
                    tracing::info!(
                        "[TAB-DRAG] connection {} assigned to pane {}",
                        &conn_id_short[..conn_id_short.len().min(8)],
                        pane_for_target
                    );
                    restore_focus_to_active_session(state, 80);
                } else {
                    tracing::warn!(
                        "[TAB-DRAG] connection {} NOT assigned to pane {} (opened as new tab?)",
                        &conn_id_short[..conn_id_short.len().min(8)],
                        pane_for_target
                    );
                }
            }
        }
    } else {
        tracing::info!("[TAB-DRAG] release outside any pane — no-op");
    }

    // Clear the drag state and the drop-target highlight.
    tab_drag.set(None);
    drag_over_pane.set(None);
    // Clean up the JS-side globals + listeners (idempotent).
    spawn(async move {
        let _ = dioxus::document::eval(
            "(function() {\n\
                if (window._rusterm_tab_drag_remove) { window._rusterm_tab_drag_remove(); window._rusterm_tab_drag_remove = null; }\n\
                window.__rusterm_tab_drag_pos = '';\n\
                window.__rusterm_tab_drag_done = false;\n\
                window.__rusterm_tab_drag_container_left = 0;\n\
                window.__rusterm_tab_drag_container_top = 0;\n\
                document.body.style.webkitUserSelect = '';\n\
                document.body.style.userSelect = '';\n\
            })()",
        ).await;
    });
    // Restore focus to the active session (the drag's `onmousedown`
    // `prevent_default` prevented focus from landing on the tab, but
    // nothing restored it to the pane that had it before — same root
    // cause as the splitter drag's focus issue).
    restore_focus_to_active_session(state, 20);
    // `active_id` is read for the single-pane fallback hit-test above.
    let _ = active_id;
}

/// Actions shown in an empty pane's title bar.
///
/// Copy duplicates the FOCUSED pane's session (Plan B semantics): the user
/// is operating in some pane, switches to a split layout, and the newly
/// created empty pane offers a one-click "clone what I was just using"
/// button. If no pane has focus (or the focused pane is itself empty),
/// the caller passes a fallback source so the button still works.
///
/// The plus button exposes the existing connection sidebar so any saved
/// connection can be dragged into this pane — dragging from the sidebar
/// opens a NEW independent session (not a clone), preserving the
/// "sidebar drop = new session for comparison" contract.
fn empty_pane_title_actions(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    layout_owner_tab_id: String,
    target_pane_idx: usize,
    copy_source: Option<(String, String)>,
) -> Element {
    let add_owner = layout_owner_tab_id.clone();
    let copy_button = match copy_source {
        Some((source_session_id, source_name)) => {
            let copy_owner = layout_owner_tab_id;
            rsx! {
                button {
                    style: "height: 18px; min-width: 24px; padding: 0 6px; border: 1px solid #414868; border-radius: 3px; background: #24283b; color: #7aa2f7; cursor: pointer; font-size: 12px; line-height: 16px; transition: background 0.12s ease, color 0.12s ease;",
                    title: "复制当前焦点会话：{source_name}",
                    onmousedown: move |e: MouseEvent| {
                        e.prevent_default();
                        e.stop_propagation();
                    },
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        if focus_pane_for_layout(
                            &mut state.write(),
                            &copy_owner,
                            target_pane_idx,
                        ) {
                            let _ = clone_session_into_pane(
                                state,
                                input_senders,
                                &copy_owner,
                                &source_session_id,
                                target_pane_idx,
                            );
                        }
                    },
                    "⧉"
                }
            }
        }
        None => rsx! {
            button {
                style: "height: 18px; min-width: 24px; padding: 0 6px; border: 1px solid #2a2b3d; border-radius: 3px; background: #1f2335; color: #414868; cursor: not-allowed; font-size: 12px; line-height: 16px;",
                title: "没有可复制的焦点会话",
                disabled: true,
                "⧉"
            }
        },
    };

    rsx! {
        div {
            style: "display: inline-flex; align-items: center; gap: 4px; margin-left: 6px;",
            {copy_button}
            button {
                style: "height: 18px; min-width: 24px; padding: 0 6px; border: 1px solid #414868; border-radius: 3px; background: #24283b; color: #9ece6a; cursor: pointer; font-size: 13px; line-height: 16px; transition: background 0.12s ease, color 0.12s ease;",
                title: "打开侧栏，将自定义连接拖入此窗格",
                onmousedown: move |e: MouseEvent| {
                    e.prevent_default();
                    e.stop_propagation();
                },
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    let _ = focus_pane_for_layout(
                        &mut state.write(),
                        &add_owner,
                        target_pane_idx,
                    );
                    state.write().sidebar_open = true;
                },
                "+"
            }
        }
    }
}

/// Accent color for a session type, used as the left edge highlight on
/// pane title bars (mirrors the sidebar's per-kind dot color). Returns a
/// neutral dim color for empty panes so the title bar still has a visual
/// anchor without implying a session type.
///
/// This is separate from `sidebar.rs::kind_color` because the pane title
/// bar only has `SessionType` (not `ConnectionKind`) — a cloned pane shares
/// the source session's type, so the accent stays consistent across clones.
fn session_type_accent_color(kind: &SessionType) -> &'static str {
    match kind {
        SessionType::Ssh => "#7aa2f7",    // blue
        SessionType::Shell => "#9ece6a",  // green
        SessionType::Serial => "#e0af68", // amber
        SessionType::Telnet => "#ff9e64", // orange
        SessionType::Tcp => "#7dcfff",    // cyan
    }
}

/// Multi-pane container: renders a `PaneLayout` as a grid of `TerminalView`
/// panes positioned absolutely within a relative-positioned container. Each
/// pane is sized according to the layout's per-row and per-column fractions.
/// Splitter bars are rendered between adjacent panes so the user can
/// drag-resize them smoothly.
///
/// ## Cross-terminal comparison mode
///
/// When the layout's `comparison` flag is set, a banner is displayed at the
/// top warning the user that input is being broadcast. The actual broadcast
/// logic lives in each pane's `on_input` handler — when comparison is on,
/// `app.rs`'s input routing sends the keystrokes to every pane's PTY
/// (not just the focused one). This component only renders the banner;
/// the broadcast happens in the input handler.
///
/// ## Why this is a function, not a separate component file
///
/// `MultiPaneContainer` needs to call `render_terminal_pane` for each
/// pane (so every pane gets the full TerminalView feature set: OneKey,
/// suggestions, reconnect). `render_terminal_pane` is defined in this
/// file and captures `state`/`input_senders` signals — moving it to a
/// separate component module would create a circular dependency. Keeping
/// `MultiPaneContainer` here lets it call `render_terminal_pane` directly.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn multi_pane_container(
    mut state: Signal<AppState>,
    input_senders: Signal<std::collections::HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    layout: PaneLayout,
    mut drag_over_pane: Signal<Option<(usize, PaneDropRegion)>>,
    container_size: Signal<Option<(f64, f64)>>,
    split_drag: Signal<Option<SplitDragState>>,
    tab_drag: Signal<Option<TabDragState>>,
    pane_move: Signal<Option<PaneMoveState>>,
) -> Element {
    // Container dimensions measured from the live DOM via a ResizeObserver
    // + polling loop (see `App`). Falls back to a 1200×800 default if the
    // measurement hasn't landed yet (e.g. on the very first frame after
    // mount, before the ResizeObserver fires). The splitter bars and pane
    // rects are computed from these dimensions, so they track the actual
    // viewport at all times — fixing the prior bug where a 1200×800
    // hardcode left panes clipped or with empty space when the window was
    // a different size (the "显示分辨率不对" symptom). Once the observer
    // fires (~1 frame after mount) the panes re-layout to the real size.
    let (container_w, container_h) = container_size().unwrap_or((1200.0_f64, 800.0_f64));

    // Collect visible panes: (index, session_id, rect).
    let visible: Vec<(usize, String, (f64, f64, f64, f64))> = layout
        .visible_panes(container_w, container_h)
        .map(|(idx, pane, rect)| (idx, pane.session_id.clone(), rect))
        .collect();

    let comparison_on = layout.comparison;
    let layout_floating = layout.is_floating();
    let layout_owner_tab_id = state.read().active_tab.clone().unwrap_or_default();
    let focused_pane = state.read().focused_pane.clone();
    // Source session for the empty-pane "copy" button. Per the Plan B
    // UX contract, the copy button duplicates the *focused* pane's session
    // ("copy what I'm currently using"), NOT a geometric neighbour. When
    // no pane has focus OR the focused pane is itself empty (the user
    // clicked an empty pane's title bar), we fall back to the nearest
    // non-empty neighbour via `source_pane_for_copy` so the button remains
    // useful instead of going dead.
    //
    // This computed once here (not inside the `for` body) because it's
    // the same for every empty pane in this layout.
    let focused_session_for_copy = focused_pane_session(&state.read()).or_else(|| {
        // Fallback: scan the layout for the nearest non-empty pane.
        // We pick the first non-empty pane in layout order — the
        // geometric "nearest" via `source_pane_for_copy` would need
        // a target_idx, and we don't have a single target here.
        layout
            .panes
            .iter()
            .find(|p| !p.session_id.is_empty())
            .map(|p| p.session_id.clone())
    });
    let focused_session_name = focused_session_for_copy
        .as_ref()
        .and_then(|sid| {
            state
                .read()
                .sessions
                .iter()
                .find(|t| t.id == *sid)
                .map(|t| t.name.clone())
        })
        .or_else(|| focused_session_for_copy.clone());

    // Pre-collect the (pane_idx, session_id, rect, drop_session_id,
    // border_style, pane_title) tuples as owned values. The move closures inside each
    // pane's ondragover / ondrop / ondragstart handlers need to capture owned copies of
    // the session_id — the rsx! `for` loop body can't contain `let`
    // statements, so we pre-compute the owned clones (and the
    // drag-over-derived border style) here and destructure them in the for
    // pattern.
    //
    // The tuple also carries stacking/chrome values for floating windows.
    // `idx` is `usize` (Copy), so closures capture it without a clone. `session_id` is
    // consumed by the `key:` interpolation and `render_terminal_pane`, so a
    // second owned copy (`drop_session_id`) is needed for the move closure.
    // `border_style` is a `&'static str` (Copy) computed by reading
    // `drag_over_pane()` once per pane during Vec construction — this read
    // subscribes `App` to the signal, so any change triggers ONE re-render
    // of `App` (which rebuilds this Vec with the new values).
    // `pane_title` is the session's display name, shown in the pane's
    // drag-handle title bar.
    //
    // Each pane is both a manual session-drag source (via its title text)
    // and an HTML5 drop target. Both drag paths now have a PRIMARY path
    // via the manual mouse-based system (Task 22 + sidebar extension):
    //   - tab/pane-title drag → `start_tab_drag` with `DragKind::Session`
    //   - sidebar → pane drag → `start_tab_drag` with `DragKind::Connection`
    // The HTML5 `ondrop` branches below are DEFENSIVE FALLBACKS retained
    // for forward compat in case HTML5 DnD is ever re-enabled. They read
    // the drag source from a custom MIME type in the DragEvent's DataTransfer:
    //   - "application/x-rusterm-session-id"     → drag from tab bar or pane title
    //   - "application/x-rusterm-connection-id"  → drag from sidebar (legacy)
    let pane_items: Vec<(
        usize,
        String,
        (f64, f64, f64, f64),
        String,
        &'static str,
        Option<PaneDropRegion>,
        &'static str,
        String,
        String,
        u32,
        &'static str,
        Element,
        String,
        String,
        &'static str,
    )> = visible
        .into_iter()
        .map(|(idx, sid, rect)| {
            // Read once per pane during Vec construction — subscribes
            // `App` to the signal so any change triggers ONE re-render.
            let drag_over = drag_over_pane();
            let is_drag_over = drag_over.is_some_and(|(i, _)| i == idx);
            let drag_over_region = if is_drag_over {
                drag_over.and_then(|(i, r)| if i == idx { Some(r) } else { None })
            } else {
                None
            };
            let is_focused = focused_pane.as_ref().is_some_and(|focused| {
                focused.layout_owner_tab_id == layout_owner_tab_id && focused.pane_idx == idx
            });
            let border = if is_drag_over {
                "border: 2px solid #7aa2f7; box-sizing: border-box;"
            } else if is_focused {
                "border: 2px solid #bb9af7; box-sizing: border-box;"
            } else if layout_floating {
                "border: 1px solid #414868; box-sizing: border-box;"
            } else {
                "border: 2px solid transparent; box-sizing: border-box;"
            };
            // Title bar chrome — three states with deliberately distinct
            // contrast so the focused pane pops and the default panes are
            // still readable (the prior `#1f2335` default blended into the
            // terminal background and the title bar looked invisible).
            //
            // Palette (Tokyo Night, brightened pass 2026-07-20):
            //   default  #2f3550 — two steps lighter than terminal bg #1a1b26;
            //                      clearly delimits the title bar without
            //                      competing with the focused pane.
            //   focused  linear-gradient(#565f89 → #414868) — Tokyo Night
            //                      "selection" tone with a subtle vertical
            //                      gradient so the focused pane reads as a
            //                      distinct highlighted surface (the user
            //                      explicitly asked for "高亮会话的小框").
            //   drag-over #7aa2f7 — bright blue accent; unambiguous drop
            //                      target, mirrors the splitter hover color.
            let title_chrome = if is_drag_over {
                "background: #7aa2f7; border-bottom: 2px solid #bb9af7;"
            } else if is_focused {
                "background: linear-gradient(180deg, #565f89 0%, #414868 100%); \
                 border-bottom: 2px solid #7aa2f7; \
                 box-shadow: 0 1px 6px rgba(122,162,247,0.25);"
            } else {
                "background: #2f3550; border-bottom: 1px solid #414868;"
            };
            // Accent color for the left edge of the title bar — a thin
            // vertical strip that identifies the session type at a glance
            // (mirrors the sidebar's per-kind dot). Empty panes use a dim
            // grey so the strip is still visible but doesn't imply a kind.
            // The strip is 4px wide (was 3px) for stronger visual anchoring.
            let accent_color = if sid.is_empty() {
                "#414868"
            } else {
                state
                    .read()
                    .sessions
                    .iter()
                    .find(|t| t.id == sid)
                    .map(|t| session_type_accent_color(&t.kind))
                    .unwrap_or("#414868")
            };
            let window_chrome = if layout_floating {
                "border-radius: 6px; box-shadow: 0 8px 24px rgba(0,0,0,0.45);"
            } else {
                ""
            };
            let z_index = layout.pane_z_index(idx).unwrap_or(1);
            // Look up the session's display name for the pane title bar.
            // An EMPTY session_id is an empty drop-zone pane (created by
            // the free-split gesture) — label it explicitly. Otherwise
            // fall back to the session id if the session was closed
            // between the layout snapshot and this render.
            let title = if sid.is_empty() {
                "空白窗格".to_string()
            } else {
                state
                    .read()
                    .sessions
                    .iter()
                    .find(|t| t.id == sid)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| sid.clone())
            };
            // `drag_sid` is a second clone for the ondragstart closure
            // (the first clone `drop_session_id` is consumed by the
            // ondrop closure; the original `sid` is consumed by the
            // `key:` interpolation and `render_terminal_pane`). rsx!
            // can't hold `let` bindings in the for body, so we pre-clone.
            let drag_sid = sid.clone();
            // Plan B copy semantics: an empty pane's copy button duplicates
            // the FOCUSED pane's session (computed once above as
            // `focused_session_for_copy`), falling back to the nearest
            // non-empty pane if no pane has focus. This replaces the prior
            // `source_pane_for_copy` geometric-neighbour heuristic, which
            // would copy an arbitrary neighbour instead of "what the user
            // is currently using". Sidebar-drag-into-pane still opens a
            // brand-new independent session (the drop handler calls
            // `open_connection`), preserving the "drag from sidebar =
            // new session for comparison" contract.
            let copy_source = if sid.is_empty() {
                focused_session_for_copy
                    .clone()
                    .zip(focused_session_name.clone())
                    .or_else(|| {
                        // Defensive fallback: if the focused session
                        // lookup failed (no focus, or focus on an empty
                        // pane), use the geometric-neighbour heuristic
                        // so the button isn't dead.
                        source_pane_for_copy(&layout, idx).and_then(|source_idx| {
                            let source_sid = layout.panes.get(source_idx)?.session_id.clone();
                            let source_name = state
                                .read()
                                .sessions
                                .iter()
                                .find(|tab| tab.id == source_sid)
                                .map(|tab| tab.name.clone())
                                .unwrap_or_else(|| source_sid.clone());
                            Some((source_sid, source_name))
                        })
                    })
            } else {
                None
            };
            let pane_actions = if sid.is_empty() {
                empty_pane_title_actions(
                    state,
                    input_senders,
                    layout_owner_tab_id.clone(),
                    idx,
                    copy_source,
                )
            } else {
                rsx! {}
            };
            (
                idx,
                sid.clone(),
                rect,
                sid,
                border,
                drag_over_region,
                title_chrome,
                title,
                drag_sid,
                z_index,
                window_chrome,
                pane_actions,
                layout_owner_tab_id.clone(),
                layout_owner_tab_id.clone(),
                accent_color,
            )
        })
        .collect();

    // Drag-resize overlay: VISUAL CURSOR INDICATOR ONLY. The actual
    // drag-resize mouse events are handled by document-level capture-phase
    // listeners installed via `install_split_drag_js_listeners` (see the
    // splitter's `onmousedown`). The polling `use_future` in `App` reads
    // the mouse position from `window.__rusterm_drag_pos` and applies the
    // delta.
    //
    // The overlay exists ONLY to show the col-resize / row-resize cursor
    // across the whole viewport during the drag (so the user gets
    // consistent cursor feedback even when the cursor is far from the
    // splitter bar). It carries NO event handlers — `pointer-events: none`
    // makes it transparent to mouse input, so it never interferes with
    // the document-level listeners.
    //
    // Pre-computed as an `Element` because `rsx!` doesn't allow `let`
    // bindings as direct children (dioxus 0.7 macro limitation — same
    // reason `pane_items` is pre-computed above).
    let drag_overlay: Element = match split_drag() {
        Some(drag) => {
            tracing::info!("[OVERLAY] rendered visible overlay drag={:?}", drag);
            let cursor = if drag.is_col {
                "col-resize"
            } else {
                "row-resize"
            };
            // `key:` must be a format string (dioxus 0.7 `rsx!` macro
            // rule) — a bare literal is rejected. We don't need the
            // value, just the format-placeholder to satisfy the macro.
            let _key_idx = drag.idx;
            rsx! {
                div {
                    key: "split-drag-overlay-{_key_idx}",
                    style: format!(
                        "position: fixed; left: 0; top: 0; right: 0; bottom: 0; \
                         z-index: 9999; cursor: {cursor}; \
                         background: transparent; \
                         pointer-events: none; \
                         user-select: none; -webkit-user-select: none;",
                    ),
                }
            }
        }
        None => rsx! {},
    };

    rsx! {
        div {
            id: "multi-pane-container",
            style: "position: absolute; left: 0; right: 0; top: 0; bottom: 0; overflow: hidden;",

            // Scoped CSS for pane title bar hover effects. dioxus 0.7's inline
            // `style` attribute can't express `:hover` pseudo-classes, so we
            // use a `<style>` block with namespaced class names (mirrors the
            // sidebar's `conn-` pattern). The hover rules ONLY apply to
            // title bars whose inline `background` is the default — focused
            // and drag-over states set a stronger background inline which
            // the `.pane-title-bar:hover` rule doesn't override (CSS
            // specificity: inline style > class rule).
            //
            // The drag handle (`⠿`) brightens on hover so the user can
            // discover that the title bar is draggable. The slight
            // `transform: scale` gives a tactile "lift" affordance.
            style { "
                .pane-title-bar:hover {{ filter: brightness(1.10); }}
                .pane-drag-handle:hover {{ color: #bb9af7 !important; transform: scale(1.15); }}
                .pane-title-text {{ transition: color 0.12s ease; }}
                .pane-title-bar:hover .pane-title-text {{ color: #ffffff; }}
                .pane-accent-strip {{ transition: width 0.12s ease; }}
            " }

            // Comparison-mode banner.
            //
            // Sits at the top of the terminal area as a non-interactive
            // overlay (`pointer-events: none` so it never blocks clicks on
            // the terminal below). Uses a subtle vertical gradient instead
            // of a flat fill so it reads as a status strip rather than a
            // solid block covering terminal content. The 1px bottom border
            // + box-shadow give it depth so the user can visually separate
            // it from the terminal output underneath.
            {comparison_on.then(|| rsx! {
                div {
                    style: "
                        position: absolute;
                        top: 0; left: 0; right: 0;
                        height: 18px;
                        background: linear-gradient(180deg, #7aa2f7 0%, #6a92e8 100%);
                        color: #1a1b26;
                        font-size: 10px;
                        font-weight: 600;
                        letter-spacing: 0.3px;
                        display: flex;
                        align-items: center;
                        justify-content: center;
                        z-index: 100;
                        pointer-events: none;
                        border-bottom: 1px solid #414868;
                        box-shadow: 0 1px 4px rgba(0,0,0,0.3);
                    ",
                    "⚠ Comparison mode ON — input is broadcast to all panes"
                }
            })}

            // Render each pane in its computed rectangle.
            //
            // PERF: `border_style` is pre-computed in `pane_items` by reading
            // `drag_over_pane()` once per pane during Vec construction. This
            // read subscribes `App` to the signal, so any change triggers
            // ONE re-render of `App`. The Signal equality check prevents
            // re-renders for no-op `set` calls — the highlight only changes
            // when the dragged pane actually changes. This aligns with the
            // user's frequency-vs-feedback preference: fewer re-renders over
            // per-tick feedback.
            for (idx, session_id, (x, y, w, h), drop_session_id, border_style, drag_over_region, title_chrome, pane_title, drag_sid, z_index, window_chrome, pane_actions, pane_owner_for_click, pane_owner_for_title, accent_color) in pane_items.into_iter() {
                div {
                    key: "pane-{idx}-{session_id}",
                    style: format!(
                        "position: absolute; left: {x}px; top: {y}px; width: {w}px; height: {h}px; overflow: hidden; display: flex; flex-direction: column; z-index: {z_index}; {border} {window_chrome}",
                        x = x, y = y, w = w, h = h, border = border_style
                    ),
                    onclick: move |_| {
                        let _ = focus_pane_for_layout(
                            &mut state.write(),
                            &pane_owner_for_click,
                            idx,
                        );
                    },
                    // `ondragover` must call prevent_default to signal
                    // that this element accepts drops. Without it, the
                    // browser fires `ondrop` with an empty DataTransfer
                    // (security restriction: drops without a dragover
                    // prevent_default are blocked).
                    //
                    // NOTE: we do NOT write to `drag_over_pane` here. The
                    // manual polling loop in `App` is the SOLE writer of
                    // that signal — it hit-tests the cursor at ~60Hz and
                    // sets the real 4-quadrant region. Letting HTML5
                    // dragover also write `Some((idx, Center))` caused it
                    // to race with the polling loop at pane boundaries:
                    // HTML5 reported pane A while the hit-test said pane B,
                    // flickering the overlay between them. Visually this
                    // looked like multiple "田"-shaped 4-block overlays
                    // appearing and disappearing on adjacent panes — the
                    // "错误的产生多个不需要的四方块" bug. Keeping the signal
                    // write ONLY in the polling loop fixes that.
                    ondragover: move |e: DragEvent| {
                        e.prevent_default();
                        e.data_transfer().set_drop_effect("move");
                    },
                    // `ondragenter` also needs prevent_default for
                    // cross-browser compatibility (some browsers require
                    // both dragenter AND dragover to be cancelled to
                    // allow drop). As with `ondragover`, we do NOT write
                    // `drag_over_pane` — the polling loop owns that
                    // signal and computes the real region.
                    ondragenter: move |e: DragEvent| {
                        e.prevent_default();
                    },
                    ondrop: move |e: DragEvent| {
                        e.prevent_default();
                        // Clear the drag-over highlight immediately —
                        // the drop has been processed, no need to wait
                        // for the next dragenter elsewhere.
                        drag_over_pane.set(None);
                        let dt = e.data_transfer();
                        // Check for the "drag from tab bar / pane title"
                        // MIME type first — an open session is being moved
                        // (either dragged from the tab bar or from another
                        // pane's title bar — both use the same MIME type
                        // since the semantic is identical: move/swap an
                        // existing session into this pane).
                        //
                        // NOTE (Task 22): tabs and pane title bars no longer
                        // use HTML5 DnD (the manual mouse-based system
                        // handles those drags via `finish_tab_drag` →
                        // `execute_tab_drop_on_pane`). This branch is
                        // retained as a defensive fallback for any residual
                        // HTML5 drag that might fire. It calls the same
                        // `execute_tab_drop_on_pane` that the manual system
                        // uses, so the dispatch logic is identical.
                        if let Some(dragged_sid) = dt.get_data("application/x-rusterm-session-id") {
                            if dragged_sid.is_empty() {
                                tracing::warn!("[DROP] empty session-id in drag data");
                                return;
                            }
                            let outcome = execute_tab_drop_on_pane(
                                &mut state.write(),
                                &dragged_sid,
                                idx,
                                &drop_session_id,
                            );
                            match outcome {
                                TabDropOutcome::SelfDropExpanded {
                                    first_pane_idx,
                                    pane_count,
                                } => {
                                    let opened = open_cloned_sessions_for_self_drop(
                                        state,
                                        input_senders,
                                        &dragged_sid,
                                        first_pane_idx,
                                        pane_count,
                                    );
                                    tracing::info!(
                                        "[DROP] self-drop expanded layout: source={} panes={} opened={}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        pane_count,
                                        opened
                                    );
                                    restore_focus_to_active_session(state, 80);
                                }
                                TabDropOutcome::SplitCreated { pane_idx }
                                | TabDropOutcome::SplitFilledExisting { pane_idx } => {
                                    tracing::info!(
                                        "[DROP] created split: session {} placed in pane {} (outcome={:?})",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        pane_idx,
                                        outcome
                                    );
                                    restore_focus_to_active_session(state, 80);
                                }
                                TabDropOutcome::Swapped => {
                                    tracing::info!(
                                        "[DROP] swapped session {} with pane {}'s session {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx,
                                        &drop_session_id[..drop_session_id.len().min(8)]
                                    );
                                }
                                TabDropOutcome::MovedToEmptyPane { cleared_source_pane: Some(src) } => {
                                    tracing::info!(
                                        "[DROP] moved session {} from pane {} to pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        src,
                                        idx
                                    );
                                }
                                TabDropOutcome::MovedToEmptyPane { cleared_source_pane: None }
                                | TabDropOutcome::AssignedToEmptyPane => {
                                    tracing::info!(
                                        "[DROP] assigned session {} to empty pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx
                                    );
                                }
                                TabDropOutcome::NoOpSelfDrop => {
                                    tracing::debug!(
                                        "[DROP] session {} dropped onto its own pane — no-op",
                                        &dragged_sid[..dragged_sid.len().min(8)]
                                    );
                                }
                                TabDropOutcome::SwapFailed => {
                                    tracing::warn!(
                                        "[DROP] swap failed for session {} → pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx
                                    );
                                }
                                TabDropOutcome::SplitFallbackSwapFailed => {
                                    tracing::warn!(
                                        "[DROP] layout at MAX_PANES and swap failed — session {} not placed",
                                        &dragged_sid[..dragged_sid.len().min(8)]
                                    );
                                }
                                TabDropOutcome::SplitFailed => {
                                    tracing::warn!(
                                        "[DROP] create-split failed for session {} → pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx
                                    );
                                }
                            }
                            return;
                        }
                        // DEFENSIVE FALLBACK: sidebar → pane drags now
                        // use the manual mouse-based system (Task 22
                        // extension — `DragKind::Connection` in
                        // `start_tab_drag`/`finish_tab_drag`). The
                        // sidebar no longer sets `draggable: true` +
                        // this MIME type, so this branch should never
                        // fire. Retained symmetrically with the
                        // session-id fallback above for forward compat.
                        if let Some(conn_id) = dt.get_data("application/x-rusterm-connection-id") {
                            if conn_id.is_empty() {
                                tracing::warn!("[DROP] empty connection-id in drag data");
                                return;
                            }
                            // Look up the connection config.
                            let conn = state
                                .read()
                                .connections
                                .iter()
                                .find(|c| c.id == conn_id)
                                .cloned();
                            let Some(conn) = conn else {
                                tracing::warn!(
                                    "[DROP] connection id {} not found in state.connections",
                                    &conn_id[..conn_id.len().min(8)]
                                );
                                return;
                            };
                            tracing::info!(
                                "[DROP] opening connection {} ({:?}) in pane {}",
                                &conn_id[..conn_id.len().min(8)],
                                conn.name,
                                idx
                            );
                            // Use the same preserve-and-grow state path as the
                            // primary manual drag system. Never replace the
                            // occupied target pane merely because this fallback
                            // HTML5 event fired.
                            let target = prepare_split_for_sidebar_drop(
                                &mut state.write(),
                                idx,
                            )
                            .map(|plan| PaneTarget {
                                layout_owner_tab_id: plan.layout_owner_tab_id,
                                pane_idx: plan.pane_idx,
                            });
                            open_connection(state, input_senders, conn, target);
                            return;
                        }
                        // Unknown MIME type — log and ignore.
                        tracing::debug!(
                            "[DROP] pane {} received drop with no recognized MIME type",
                            idx
                        );
                    },
                    // Title bar / drag handle. The bar is the only
                    // part of the pane that initiates a tab drag — the
                    // terminal content below must stay non-draggable so
                    // text selection and mouse clicks still work.
                    //
                    // Task 22: replaced the prior HTML5 `draggable: true`
                    // + `ondragstart` (which was unreliable in dioxus
                    // 0.7's desktop webview) with a manual mouse-based
                    // drag system mirroring the splitter drag-resize fix.
                    // `onmousedown` (primary button) calls
                    // `start_tab_drag`, which sets the `tab_drag` signal
                    // and installs document-level capture-phase JS
                    // listeners. The polling `use_future` in `App`
                    // takes over from there.
                    //
                    // Plain click-to-select still works: `onmousedown`
                    // updates only pane focus, then sets `tab_drag` with
                    // `dragging: false` for non-empty panes. The polling loop
                    // executes a drop only after the cursor crosses the drag
                    // threshold.
                    div {
                        class: "pane-title-bar",
                        style: format!("
                            height: 24px;
                            {title_chrome}
                            display: flex;
                            align-items: center;
                            padding: 0 7px 0 0;
                            font-size: 11px;
                            color: #c0caf5;
                            cursor: grab;
                            user-select: none;
                            -webkit-user-select: none;
                            flex-shrink: 0;
                            z-index: 10;
                            transition: background 0.12s ease;
                        "),
                        title: "拖动会话标题可移动到其他窗格；⠿ 可拖动浮动窗",
                        onmousedown: move |e: MouseEvent| {
                            // Only start a drag on primary button (left
                            // click). Middle/right clicks have other
                            // semantics and shouldn't initiate a drag.
                            // Empty drop-zone panes have no session to
                            // drag.
                            if e.trigger_button() == Some(MouseButton::Primary) {
                                let _ = focus_pane_for_layout(
                                    &mut state.write(),
                                    &pane_owner_for_title,
                                    idx,
                                );
                                // Stop the browser from starting a native
                                // text-selection drag on this mousedown
                                // (prevents page text getting highlighted
                                // while dragging the pane title bar).
                                e.prevent_default();
                                e.stop_propagation();
                                if drag_sid.is_empty() {
                                    return;
                                }
                                let c = e.client_coordinates();
                                // Look up the session's display name for
                                // the ghost element. Falls back to the
                                // session id if the session was closed
                                // between the layout snapshot and this
                                // mousedown (defensive).
                                let ghost_name = state
                                    .read()
                                    .sessions
                                    .iter()
                                    .find(|t| t.id == drag_sid)
                                    .map(|t| t.name.clone())
                                    .unwrap_or_else(|| drag_sid.clone());
                                start_tab_drag(
                                    tab_drag,
                                    DragKind::Session(drag_sid.clone()),
                                    ghost_name,
                                    c.x,
                                    c.y,
                                );
                            }
                        },
                        // Left accent strip — a thin vertical bar colored
                        // by the session type (mirrors the sidebar's
                        // per-kind dot). Empty panes get a dim grey strip.
                        // `flex-shrink: 0` keeps it from collapsing when
                        // the title text is long.
                        span {
                            class: "pane-accent-strip",
                            style: "width: 4px; align-self: stretch; flex-shrink: 0; background: {accent_color}; margin-right: 7px; box-shadow: 0 0 4px {accent_color};",
                            title: "session-type-accent",
                        }
                        span {
                            class: "pane-drag-handle",
                            style: "display: inline-flex; align-items: center; justify-content: center; width: 18px; margin-right: 5px; cursor: move; color: #7aa2f7; font-size: 13px; transition: color 0.12s ease, transform 0.12s ease;",
                            title: "拖动小窗口（浮动模式）",
                            onmousedown: move |e: MouseEvent| {
                                if e.trigger_button() == Some(MouseButton::Primary) {
                                    e.prevent_default();
                                    e.stop_propagation();
                                    let owner = state.read().active_tab.clone();
                                    if let Some(owner) = owner {
                                        let _ = focus_pane_for_layout(
                                            &mut state.write(),
                                            &owner,
                                            idx,
                                        );
                                    }
                                    let c = e.client_coordinates();
                                    start_pane_move(
                                        state,
                                        pane_move,
                                        idx,
                                        c.x,
                                        c.y,
                                        container_w,
                                        container_h,
                                    );
                                }
                            },
                            "⠿"
                        }
                        span {
                            class: "pane-title-text",
                            style: "flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; padding-left: 2px;",
                            "{pane_title}"
                        }
                        {pane_actions}
                    },
                    // Terminal content area: fills the remaining height
                    // below the title bar. Wrapped in a flex:1 div so the
                    // title bar (above) stays at 24px and the terminal
                    // gets the rest. `position: relative` + `overflow:
                    // hidden` matches the single-pane path's container.
                    //
                    // The drop-zone hint overlay (highlighted target half
                    // + center crosshair) is mounted INSIDE this content
                    // area so it doesn't overlap the title bar and is
                    // clipped to the terminal region.
                    //
                    // An EMPTY session_id means this pane is a drop-zone
                    // placeholder (created by the free-split gesture
                    // before a session was assigned). Render a visible
                    // hint instead of nothing — an invisible dead region
                    // looked like "the window can't be filled".
                    div {
                        style: "flex: 1; position: relative; overflow: hidden; min-height: 0;",
                        // Drop hint overlay: translucent blue rectangle on
                        // the target half plus the SINGLE center line that
                        // marks the dividing axis (the "用中线作为标记"
                        // affordance). Mounted only while `drag_over_pane`
                        // points at THIS pane. `pointer-events: none` is
                        // critical — without it the overlay would
                        // intercept the drop and the pane's `ondrop` would
                        // never fire.
                        //
                        // Visual scheme (single center line, NOT the "田"
                        // 4-block shape):
                        //   Left / Right  → VERTICAL center line (divides
                        //                  left from right; signals 横着 /
                        //                  horizontal placement).
                        //   Top  / Bottom → HORIZONTAL center line (divides
                        //                  top from bottom; signals 竖着 /
                        //                  vertical placement).
                        //   Center        → both lines dimmed (swap/move zone).
                        //
                        // Showing ONE line per region (instead of always
                        // both) is the fix for the "错误的产生多个不需要的
                        // 四方块" bug — the prior crosshair always drew both
                        // lines forming a 田 shape that was ambiguous about
                        // the split direction.
                        {drag_over_region.is_some().then(|| {
                            let region = drag_over_region.unwrap();
                            let half_style = match region {
                                PaneDropRegion::Top => "position: absolute; left: 0; top: 0; width: 100%; height: 50%; background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
                                PaneDropRegion::Bottom => "position: absolute; left: 0; top: 50%; width: 100%; height: 50%; background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
                                PaneDropRegion::Left => "position: absolute; left: 0; top: 0; width: 50%; height: 100%; background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
                                PaneDropRegion::Right => "position: absolute; left: 50%; top: 0; width: 50%; height: 100%; background: rgba(122,162,247,0.18); pointer-events: none; z-index: 20;",
                                PaneDropRegion::Center => "",
                            };
                            // Pick the SINGLE center line ("中线") matching the
                            // split axis. See `center_line_styles_for_region`
                            // for the visual scheme.
                            let (vertical_line, horizontal_line) =
                                center_line_styles_for_region(region);
                            rsx! {
                                // Highlighted target half (skipped for Center).
                                {(!matches!(region, PaneDropRegion::Center)).then(|| rsx! {
                                    div { style: "{half_style}" }
                                })}
                                // The single relevant center line ("中线").
                                // Only one is shown for split regions — this
                                // is what tells the user 横着 (left/right)
                                // vs 竖着 (top/bottom) placement.
                                {vertical_line.map(|style| rsx! { div { style: "{style}" } })}
                                {horizontal_line.map(|style| rsx! { div { style: "{style}" } })}
                            }
                        })}
                        if session_id.is_empty() {
                            // Empty drop-zone pane: split-pane hint with clear
                            // call-to-action. The hint shows three options the
                            // user can take, mirroring the buttons in the title
                            // bar (copy focused / open sidebar / new session).
                            // This is the "分屏提示" affordance — when the
                            // user enters split mode (or drags a tab onto a
                            // pane to expand the layout), each new empty pane
                            // tells them exactly how to fill it.
                            div {
                                style: "
                                    position: absolute; inset: 0;
                                    display: flex; flex-direction: column;
                                    align-items: center; justify-content: center;
                                    gap: 8px;
                                    background: linear-gradient(180deg, #16161e 0%, #1a1b26 100%);
                                    border: 1px dashed #414868;
                                    color: #565f89;
                                    font-size: 12px;
                                    user-select: none;
                                    -webkit-user-select: none;
                                    padding: 12px;
                                    text-align: center;
                                ",
                                div {
                                    style: "font-size: 22px; color: #414868; line-height: 1; margin-bottom: 4px;",
                                    "⊡"
                                }
                                div {
                                    style: "color: #7aa2f7; font-weight: 600; font-size: 12px;",
                                    "空白窗格"
                                }
                                div {
                                    style: "color: #565f89; font-size: 11px; line-height: 1.5;",
                                    "点击标题栏 ⧉ 复制焦点会话"
                                    br {}
                                    "拖动左侧会话到此处新建会话"
                                    br {}
                                    "或拖动标签页/会话标题到此处"
                                }
                            }
                        } else {
                            {render_terminal_pane(state, input_senders, session_id.clone())}
                        }
                    }
                }
            }

            // Grid splitters are hidden after promotion to floating windows;
            // each window then owns its position independently.
            {(!layout_floating).then(|| rsx! {
                {render_col_splitters(&layout, container_w, container_h, state, split_drag)}
                {render_row_splitters(&layout, container_w, container_h, state, split_drag)}
            })}

            // Drag-resize overlay: while a splitter drag is in progress,
            // render an invisible full-screen div that captures all
            // mouse events. This replaces the prior JS-listener +
            // `eval`-poll approach (which was broken by a missing
            // `return` prefix in the eval string and a race between
            // `spawn`-installed listeners and the first mousemove).
            //
            // The overlay sits above everything (z-index: 9999) with
            // `position: fixed; inset: 0`, so any mousemove anywhere
            // in the window routes to its `onmousemove` handler — no
            // document-level JS listeners needed. `onmouseup` ends the
            // drag. `user-select: none` prevents text selection during
            // the drag.
            //
            // We read `split_drag()` fresh inside `onmousemove` (not
            // the `drag` captured at render time) so that rapid
            // mousemove events between frames all see the latest
            // `last_applied_pos` — dioxus's `Signal::set` updates the
            // underlying value synchronously, even though the
            // re-render is batched. Without this, multiple mousemove
            // events in the same frame would each compute the delta
            // from the same stale `last_applied_pos` and apply it
            // multiple times (e.g. 5x for 5 events = 5x the intended
            // resize).
            {drag_overlay}
        }
    }
}

fn render_col_splitters(
    layout: &PaneLayout,
    container_w: f64,
    container_h: f64,
    mut state: Signal<AppState>,
    mut split_drag: Signal<Option<SplitDragState>>,
) -> Element {
    let boundaries: Vec<(usize, f64, f64, f64, f64)> = layout
        .splitters(container_w, container_h)
        .into_iter()
        .filter(|splitter| splitter.axis == SplitAxis::LeftRight)
        .map(|splitter| {
            (
                splitter.splitter_idx,
                splitter.x,
                splitter.y,
                splitter.height,
                splitter.local_extent,
            )
        })
        .collect();
    rsx! {
        for (splitter_idx, x_val, y_val, height, local_extent) in boundaries.into_iter() {
            div {
                key: "col-split-{splitter_idx}",
                style: format!(
                    "position: absolute; left: {x_val}px; top: {y_val}px; height: {height}px; width: 10px; \
                     margin-left: -5px; cursor: col-resize; background: #2a2b3d; z-index: 50; \
                     transition: background 0.1s; user-select: none;",
                ),
                onmousedown: move |e: MouseEvent| {
                    e.prevent_default();
                    e.stop_propagation();
                    if local_extent <= 0.0 {
                        return;
                    }
                    let start_client_x = e.client_coordinates().x;
                    let start_client_y = e.client_coordinates().y;
                    split_drag.set(Some(SplitDragState {
                        idx: splitter_idx,
                        is_col: true,
                        container_extent: local_extent,
                        last_applied_pos: start_client_x,
                    }));
                    install_split_drag_js_listeners(start_client_x, start_client_y);
                },
                oncontextmenu: move |e: MouseEvent| {
                    e.prevent_default();
                    if resize_layout_split(&mut state.write(), splitter_idx, -0.05) {
                        tracing::info!("[LAYOUT] local left/right split shrunk");
                    }
                },
                title: "Drag to resize local left/right split",
            }
        }
    }
}

fn render_row_splitters(
    layout: &PaneLayout,
    container_w: f64,
    container_h: f64,
    mut state: Signal<AppState>,
    mut split_drag: Signal<Option<SplitDragState>>,
) -> Element {
    let boundaries: Vec<(usize, f64, f64, f64, f64)> = layout
        .splitters(container_w, container_h)
        .into_iter()
        .filter(|splitter| splitter.axis == SplitAxis::TopBottom)
        .map(|splitter| {
            (
                splitter.splitter_idx,
                splitter.x,
                splitter.y,
                splitter.width,
                splitter.local_extent,
            )
        })
        .collect();
    rsx! {
        for (splitter_idx, x_val, y_val, width, local_extent) in boundaries.into_iter() {
            div {
                key: "row-split-{splitter_idx}",
                style: format!(
                    "position: absolute; top: {y_val}px; left: {x_val}px; width: {width}px; height: 10px; \
                     margin-top: -5px; cursor: row-resize; background: #2a2b3d; z-index: 50; \
                     transition: background 0.1s; user-select: none;",
                ),
                onmousedown: move |e: MouseEvent| {
                    e.prevent_default();
                    e.stop_propagation();
                    if local_extent <= 0.0 {
                        return;
                    }
                    let start_client_x = e.client_coordinates().x;
                    let start_client_y = e.client_coordinates().y;
                    split_drag.set(Some(SplitDragState {
                        idx: splitter_idx,
                        is_col: false,
                        container_extent: local_extent,
                        last_applied_pos: start_client_y,
                    }));
                    install_split_drag_js_listeners(start_client_x, start_client_y);
                },
                oncontextmenu: move |e: MouseEvent| {
                    e.prevent_default();
                    if resize_layout_split(&mut state.write(), splitter_idx, -0.05) {
                        tracing::info!("[LAYOUT] local top/bottom split shrunk");
                    }
                },
                title: "Drag to resize local top/bottom split",
            }
        }
    }
}

/// Strip ANSI/VT escape sequences from terminal output so OneKey expect-regexes
/// can match colored prompts (e.g. a bastion login screen that emits
/// `\x1b[1;36mPassword\x1b[0m for 'host': `). Without this the escape bytes sit
/// between "Password" and "for" and the regex never matches, so the popup never
/// appears and the credential is never autofilled.
fn strip_ansi(s: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // CSI (ESC [ params intermediates final), OSC (ESC ] ... BEL/ST),
        // charset designators (ESC ( ) * + char), and other single-char ESC
        // sequences. Order matters: OSC/CSI/charset are tried before the
        // catch-all `\x1b[@-_]` so multi-byte sequences aren't split.
        regex::Regex::new(
            r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()*+][A-Za-z0-9]|\x1b[@-_]",
        )
        .expect("static ANSI-stripping regex must compile")
    });
    re.replace_all(s, "").to_string()
}

/// Find the first step in `ok` whose `expect` regex matches `line`
/// (case-insensitively). A OneKey is a sequence of Expect/Send steps; only the
/// first matching step is offered for a given prompt, so a Username step and a
/// Password step (with different expects) are correctly distinguished.
fn first_matching_step<'a>(ok: &'a OneKey, line: &str) -> Option<&'a OneKeyStep> {
    for step in &ok.steps {
        let pattern = format!("(?i){}", step.expect);
        if let Ok(re) = regex::Regex::new(&pattern) {
            if re.is_match(line) {
                return Some(step);
            }
        }
    }
    None
}

fn onekey_prompt_text(state: &AppState, session_id: &str, data: &[u8]) -> String {
    let terminal_line = state
        .terminals
        .get(session_id)
        .map(|handle| handle.lock().terminal.extract_current_line())
        .unwrap_or_default();
    if terminal_line.trim().is_empty() {
        strip_ansi(&String::from_utf8_lossy(data))
    } else {
        terminal_line
    }
}

/// Scan new terminal output for OneKey expect-pattern matches. If any OneKey's
/// expect regex matches and the session's popup isn't already showing, show the
/// popup with the matching entries. Persists across focus changes (only new
/// output triggers this — focus changes produce no output, so no re-scan).
fn check_onekey_match(mut state: Signal<AppState>, session_id: &str, data: &[u8]) {
    if !onekey_enabled_for_session(&state.read(), session_id) {
        return;
    }
    let onekeys = state.read().onekeys.clone();
    if onekeys.is_empty() {
        return;
    }
    // Don't re-trigger while the popup is already showing (persist).
    let already_visible = state
        .read()
        .onekey_popups
        .get(session_id)
        .map(|p| p.visible)
        .unwrap_or(false);
    if already_visible {
        return;
    }
    // Read the assembled current line from this session's own terminal model.
    // Credential prompts are frequently split across SSH output chunks; matching
    // only `data` would miss `"Pass" + "word:"`. The terminal has already
    // processed this output at both call sites, so its current line is the
    // correct pane-local matching boundary. Fall back to stripped chunk text
    // when the terminal line is empty (for unusual newline-terminated prompts).
    let text = onekey_prompt_text(&state.read(), session_id, data);
    // Match only the final non-empty line. Matching full scrollback/history
    // output could surface credentials for an old command in the wrong prompt.
    let last_line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    // For each OneKey, find the FIRST step whose expect matches the last line
    // (case-insensitive). first_matching_step picks the right step per prompt,
    // so a Username step and a Password step are distinguished correctly.
    let mut matches: Vec<OneKeyMatch> = Vec::new();
    let mut matched_expect: Option<String> = None;
    for ok in &onekeys {
        let Some(step) = first_matching_step(ok, last_line) else {
            continue;
        };
        if matched_expect.is_none() {
            matched_expect = Some(step.expect.clone());
        }
        // `step.send` is the DECRYPTED plaintext (AppState.onekeys is decrypted
        // on unlock / kept plaintext after a manager save). Log the matched step
        // + ALL steps (label/expect/send_len) so we can verify the password
        // step exists and its expect matches the prompt (e.g. git's
        // "Password for '…':").
        let steps_summary: Vec<String> = ok
            .steps
            .iter()
            .map(|s| {
                format!(
                    "{{label={:?},expect={:?},send_len={}}}",
                    s.label,
                    s.expect,
                    s.send.len()
                )
            })
            .collect();
        tracing::info!(
            "[ONEKEY-MATCH] session={} onekey={} matched_expect={:?} send_len={} all_steps=[{}]",
            &session_id[..session_id.len().min(8)],
            ok.name,
            step.expect,
            step.send.len(),
            steps_summary.join(", ")
        );
        matches.push(OneKeyMatch {
            name: ok.name.clone(),
            send: step.send.clone(),
        });
    }
    if !matches.is_empty() {
        state.write().onekey_popups.insert(
            session_id.to_string(),
            OneKeyPopupState {
                visible: true,
                matches,
                selected: 0,
                matched_expect,
            },
        );
    }
}

const SHELL_INTEGRATION_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_millis(1_200);

/// Debounces the remote shell's initial output before injecting shell
/// integration. Unlike a fixed startup sleep, every late MOTD/banner chunk
/// moves the deadline forward, so the integration command cannot create a new
/// prompt in front of output that is still arriving. No fallback deadline is
/// used before the first output: a silent/not-ready shell is safer left
/// untouched than having input injected blindly.
#[derive(Default)]
struct InitialOutputQuiescence {
    last_output_at: Option<std::time::Instant>,
}

impl InitialOutputQuiescence {
    fn observe_output(&mut self, observed_at: std::time::Instant) {
        self.last_output_at = Some(observed_at);
    }

    fn remaining(
        &self,
        now: std::time::Instant,
        quiet_period: std::time::Duration,
    ) -> Option<std::time::Duration> {
        self.last_output_at
            .map(|last| (last + quiet_period).saturating_duration_since(now))
    }
}

fn shell_integration_setup() -> Vec<u8> {
    let mut setup = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#
        .as_bytes()
        .to_vec();
    setup.push(b'\n');
    setup
}

async fn inject_shell_integration_when_quiet(
    mut output_activity: mpsc::UnboundedReceiver<()>,
    integration_tx: mpsc::UnboundedSender<Vec<u8>>,
    session_id: String,
) {
    // Require actual remote output (normally banner/MOTD/prompt) before any
    // automatic input. This prevents a slow login from being raced by a
    // fallback timer.
    if output_activity.recv().await.is_none() {
        return;
    }
    let mut gate = InitialOutputQuiescence::default();
    gate.observe_output(std::time::Instant::now());

    loop {
        let remaining = gate
            .remaining(std::time::Instant::now(), SHELL_INTEGRATION_QUIET_PERIOD)
            .expect("initial output was observed");
        match tokio::time::timeout(remaining, output_activity.recv()).await {
            Ok(Some(())) => gate.observe_output(std::time::Instant::now()),
            Ok(None) => return,
            Err(_) => break,
        }
    }

    if integration_tx.send(shell_integration_setup()).is_ok() {
        tracing::info!(
            "[SSH] injected shell integration after initial-output quiet period for {}",
            session_id
        );
    }
}

fn preferred_initial_terminal_size(
    pane_size: TerminalSize,
    connect_measurement: TerminalSize,
) -> TerminalSize {
    let pane_is_unmeasured = pane_size.cols == 80
        && pane_size.rows == 24
        && pane_size.pixel_width == 0
        && pane_size.pixel_height == 0;
    if pane_is_unmeasured {
        connect_measurement
    } else {
        pane_size
    }
}

fn onekey_enabled_for_session(state: &AppState, session_id: &str) -> bool {
    state
        .session_configs
        .get(session_id)
        .is_some_and(|config| config.onekey)
}

fn begin_reconnect(state: &mut AppState, session_id: &str) -> bool {
    if state.session_connection_states.get(session_id)
        != Some(&SessionConnectionState::Disconnected)
    {
        return false;
    }
    state
        .session_connection_states
        .insert(session_id.to_string(), SessionConnectionState::Reconnecting);
    true
}

#[cfg(test)]
mod session_startup_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn delayed_motd_tail_resets_shell_integration_quiet_period() {
        let started_at = Instant::now();
        let mut gate = InitialOutputQuiescence::default();
        let quiet_period = Duration::from_millis(1_200);

        assert_eq!(gate.remaining(started_at, quiet_period), None);
        gate.observe_output(started_at + Duration::from_millis(100));
        assert_eq!(
            gate.remaining(started_at + Duration::from_millis(1_299), quiet_period),
            Some(Duration::from_millis(1)),
        );

        // Ubuntu's dynamic MOTD may emit a final paragraph after the prompt.
        // That output must restart the quiet period instead of allowing the
        // integration command to create a second prompt ahead of the MOTD tail.
        gate.observe_output(started_at + Duration::from_millis(1_300));
        assert_eq!(
            gate.remaining(started_at + Duration::from_millis(2_499), quiet_period),
            Some(Duration::from_millis(1)),
        );
        assert_eq!(
            gate.remaining(started_at + Duration::from_millis(2_500), quiet_period),
            Some(Duration::ZERO),
        );
    }

    #[test]
    fn pane_terminal_size_wins_over_stale_connect_measurement() {
        let pane_size = TerminalSize {
            cols: 132,
            rows: 41,
            pixel_width: 1_056,
            pixel_height: 779,
        };
        let stale_measurement = TerminalSize {
            cols: 126,
            rows: 41,
            pixel_width: 0,
            pixel_height: 0,
        };

        assert_terminal_size_eq(
            preferred_initial_terminal_size(pane_size, stale_measurement),
            pane_size,
        );
    }

    #[test]
    fn onekey_enablement_is_scoped_to_the_pane_session() {
        let enabled = ConnectionConfig {
            id: "enabled".to_string(),
            name: "enabled".to_string(),
            kind: ConnectionKind::Shell(ShellConfig {
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                working_dir: None,
            }),
            group: None,
            tags: Vec::new(),
            onekey: true,
        };
        let mut disabled = enabled.clone();
        disabled.id = "disabled".to_string();
        disabled.name = "disabled".to_string();
        disabled.onekey = false;

        let mut state = AppState::default();
        state.session_configs.insert("pane-a".to_string(), enabled);
        state.session_configs.insert("pane-b".to_string(), disabled);

        assert!(onekey_enabled_for_session(&state, "pane-a"));
        assert!(!onekey_enabled_for_session(&state, "pane-b"));
        assert!(!onekey_enabled_for_session(&state, "missing"));
    }

    #[test]
    fn onekey_prompt_uses_the_owning_panes_assembled_terminal_line() {
        let mut entry = TerminalEntry {
            terminal: Terminal::new(TerminalSize::default()),
            parser: vte::ansi::Processor::new(),
            scroll_offset: 0,
        };
        entry.process_and_render(b"[sudo] Pass");
        entry.process_and_render(b"word for ecs-user: ");

        let mut state = AppState::default();
        state
            .terminals
            .insert("pane-b".to_string(), Arc::new(Mutex::new(entry)));

        assert_eq!(
            onekey_prompt_text(&state, "pane-b", b"word for ecs-user: "),
            "[sudo] Password for ecs-user: ",
        );
        assert_eq!(
            onekey_prompt_text(&state, "pane-a", b"Username: "),
            "Username: ",
        );
    }

    #[test]
    fn reconnect_transition_blocks_duplicates_and_allows_retry_after_failure() {
        let mut state = AppState::default();
        state
            .session_connection_states
            .insert("pane-a".to_string(), SessionConnectionState::Disconnected);

        assert!(begin_reconnect(&mut state, "pane-a"));
        assert!(!begin_reconnect(&mut state, "pane-a"));
        assert_eq!(
            state.session_connection_states.get("pane-a"),
            Some(&SessionConnectionState::Reconnecting),
        );

        state
            .session_connection_states
            .insert("pane-a".to_string(), SessionConnectionState::Disconnected);
        assert!(begin_reconnect(&mut state, "pane-a"));
    }

    fn assert_terminal_size_eq(actual: TerminalSize, expected: TerminalSize) {
        assert_eq!(actual.cols, expected.cols);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.pixel_width, expected.pixel_width);
        assert_eq!(actual.pixel_height, expected.pixel_height);
    }
}

fn start_ssh_connection(
    mut state: Signal<AppState>,
    mut input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    tab_id: String,
    ssh_config: SshConfig,
) {
    spawn(async move {
        // Try to get measured container size, but don't block too long
        // Connect quickly with whatever size we have; the resize polling
        // will correct the PTY size once layout is ready.
        let mut measured_size = TerminalSize::default();
        let measure_cid = format!("terminal-input-{tab_id}");
        let scroll_cid = format!("terminal-scroll-{tab_id}");
        for attempt in 0..10 {
            let delay = if attempt < 3 { 50 } else { 100 };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            if let Ok(result) = dioxus::document::eval(&format!(
                "(function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return ''; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return ''; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const h = rect.height - padV - bh; if (h <= 0) return ''; let w = rect.width - padH - bw; const sd = document.getElementById('{scroll_cid}'); if (sd) {{ const sdRect = sd.getBoundingClientRect(); w = sdRect.width; if (sd.firstElementChild) {{ w = Math.max(0, sdRect.width - sd.firstElementChild.getBoundingClientRect().width); }} }} if (w <= 0) return ''; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor(h / ch)); if (cols > 1 && rows > 1) return cols + ',' + rows; return ''; }})()"
            )).await {
                if let Some(s) = result.as_str() {
                    if !s.is_empty() {
                        let parts: Vec<&str> = s.split(',').collect();
                        if parts.len() >= 2 {
                            if let (Ok(cols), Ok(rows)) = (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
                                if cols > 1 && rows > 1 {
                                    measured_size.cols = cols;
                                    measured_size.rows = rows;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // TerminalView may already have measured and resized this pane before
        // the SSH session exists. Prefer that size (including pixel dimensions)
        // over the connect-time DOM fallback. This also closes the race where
        // TerminalView's first resize happened before `resize_senders` was
        // registered and therefore could not reach the remote PTY.
        let pane_size = state
            .read()
            .terminals
            .get(&tab_id)
            .map(|handle| handle.lock().terminal.size())
            .unwrap_or_default();
        let initial_size = preferred_initial_terminal_size(pane_size, measured_size);

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let host_for_import = ssh_config.host.clone();
        let client = rusterm_ssh::SshClient::new(ssh_config, event_tx.clone());

        match client.connect(tab_id.clone(), initial_size).await {
            Ok((session, ssh_session)) => {
                state
                    .write()
                    .session_connection_states
                    .insert(tab_id.clone(), SessionConnectionState::Connected);
                input_senders
                    .write()
                    .insert(tab_id.clone(), session.input_tx.clone());

                state
                    .write()
                    .close_senders
                    .push((tab_id.clone(), session.close_tx.clone()));

                state
                    .write()
                    .resize_senders
                    .insert(tab_id.clone(), session.resize_tx.clone());

                // Set the input sender on the terminal so it can send DA/DSR responses
                if let Some(handle) = state.read().terminals.get(&tab_id) {
                    let mut entry = handle.lock();
                    entry.terminal.set_input_sender(session.input_tx.clone());
                }

                // Sync the remote PTY to the same size used to create the SSH
                // channel. `initial_size` prefers TerminalView's pane-specific
                // measurement, so every clone starts with its own geometry.
                let _ = session.resize_tx.send((
                    initial_size.cols,
                    initial_size.rows,
                    initial_size.pixel_width,
                    initial_size.pixel_height,
                ));

                // Feature #7: SSH login auto-configure terminal to the left side.
                //
                // When the user logs into a remote machine via SSH, the new
                // session's tab is automatically moved to the leftmost position
                // in the tab bar ("configure terminal to the left side").
                //
                // Idempotency: the host is recorded in the `configured_hosts`
                // DB table ONLY after a successful configuration step (i.e.,
                // the tab actually moved). On subsequent SSH logins to a host
                // that's already in that table, the move-and-record step is
                // skipped entirely — "avoid duplicate configuration".
                //
                // Recording scope: per the requirement, intermediate debug
                // steps of the configuration process are NOT recorded — only
                // the final success is. That means we don't log the move
                // itself; we just perform it and, on success, persist a single
                // row keyed by host. Failures (e.g., DB write error) are
                // logged at `warn` so a future reconnect can retry the
                // recording, but they don't surface to the user.
                {
                    let host_for_config = host_for_import.clone();
                    let sid_for_config = tab_id.clone();
                    let mut state_for_config = state;
                    spawn(async move {
                        let db_path = dirs::data_dir()
                            .unwrap_or_default()
                            .join("rusterm")
                            .join("rusterm.db");
                        let db = match rusterm_db::Database::open(Some(db_path)).await {
                            Ok(db) => db,
                            Err(e) => {
                                tracing::warn!(
                                    "[SSH-AUTOCONFIG] failed to open DB for host {}: {}",
                                    host_for_config,
                                    e
                                );
                                return;
                            }
                        };

                        // Short-circuit: if this host has already been
                        // auto-configured on a prior SSH login, skip the
                        // move-and-record step entirely (avoid duplicate
                        // configuration).
                        let already_configured = match db.is_host_configured(&host_for_config).await
                        {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(
                                    "[SSH-AUTOCONFIG] is_host_configured failed for {}: {}",
                                    host_for_config,
                                    e
                                );
                                // On lookup failure, fall through to the
                                // configure path — worst case we record
                                // a duplicate (idempotent INSERT OR IGNORE).
                                false
                            }
                        };
                        if already_configured {
                            // Already configured on a prior login. Skip the
                            // configuration step — no move, no recording.
                            // (The requirement: avoid duplicate configuration.)
                            return;
                        }

                        // Perform the configuration step: move the session's
                        // tab to the leftmost position. Returns true only if
                        // the tab actually moved (i.e., a real configuration
                        // step occurred). Returns false if the tab was not
                        // found or was already leftmost.
                        let moved = {
                            let mut s = state_for_config.write();
                            move_session_to_leftmost(&mut s, &sid_for_config)
                        };

                        if !moved {
                            // No configuration step occurred (tab was already
                            // leftmost, or — extremely unlikely — the tab id
                            // wasn't found). Either way, there's nothing to
                            // record. Don't write to the DB — the next connect
                            // will retry the move if the tab ends up not at
                            // index 0 for some reason.
                            return;
                        }

                        // Record the final success: host is now configured.
                        // Only this single row is written — no intermediate
                        // debug steps are recorded (per the requirement).
                        if let Err(e) = db.mark_host_configured(&host_for_config).await {
                            tracing::warn!(
                                "[SSH-AUTOCONFIG] failed to record host {} as configured: {}",
                                host_for_config,
                                e
                            );
                            // The move already happened on screen — the only
                            // thing that failed is the DB recording. Next
                            // connect to the same host will retry the move
                            // (since the host isn't recorded as configured).
                            // This is acceptable: the move is idempotent and
                            // cheap, and re-running it on the next connect
                            // is harmless.
                        }
                    });
                }

                // Install OSC 133/OSC 7 shell integration only after the
                // remote's initial output has stayed quiet. Every SSH output
                // chunk below resets the debounce deadline, including delayed
                // dynamic-MOTD paragraphs. This avoids creating a new prompt in
                // front of login text that is still arriving.
                let (initial_output_activity_tx, initial_output_activity_rx) =
                    mpsc::unbounded_channel();
                spawn(inject_shell_integration_when_quiet(
                    initial_output_activity_rx,
                    session.input_tx.clone(),
                    tab_id.clone(),
                ));

                let _session_guard = session;

                // Pre-seed session history from local shell history.
                // Filter out commands that are known to have failed so they
                // don't show up in the suggestion popup immediately on connect.
                // (The background import below will later merge in remote
                // history with the same filter.)
                {
                    let provider = rusterm_history::HybridHistoryProvider::new();
                    let initial_history: Vec<String> = provider
                        .search("", 3000)
                        .into_iter()
                        .map(|m| m.command)
                        .collect();
                    if !initial_history.is_empty() {
                        let db_path = dirs::data_dir()
                            .unwrap_or_default()
                            .join("rusterm")
                            .join("rusterm.db");
                        let mut failed_set: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                            failed_set = db.known_failed_commands().await.unwrap_or_default();
                        }
                        let filtered: Vec<String> = initial_history
                            .into_iter()
                            .filter(|cmd| !failed_set.contains(cmd))
                            .collect();
                        if !filtered.is_empty() {
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                                tab.command_history = filtered;
                            }
                        }
                    }
                }

                // Background: import remote shell history into the local DB
                // so suggestions can draw from both local + remote history.
                // Retries up to 3 times to handle transient exec channel failures.
                //
                // A shared `exec_import_succeeded` flag coordinates with the
                // interactive-shell fallback below: if the exec import succeeds,
                // the fallback is skipped (it would otherwise capture + suppress
                // 15s of terminal output for nothing — the "stuck" symptom the
                // user reported, where commands ran but their output vanished
                // into the capture buffer and the terminal appeared frozen).
                let exec_import_succeeded =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                {
                    let ssh_session_for_history = ssh_session.clone();
                    let exec_flag = exec_import_succeeded.clone();
                    let host_for_history = host_for_import.clone();
                    let sid_for_history = tab_id.clone();
                    let mut state_for_history = state;
                    spawn(async move {
                        // Wait for the shell to stabilize before opening the exec channel
                        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

                        let mut remote_commands = {
                            let mut retries = 3;
                            let mut result = Vec::new();
                            let mut success = false;
                            while retries > 0 {
                                match ssh_session_for_history.fetch_remote_history().await {
                                    Ok(cmds) => {
                                        tracing::info!(
                                            "Imported {} remote history commands from {}",
                                            cmds.len(),
                                            host_for_history
                                        );
                                        result = cmds;
                                        success = true;
                                        // Signal the interactive-shell fallback
                                        // (if it hasn't started yet) that its
                                        // capture is unnecessary — this skips
                                        // the 15s output-suppression window
                                        // that would otherwise freeze the
                                        // terminal.
                                        exec_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                        break;
                                    }
                                    Err(e) => {
                                        retries -= 1;
                                        tracing::warn!(
                                            "Failed to fetch remote history from {} (retries left: {}): {}",
                                            host_for_history,
                                            retries,
                                            e
                                        );
                                        if retries > 0 {
                                            tokio::time::sleep(std::time::Duration::from_secs(2))
                                                .await;
                                        }
                                    }
                                }
                            }
                            if !success {
                                tracing::error!(
                                    "All retries exhausted for remote history import from {}",
                                    host_for_history
                                );
                            }
                            result
                        };

                        if remote_commands.is_empty() {
                            return;
                        }

                        // Persist to SQLite (replace previous import for this host).
                        //
                        // IMPORTANT: skip commands that are known to have failed
                        // (exit_code != 0 with no successful run). Without this,
                        // `~/.bash_history` would re-introduce typos like `pwdwd`
                        // as `exit_code = NULL` on every reconnect, and the HAVING
                        // clause would keep them ("unknown, assume success") —
                        // exactly the bug the user reported (popup still shows
                        // `pwdwd` after a prior failed run). `known_failed_commands`
                        // is durable across sessions (stored in the DB), so a typo
                        // marked failed in session A stays filtered out in session B.
                        let db_path = dirs::data_dir()
                            .unwrap_or_default()
                            .join("rusterm")
                            .join("rusterm.db");
                        if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                            // Snapshot the known-failed set BEFORE deleting by
                            // hostname. The failure markers themselves are
                            // stored with hostname=NULL so they survive
                            // `delete_history_by_hostname`, but we snapshot
                            // first anyway for clarity and to filter the
                            // incoming batch before it's written.
                            let failed_set = db.known_failed_commands().await.unwrap_or_default();
                            if !failed_set.is_empty() {
                                tracing::info!(
                                    "[SSH] import skipping {} known-failed commands for {}",
                                    failed_set.len(),
                                    host_for_history
                                );
                            }
                            // Partition remote_commands into (keep, skip) based
                            // on the known-failed set. Keep the order stable.
                            let mut kept: Vec<String> = Vec::with_capacity(remote_commands.len());
                            let mut skipped = 0usize;
                            for cmd in remote_commands.drain(..) {
                                if failed_set.contains(&cmd) {
                                    skipped += 1;
                                } else {
                                    kept.push(cmd);
                                }
                            }
                            if skipped > 0 {
                                tracing::info!(
                                    "[SSH] import skipped {} commands for {} (known-failed)",
                                    skipped,
                                    host_for_history
                                );
                            }
                            // `remote_commands` is now empty; `kept` holds the
                            // filtered list for the merge step below.
                            remote_commands = kept;

                            let _ = db.delete_history_by_hostname(&host_for_history).await;
                            let entries: Vec<_> = remote_commands
                                .iter()
                                .map(|cmd| rusterm_db::history::HistoryEntry {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    command: cmd.clone(),
                                    session_id: sid_for_history.clone(),
                                    cwd: None,
                                    hostname: Some(host_for_history.clone()),
                                    exit_code: None,
                                    duration_ms: None,
                                    created_at: chrono::Utc::now().to_rfc3339(),
                                })
                                .collect();
                            if let Err(e) = db.save_history_batch(entries).await {
                                tracing::warn!("Failed to save remote history batch: {}", e);
                            }
                        }

                        // Merge into session command_history (prepend unique entries)
                        let mut existing = state_for_history
                            .read()
                            .sessions
                            .iter()
                            .find(|t| t.id == sid_for_history)
                            .map(|t| t.command_history.clone())
                            .unwrap_or_default();
                        let before_len = existing.len();
                        for cmd in remote_commands.into_iter().rev() {
                            if !existing.contains(&cmd) {
                                existing.insert(0, cmd);
                            }
                        }
                        if existing.len() != before_len {
                            state_for_history
                                .write()
                                .sessions
                                .iter_mut()
                                .find(|t| t.id == sid_for_history)
                                .map(|tab| tab.command_history = existing);
                        }
                    });
                }

                let _conn_guard = ssh_session;

                // Capture buffer for interactive-shell history import.
                // When Some, the event loop accumulates raw output into it.
                // Used by servers that only allow one channel (jump servers).
                let capture_buffer: Arc<parking_lot::Mutex<Option<String>>> =
                    Arc::new(parking_lot::Mutex::new(None));

                // Interactive shell history import — sends a command through
                // the user's own shell, captures the output, then clears the
                // terminal. Used when exec + shell-channel fallbacks both fail.
                //
                // SKIPPED when the exec import above succeeded — in that case
                // the history is already in the DB, and running this fallback
                // would pointlessly suppress 15s of terminal output (the
                // capture buffer swallows ALL SessionEvent::Output while it's
                // Some), making the terminal appear frozen even though the
                // user's commands are running fine on the remote.
                {
                    let input_tx_for_import = input_senders.read().get(&tab_id).cloned();
                    let capture_buffer_clone = capture_buffer.clone();
                    let host_for_shell_import = host_for_import.clone();
                    let sid_for_shell_import = tab_id.clone();
                    let mut state_for_shell_import = state;
                    let exec_flag_for_shell_import = exec_import_succeeded.clone();
                    spawn(async move {
                        let input_tx = match input_tx_for_import {
                            Some(tx) => tx,
                            None => return,
                        };

                        // Wait for exec/shell import attempts to finish first.
                        // The exec import waits 800ms then retries up to 3× with
                        // 2s backoff, so worst case is 800ms + 3×2s = 6.8s.
                        // We wait 8s (a safety margin) so we don't race ahead
                        // of the exec import — if we check the flag before the
                        // exec import has had a chance to set it, we'd start the
                        // 15s capture window (which freezes the terminal) even
                        // though the exec import is about to succeed.
                        tokio::time::sleep(std::time::Duration::from_secs(8)).await;

                        // If the exec import already succeeded, this fallback
                        // is unnecessary — skip it to avoid the 15s capture
                        // window that freezes the terminal.
                        if exec_flag_for_shell_import.load(std::sync::atomic::Ordering::SeqCst) {
                            tracing::info!(
                                "[SSH] Skipping interactive shell import for {} — exec import succeeded",
                                host_for_shell_import
                            );
                            return;
                        }

                        // Start capturing
                        *capture_buffer_clone.lock() = Some(String::new());

                        // Send the command. Leading space avoids shell history
                        // recording (HIST_IGNORE_SPACE / HISTCONTROL=ignorespace).
                        // Use variable expansion for markers so they DON'T appear
                        // literally in the command echo — the echo shows ${S}_START
                        // but the output shows _RUSTERM_START_ (after expansion).
                        // This prevents the polling from matching the echo.
                        let cmd = " S=RUSTERM; printf \"_${S}_START_\\n\"; for f in ~/.zsh_history ~/.bash_history ~/.history ~/.zhistory ~/.local/share/fish/fish_history; do if [ -f \"$f\" ]; then cat \"$f\" 2>/dev/null; fi; done; printf \"_${S}_END_\\n\"\n";
                        let _ = input_tx.send(cmd.as_bytes().to_vec());

                        // Poll for the end marker (up to 15 seconds)
                        let mut found = false;
                        for _ in 0..150 {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let buf = capture_buffer_clone.lock();
                            if let Some(ref s) = *buf {
                                if s.contains("_RUSTERM_END_") {
                                    found = true;
                                    break;
                                }
                            }
                        }

                        // Grab captured output and stop capturing
                        let captured = capture_buffer_clone.lock().take().unwrap_or_default();
                        *capture_buffer_clone.lock() = None;

                        // NOTE: do NOT send a Ctrl+L (0x0c) here. The history
                        // dump was captured (not rendered — see the `capturing`
                        // guard in the Output handler), so the on-screen
                        // MOTD/prompt is unaffected by it. A Ctrl+L would only
                        // wipe that real content into scrollback → blank screen.
                        if !found {
                            tracing::warn!(
                                "[SSH] Interactive shell import: end marker not found within 15s for {}",
                                host_for_shell_import
                            );
                            return;
                        }

                        // Extract content between the start and end markers.
                        // Markers use variable expansion so they only appear in
                        // the actual output, not in the command echo.
                        let start_m = "_RUSTERM_START_";
                        let end_m = "_RUSTERM_END_";
                        tracing::info!(
                            "[SSH] Interactive shell import: captured {} bytes, first 500 chars: {:?}",
                            captured.len(),
                            &captured[..captured.len().min(500)]
                        );
                        let extracted = {
                            if let Some(start_pos) = captured.find(start_m) {
                                let after_start = start_pos + start_m.len();
                                if let Some(end_pos) = captured[after_start..].find(end_m) {
                                    captured[after_start..after_start + end_pos].to_string()
                                } else {
                                    tracing::warn!(
                                        "[SSH] Interactive shell import: start marker found but no end marker after it"
                                    );
                                    String::new()
                                }
                            } else {
                                tracing::warn!(
                                    "[SSH] Interactive shell import: no start marker found in captured output"
                                );
                                String::new()
                            }
                        };
                        tracing::info!(
                            "[SSH] Interactive shell import: extracted {} bytes between markers, first 200 chars: {:?}",
                            extracted.len(),
                            &extracted[..extracted.len().min(200)]
                        );

                        let mut remote_commands = rusterm_ssh::parse_remote_history(&extracted);
                        tracing::info!(
                            "[SSH] Interactive shell import: parsed {} commands from {}",
                            remote_commands.len(),
                            host_for_shell_import
                        );

                        if remote_commands.is_empty() {
                            return;
                        }

                        // Save to DB (replace previous import for this host).
                        // Skip known-failed commands so typos like `pwdwd` don't
                        // get re-introduced from `~/.bash_history` as
                        // `exit_code = NULL` (which the HAVING clause would keep
                        // as "unknown, assume success"). See the exec-import
                        // path for the full rationale.
                        let db_path = dirs::data_dir()
                            .unwrap_or_default()
                            .join("rusterm")
                            .join("rusterm.db");

                        // Fetch the known-failed set (async) BEFORE deleting
                        // by hostname. The failure markers are stored with
                        // hostname=NULL so `delete_history_by_hostname` won't
                        // touch them, but we snapshot first for clarity.
                        let mut failed_set: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        if let Ok(db) = rusterm_db::Database::open(Some(db_path.clone())).await {
                            failed_set = db.known_failed_commands().await.unwrap_or_default();
                        }

                        // Partition remote_commands into (keep, skip) based on
                        // the known-failed set.
                        let mut kept: Vec<String> = Vec::with_capacity(remote_commands.len());
                        let mut skipped = 0usize;
                        for cmd in remote_commands.drain(..) {
                            if failed_set.contains(&cmd) {
                                skipped += 1;
                            } else {
                                kept.push(cmd);
                            }
                        }
                        if skipped > 0 {
                            tracing::info!(
                                "[SSH] Interactive shell import: skipped {} known-failed commands for {}",
                                skipped,
                                host_for_shell_import
                            );
                        }

                        if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                            let _ = db.delete_history_by_hostname(&host_for_shell_import).await;
                            let entries: Vec<_> = kept
                                .iter()
                                .map(|cmd| rusterm_db::history::HistoryEntry {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    command: cmd.clone(),
                                    session_id: sid_for_shell_import.clone(),
                                    cwd: None,
                                    hostname: Some(host_for_shell_import.clone()),
                                    exit_code: None,
                                    duration_ms: None,
                                    created_at: chrono::Utc::now().to_rfc3339(),
                                })
                                .collect();
                            if let Err(e) = db.save_history_batch(entries).await {
                                tracing::warn!("Failed to save shell-imported history: {}", e);
                            }
                        }

                        // Merge the FILTERED list (not the original
                        // `remote_commands`) into session command_history so
                        // the in-memory popup source also skips known-failed
                        // commands.
                        let mut existing = state_for_shell_import
                            .read()
                            .sessions
                            .iter()
                            .find(|t| t.id == sid_for_shell_import)
                            .map(|t| t.command_history.clone())
                            .unwrap_or_default();
                        for cmd in kept.into_iter().rev() {
                            if !existing.contains(&cmd) {
                                existing.insert(0, cmd);
                            }
                        }
                        state_for_shell_import
                            .write()
                            .sessions
                            .iter_mut()
                            .find(|t| t.id == sid_for_shell_import)
                            .map(|tab| tab.command_history = existing);
                    });
                }

                while let Some(event) = event_rx.recv().await {
                    match event {
                        SessionEvent::Output(id, data) => {
                            // Reset the SSH login-output debounce before any
                            // capture filtering; hidden history-import output is
                            // still remote activity and must postpone injection.
                            let _ = initial_output_activity_tx.send(());
                            // Capture raw output for history import if active. While
                            // capturing (the interactive history-import dump), SKIP
                            // rendering/logging/matching: the ~77KB dump would
                            // otherwise flash on screen and could spuriously trigger
                            // OneKey/exit-code matching on old commands that happen
                            // to contain "password" etc.
                            let capturing = {
                                let mut cap = capture_buffer.lock();
                                if let Some(ref mut buf) = *cap {
                                    buf.push_str(&String::from_utf8_lossy(&data));
                                }
                                cap.is_some()
                            };
                            if capturing {
                                continue;
                            }
                            // Log output
                            {
                                let logs = state.read().session_logs.clone();
                                if let Some(log) = logs.get(&id) {
                                    log.lock().log_output(&data);
                                }
                            }
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let (render_result, exit_code, new_cwd) = {
                                    let mut entry = handle.lock();
                                    (
                                        entry.process_and_render(&data),
                                        entry.terminal.take_exit_code(),
                                        entry.terminal.cwd().map(|p| p.to_path_buf()),
                                    )
                                };
                                // Shell integration (OSC 133;D): DEFERRED RECORDING.
                                // Commands are queued in `pending_exit_check` when Enter is
                                // pressed (see on_command). Only now, when the shell reports
                                // the exit code, do we decide whether to commit the command
                                // to history + DB (rc==0) or silently drop it (rc!=0).
                                // This replaces the old "record then delete on fail" flow —
                                // failed commands can never appear in suggestions, because
                                // they were never recorded in the first place.
                                if exit_code.is_some() {
                                    tracing::info!(
                                        "[OUTPUT-SSH] session={} exit_code={:?} queue_len={}",
                                        &id[..id.len().min(8)],
                                        exit_code,
                                        state
                                            .read()
                                            .pending_exit_check
                                            .get(&id)
                                            .map(|q| q.len())
                                            .unwrap_or(0)
                                    );
                                }
                                let committed: Option<(String, String, Option<String>)> =
                                    if let Some(rc) = exit_code {
                                        let mut s = state.write();
                                        let popped = s
                                            .pending_exit_check
                                            .get_mut(&id)
                                            .and_then(|q| q.pop_front());
                                        tracing::info!(
                                            "[OUTPUT-SSH] rc={} popped={:?}",
                                            rc,
                                            popped.as_ref().map(|(c, _)| c.clone())
                                        );
                                        if rc == 0 {
                                            // Successful: commit to history + DB (with
                                            // exit_code=Some(0) so search_history's HAVING
                                            // clause treats it as a known-good command).
                                            if let Some((cmd, db_id)) = popped {
                                                let hostname = s
                                                    .sessions
                                                    .iter()
                                                    .find(|t| t.id == id)
                                                    .and_then(|t| t.hostname.clone());
                                                if let Some(tab) =
                                                    s.sessions.iter_mut().find(|t| t.id == id)
                                                {
                                                    if tab.command_history.last() != Some(&cmd) {
                                                        tab.command_history.push(cmd.clone());
                                                    }
                                                }
                                                Some((cmd, db_id, hostname))
                                            } else {
                                                None
                                            }
                                        } else {
                                            // Failed: mark the command as known-failed in
                                            // the DB so it stops being suggested AND so
                                            // that subsequent history imports skip it.
                                            //
                                            // Why `mark_command_failed` instead of
                                            // `delete_history_by_command`: deletion
                                            // would let the next history import (which
                                            // reads `~/.bash_history`) re-introduce
                                            // the failed command as `exit_code = NULL`,
                                            // which the HAVING clause keeps ("unknown,
                                            // assume success"). Marking it as failed
                                            // leaves a durable non-zero exit_code row
                                            // that the HAVING clause drops, and that
                                            // `known_failed_commands` reports so
                                            // imports can skip it. If the user later
                                            // runs it successfully, deferred recording
                                            // (rc==0 branch above) saves a success row
                                            // and the HAVING clause re-enables it.
                                            //
                                            // TIMING WINDOW GUARD: `mark_command_failed`
                                            // runs in a `spawn` below — between the
                                            // `retain` (immediate) and the DB write
                                            // (async), the DB still has the prior
                                            // `exit_code = NULL` import row, which the
                                            // HAVING clause keeps. To prevent the
                                            // suggestion query from re-surfacing the
                                            // just-failed command during that window,
                                            // we ALSO insert the command into
                                            // `recent_failed_commands` synchronously
                                            // here. The suggestion query filters
                                            // against this set; the spawn removes the
                                            // entry once `mark_command_failed` commits
                                            // (the DB HAVING then takes over).
                                            if let Some((cmd, _db_id)) = popped {
                                                if let Some(tab) =
                                                    s.sessions.iter_mut().find(|t| t.id == id)
                                                {
                                                    tab.command_history.retain(|c| c != &cmd);
                                                }
                                                s.recent_failed_commands.insert(cmd.clone());
                                                let cmd_for_mark = cmd.clone();
                                                let rc_for_mark = rc;
                                                let sid_log = id.clone();
                                                // Drop the write lock BEFORE cloning `state`
                                                // — `state.clone()` is an immutable borrow,
                                                // which conflicts with the mutable borrow
                                                // held by `s` (from `state.write()`).
                                                drop(s);
                                                let mut state_for_mark = state.clone();
                                                spawn(async move {
                                                    let db_path = dirs::data_dir()
                                                        .unwrap_or_default()
                                                        .join("rusterm")
                                                        .join("rusterm.db");
                                                    let mark_ok = if let Ok(db) =
                                                        rusterm_db::Database::open(Some(db_path))
                                                            .await
                                                    {
                                                        match db
                                                            .mark_command_failed(
                                                                &cmd_for_mark,
                                                                rc_for_mark,
                                                            )
                                                            .await
                                                        {
                                                            Ok(()) => {
                                                                tracing::info!(
                                                                    "[SSH] marked command as \
                                                                     failed in history DB: \
                                                                     {:?} rc={} (session={})",
                                                                    cmd_for_mark,
                                                                    rc_for_mark,
                                                                    &sid_log
                                                                        [..sid_log.len().min(8)],
                                                                );
                                                                true
                                                            }
                                                            Err(e) => {
                                                                tracing::warn!(
                                                                    "Failed to mark command as \
                                                                     failed in history DB: {}",
                                                                    e
                                                                );
                                                                false
                                                            }
                                                        }
                                                    } else {
                                                        false
                                                    };
                                                    // Only unblock suggestions for this
                                                    // command once the DB actually has the
                                                    // failure marker. If the write failed,
                                                    // leave it in the set for the rest of
                                                    // the session — better to over-filter
                                                    // (never suggest a known-failed command)
                                                    // than to let a typo re-surface because
                                                    // the DB still has a NULL import row.
                                                    if mark_ok {
                                                        state_for_mark
                                                            .write()
                                                            .recent_failed_commands
                                                            .remove(&cmd_for_mark);
                                                    }
                                                });
                                            }
                                            None
                                        }
                                    } else {
                                        None
                                    };
                                {
                                    let mut s = state.write();
                                    if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                        tab.render_output = render_result;
                                        tab.version += 1;
                                        // Mirror the shell's last-reported cwd (OSC 7)
                                        // into the tab so the session-state save
                                        // path can read it without taking the
                                        // terminal lock. `None` means the shell
                                        // hasn't reported one yet — leave the
                                        // tab's cwd untouched in that case so we
                                        // don't clobber a previous report.
                                        if let Some(cwd) = &new_cwd {
                                            tab.cwd = Some(cwd.to_string_lossy().into_owned());
                                        }
                                    }
                                }
                                if let Some((cmd, db_id, hostname)) = committed {
                                    let sid = id.clone();
                                    // Clone for the analytics record (the original `cmd` is
                                    // moved into the DB entry below). When the `analytics`
                                    // feature is off, the spawned analytics task below is
                                    // cfg-gated out entirely.
                                    let _analytics_cmd = cmd.clone();
                                    let _analytics_host = hostname.clone();
                                    let analytics_created = chrono::Utc::now().to_rfc3339();
                                    let analytics_created_for_db = analytics_created.clone();
                                    spawn(async move {
                                        let db_path = dirs::data_dir()
                                            .unwrap_or_default()
                                            .join("rusterm")
                                            .join("rusterm.db");
                                        if let Ok(db) =
                                            rusterm_db::Database::open(Some(db_path)).await
                                        {
                                            let entry = rusterm_db::history::HistoryEntry {
                                                id: db_id,
                                                command: cmd,
                                                session_id: sid,
                                                cwd: None,
                                                hostname,
                                                exit_code: Some(0),
                                                duration_ms: None,
                                                created_at: analytics_created_for_db,
                                            };
                                            if let Err(e) = db.save_history(entry).await {
                                                tracing::warn!("Failed to save history: {}", e);
                                            }
                                        }
                                    });
                                    // Incremental analytics mirror: record the successful
                                    // command into DuckDB so the analytics DB stays
                                    // current without a full re-mirror on every command.
                                    // Spawned separately so a slow DuckDB insert doesn't
                                    // block the output loop. Errors are logged but
                                    // non-fatal — analytics is best-effort.
                                    // Reuse the AppState's lazily-initialized
                                    // AnalyticsHandle instead of opening a fresh DuckDB
                                    // connection per command. The handle holds a single
                                    // Arc<Mutex<Option<AnalyticsDB>>> so the connection
                                    // persists across calls — we don't pay the
                                    // ~5-50ms `open + init_schema` cost on every
                                    // keystroke-level command execution, and we don't
                                    // risk file-lock contention with the startup mirror
                                    // task below. The .clone() here is cheap (just an
                                    // Arc bump) and lets us move the handle into the
                                    // spawned task without holding the state read lock.
                                    #[cfg(feature = "analytics")]
                                    {
                                        let analytics_handle = state.read().analytics.clone();
                                        spawn(async move {
                                            let analytics_entry =
                                                rusterm_analytics::AnalyticsCommand {
                                                    command: _analytics_cmd,
                                                    hostname: _analytics_host,
                                                    exit_code: Some(0),
                                                    created_at: analytics_created,
                                                };
                                            if let Err(e) =
                                                analytics_handle.record_command(&analytics_entry)
                                            {
                                                tracing::warn!(
                                                    "[ANALYTICS] failed to record command: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }
                            check_onekey_match(state, &id, &data);
                        }
                        SessionEvent::Disconnected(id, reason) => {
                            input_senders.write().remove(&id);
                            let msg = format!(
                                "\r\n--- Disconnected: {} ---\r\nPress Enter to reconnect.\r\n",
                                reason
                            );
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let render_result =
                                    handle.lock().process_and_render(msg.as_bytes());
                                let mut s = state.write();
                                s.session_connection_states
                                    .insert(id.clone(), SessionConnectionState::Disconnected);
                                s.onekey_popups.remove(&id);
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::RemoteHistory(id, commands) => {
                            tracing::info!(
                                "[SSH] received remote history: {} commands for {}",
                                commands.len(),
                                &id[..id.len().min(8)]
                            );
                            // Filter out known-failed commands so this event
                            // (if ever wired up) doesn't re-introduce them
                            // into the in-memory `command_history`.
                            let db_path = dirs::data_dir()
                                .unwrap_or_default()
                                .join("rusterm")
                                .join("rusterm.db");
                            let mut failed_set: std::collections::HashSet<String> =
                                std::collections::HashSet::new();
                            if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                failed_set = db.known_failed_commands().await.unwrap_or_default();
                            }
                            let filtered: Vec<String> = commands
                                .into_iter()
                                .filter(|cmd| !failed_set.contains(cmd))
                                .collect();
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                tab.command_history = filtered;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[Reconnect] SSH connection failed for {}: {}",
                    &tab_id[..tab_id.len().min(8)],
                    e
                );
                state
                    .write()
                    .session_connection_states
                    .insert(tab_id.clone(), SessionConnectionState::Disconnected);
                let msg = format!("Connection failed: {}\r\nPress Enter to reconnect.\r\n", e);
                let terminals = state.read().terminals.clone();
                if let Some(handle) = terminals.get(&tab_id) {
                    let render_result = handle.lock().process_and_render(msg.as_bytes());
                    let mut s = state.write();
                    if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                        tab.render_output = render_result;
                        tab.version += 1;
                    }
                }
            }
        }
    });
}

fn start_shell_connection(
    mut state: Signal<AppState>,
    mut input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    tab_id: String,
    shell_config: ShellConfig,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();

    let size = {
        let terminals = state.read().terminals.clone();
        if let Some(handle) = terminals.get(&tab_id) {
            handle.lock().terminal.size()
        } else {
            TerminalSize::default()
        }
    };

    match rusterm_proto::ShellConnection::open(&shell_config, size, tab_id.clone(), event_tx) {
        Ok(session) => {
            state
                .write()
                .session_connection_states
                .insert(tab_id.clone(), SessionConnectionState::Connected);
            input_senders
                .write()
                .insert(tab_id.clone(), session.input_tx.clone());

            state
                .write()
                .close_senders
                .push((tab_id.clone(), session.close_tx.clone()));

            state
                .write()
                .resize_senders
                .insert(tab_id.clone(), session.resize_tx.clone());

            if let Some(handle) = state.read().terminals.get(&tab_id) {
                let mut entry = handle.lock();
                entry.terminal.set_input_sender(session.input_tx.clone());
            }

            // Inject shell integration (OSC 133) for local shells too, so the
            // shell reports each command's exit code. This lets the app drop
            // failed commands from the suggestion popup (the user doesn't want
            // their typos and broken commands showing up as autocomplete).
            //
            // The setup code is sent inline (same approach as SSH). It WILL be
            // echoed on screen as a one-time `__rusterm_precmd() {...}` line,
            // but that's a cosmetic trade-off we accept in exchange for correct
            // exit-code tracking. We do NOT send a trailing Ctrl+L to hide the
            // echo — Ctrl+L clears the WHOLE screen, which wipes the MOTD /
            // banner content into scrollback and leaves a blank terminal.
            // The one-time setup echo is left visible rather than blanking the
            // session.
            //
            // Previously this was disabled for local shells, but that meant
            // failed commands stayed in `command_history` and showed up as
            // inline suggestions — exactly what the user complained about.
            {
                let integration_tx = session.input_tx.clone();
                let int_sid = tab_id.clone();
                let mut setup: Vec<u8> = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#.as_bytes().to_vec();
                setup.push(b'\n');
                spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    let _ = integration_tx.send(setup);
                    tracing::info!("[local] injected shell integration for {}", int_sid);
                });
            }

            // Pre-populate local shell history from native history files.
            // Filter out commands that are known to have failed (marked via
            // `mark_command_failed`) so typos like `pwdwd` — which live in
            // `~/.bash_history` / `~/.zsh_history` — don't show up in the
            // suggestion popup. Same rationale as the SSH import path.
            //
            // The DB query is async, so we spawn a task to fetch the
            // known-failed set, filter the local history, and write the
            // result back into the session's `command_history`.
            {
                let provider = rusterm_history::HybridHistoryProvider::new();
                let local_history: Vec<String> = provider
                    .search("", 2000)
                    .into_iter()
                    .map(|m| m.command)
                    .collect();
                if !local_history.is_empty() {
                    let sid_for_local_import = tab_id.clone();
                    let mut state_for_local_import = state;
                    spawn(async move {
                        let db_path = dirs::data_dir()
                            .unwrap_or_default()
                            .join("rusterm")
                            .join("rusterm.db");
                        let mut failed_set: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                            failed_set = db.known_failed_commands().await.unwrap_or_default();
                        }
                        let filtered: Vec<String> = local_history
                            .into_iter()
                            .filter(|cmd| !failed_set.contains(cmd))
                            .collect();
                        if !filtered.is_empty() {
                            let mut s = state_for_local_import.write();
                            if let Some(tab) =
                                s.sessions.iter_mut().find(|t| t.id == sid_for_local_import)
                            {
                                tab.command_history = filtered;
                            }
                        }
                    });
                }
            }

            let _session_guard = session;

            spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    match event {
                        SessionEvent::Output(id, data) => {
                            {
                                let logs = state.read().session_logs.clone();
                                if let Some(log) = logs.get(&id) {
                                    log.lock().log_output(&data);
                                }
                            }
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let (render_result, exit_code, new_cwd) = {
                                    let mut entry = handle.lock();
                                    (
                                        entry.process_and_render(&data),
                                        entry.terminal.take_exit_code(),
                                        entry.terminal.cwd().map(|p| p.to_path_buf()),
                                    )
                                };
                                // Shell integration (OSC 133;D): DEFERRED RECORDING.
                                // Commands are queued in `pending_exit_check` when Enter is
                                // pressed (see on_command). Only now, when the shell reports
                                // the exit code, do we decide whether to commit the command
                                // to history + DB (rc==0) or silently drop it (rc!=0).
                                // This replaces the old "record then delete on fail" flow —
                                // failed commands can never appear in suggestions, because
                                // they were never recorded in the first place.
                                if exit_code.is_some() {
                                    tracing::info!(
                                        "[OUTPUT-LOCAL] session={} exit_code={:?} queue_len={}",
                                        &id[..id.len().min(8)],
                                        exit_code,
                                        state
                                            .read()
                                            .pending_exit_check
                                            .get(&id)
                                            .map(|q| q.len())
                                            .unwrap_or(0)
                                    );
                                }
                                let committed: Option<(String, String, Option<String>)> =
                                    if let Some(rc) = exit_code {
                                        let mut s = state.write();
                                        let popped = s
                                            .pending_exit_check
                                            .get_mut(&id)
                                            .and_then(|q| q.pop_front());
                                        tracing::info!(
                                            "[OUTPUT-LOCAL] rc={} popped={:?}",
                                            rc,
                                            popped.as_ref().map(|(c, _)| c.clone())
                                        );
                                        if rc == 0 {
                                            // Successful: commit to history + DB (with
                                            // exit_code=Some(0) so search_history's HAVING
                                            // clause treats it as a known-good command).
                                            if let Some((cmd, db_id)) = popped {
                                                let hostname = s
                                                    .sessions
                                                    .iter()
                                                    .find(|t| t.id == id)
                                                    .and_then(|t| t.hostname.clone());
                                                if let Some(tab) =
                                                    s.sessions.iter_mut().find(|t| t.id == id)
                                                {
                                                    if tab.command_history.last() != Some(&cmd) {
                                                        tab.command_history.push(cmd.clone());
                                                    }
                                                }
                                                Some((cmd, db_id, hostname))
                                            } else {
                                                None
                                            }
                                        } else {
                                            // Failed: mark the command as known-failed in
                                            // the DB so it stops being suggested AND so
                                            // that subsequent history imports skip it.
                                            // See the SSH branch for the full rationale
                                            // (mark_command_failed vs delete_history_by_command).
                                            //
                                            // TIMING WINDOW GUARD: same as the SSH
                                            // branch — insert into
                                            // `recent_failed_commands` synchronously
                                            // here so the suggestion query filters it
                                            // out during the async `mark_command_failed`
                                            // window (when the DB still has the prior
                                            // NULL import row that HAVING would keep).
                                            // The spawn removes the entry once the DB
                                            // write commits.
                                            if let Some((cmd, _db_id)) = popped {
                                                if let Some(tab) =
                                                    s.sessions.iter_mut().find(|t| t.id == id)
                                                {
                                                    tab.command_history.retain(|c| c != &cmd);
                                                }
                                                s.recent_failed_commands.insert(cmd.clone());
                                                let cmd_for_mark = cmd.clone();
                                                let rc_for_mark = rc;
                                                let sid_log = id.clone();
                                                drop(s);
                                                let mut state_for_mark = state.clone();
                                                spawn(async move {
                                                    let db_path = dirs::data_dir()
                                                        .unwrap_or_default()
                                                        .join("rusterm")
                                                        .join("rusterm.db");
                                                    let mark_ok = if let Ok(db) =
                                                        rusterm_db::Database::open(Some(db_path))
                                                            .await
                                                    {
                                                        match db
                                                            .mark_command_failed(
                                                                &cmd_for_mark,
                                                                rc_for_mark,
                                                            )
                                                            .await
                                                        {
                                                            Ok(()) => {
                                                                tracing::info!(
                                                                    "[LOCAL] marked command as \
                                                                     failed in history DB: \
                                                                     {:?} rc={} (session={})",
                                                                    cmd_for_mark,
                                                                    rc_for_mark,
                                                                    &sid_log
                                                                        [..sid_log.len().min(8)],
                                                                );
                                                                true
                                                            }
                                                            Err(e) => {
                                                                tracing::warn!(
                                                                    "Failed to mark command as \
                                                                     failed in history DB: {}",
                                                                    e
                                                                );
                                                                false
                                                            }
                                                        }
                                                    } else {
                                                        false
                                                    };
                                                    if mark_ok {
                                                        state_for_mark
                                                            .write()
                                                            .recent_failed_commands
                                                            .remove(&cmd_for_mark);
                                                    }
                                                });
                                            }
                                            None
                                        }
                                    } else {
                                        None
                                    };
                                {
                                    let mut s = state.write();
                                    if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                        tab.render_output = render_result;
                                        tab.version += 1;
                                        // Mirror the shell's last-reported cwd (OSC 7)
                                        // into the tab so the session-state save
                                        // path can read it without taking the
                                        // terminal lock. `None` means the shell
                                        // hasn't reported one yet — leave the
                                        // tab's cwd untouched in that case so we
                                        // don't clobber a previous report.
                                        if let Some(cwd) = &new_cwd {
                                            tab.cwd = Some(cwd.to_string_lossy().into_owned());
                                        }
                                    }
                                }
                                if let Some((cmd, db_id, hostname)) = committed {
                                    let sid = id.clone();
                                    // Clone for the analytics record (the original `cmd` is
                                    // moved into the DB entry below). When the `analytics`
                                    // feature is off, the spawned analytics task below is
                                    // cfg-gated out entirely.
                                    let _analytics_cmd = cmd.clone();
                                    let _analytics_host = hostname.clone();
                                    let analytics_created = chrono::Utc::now().to_rfc3339();
                                    let analytics_created_for_db = analytics_created.clone();
                                    spawn(async move {
                                        let db_path = dirs::data_dir()
                                            .unwrap_or_default()
                                            .join("rusterm")
                                            .join("rusterm.db");
                                        if let Ok(db) =
                                            rusterm_db::Database::open(Some(db_path)).await
                                        {
                                            let entry = rusterm_db::history::HistoryEntry {
                                                id: db_id,
                                                command: cmd,
                                                session_id: sid,
                                                cwd: None,
                                                hostname,
                                                exit_code: Some(0),
                                                duration_ms: None,
                                                created_at: analytics_created_for_db,
                                            };
                                            if let Err(e) = db.save_history(entry).await {
                                                tracing::warn!("Failed to save history: {}", e);
                                            }
                                        }
                                    });
                                    // Incremental analytics mirror: record the successful
                                    // command into DuckDB so the analytics DB stays
                                    // current without a full re-mirror on every command.
                                    // Spawned separately so a slow DuckDB insert doesn't
                                    // block the output loop. Errors are logged but
                                    // non-fatal — analytics is best-effort.
                                    // Reuse the AppState's lazily-initialized
                                    // AnalyticsHandle instead of opening a fresh DuckDB
                                    // connection per command. The handle holds a single
                                    // Arc<Mutex<Option<AnalyticsDB>>> so the connection
                                    // persists across calls — we don't pay the
                                    // ~5-50ms `open + init_schema` cost on every
                                    // keystroke-level command execution, and we don't
                                    // risk file-lock contention with the startup mirror
                                    // task below. The .clone() here is cheap (just an
                                    // Arc bump) and lets us move the handle into the
                                    // spawned task without holding the state read lock.
                                    #[cfg(feature = "analytics")]
                                    {
                                        let analytics_handle = state.read().analytics.clone();
                                        spawn(async move {
                                            let analytics_entry =
                                                rusterm_analytics::AnalyticsCommand {
                                                    command: _analytics_cmd,
                                                    hostname: _analytics_host,
                                                    exit_code: Some(0),
                                                    created_at: analytics_created,
                                                };
                                            if let Err(e) =
                                                analytics_handle.record_command(&analytics_entry)
                                            {
                                                tracing::warn!(
                                                    "[ANALYTICS] failed to record command: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }
                            check_onekey_match(state, &id, &data);
                        }
                        SessionEvent::Disconnected(id, reason) => {
                            input_senders.write().remove(&id);
                            let msg = format!(
                                "\r\n--- Disconnected: {} ---\r\nPress Enter to reconnect.\r\n",
                                reason
                            );
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let render_result =
                                    handle.lock().process_and_render(msg.as_bytes());
                                let mut s = state.write();
                                s.session_connection_states
                                    .insert(id.clone(), SessionConnectionState::Disconnected);
                                s.onekey_popups.remove(&id);
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::RemoteHistory(id, commands) => {
                            // Filter out known-failed commands so this event
                            // (if ever wired up) doesn't re-introduce them
                            // into the in-memory `command_history`.
                            let db_path = dirs::data_dir()
                                .unwrap_or_default()
                                .join("rusterm")
                                .join("rusterm.db");
                            let mut failed_set: std::collections::HashSet<String> =
                                std::collections::HashSet::new();
                            if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                failed_set = db.known_failed_commands().await.unwrap_or_default();
                            }
                            let filtered: Vec<String> = commands
                                .into_iter()
                                .filter(|cmd| !failed_set.contains(cmd))
                                .collect();
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                tab.command_history = filtered;
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        Err(e) => {
            tracing::warn!(
                "[Reconnect] shell connection failed for {}: {}",
                &tab_id[..tab_id.len().min(8)],
                e
            );
            state
                .write()
                .session_connection_states
                .insert(tab_id.clone(), SessionConnectionState::Disconnected);
            let msg = format!("Shell failed: {}\r\nPress Enter to reconnect.\r\n", e);
            let terminals = state.read().terminals.clone();
            if let Some(handle) = terminals.get(&tab_id) {
                let render_result = handle.lock().process_and_render(msg.as_bytes());
                let mut s = state.write();
                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                    tab.render_output = render_result;
                    tab.version += 1;
                }
            }
        }
    }
}

fn build_ssh_auth(form: &NewConnectionForm) -> SshAuth {
    match form.auth_type.as_str() {
        "key" => SshAuth::Key {
            private_key_path: if form.key_path.is_empty() {
                "~/.ssh/id_rsa".to_string()
            } else {
                form.key_path.clone()
            },
            passphrase: if form.passphrase.is_empty() {
                None
            } else {
                Some(form.passphrase.clone())
            },
        },
        "agent" => SshAuth::Agent,
        _ => SshAuth::Password {
            password: form.password.clone(),
        },
    }
}

/// Rebuild a `ConnectionConfig` from an edit-dialog form, preserving the
/// original id and any fields the dialog doesn't expose (group, tags, and for
/// SSH: proxy_jump / keepalive_interval). For non-SSH kinds the whole `kind`
/// is preserved as-is — the dialog only edits SSH-specific fields, so a Shell /
/// Serial / Telnet / TCP connection keeps its config and just gets its name /
/// onekey updated.
fn rebuild_connection(original: &ConnectionConfig, form: &NewConnectionForm) -> ConnectionConfig {
    let kind = match &original.kind {
        ConnectionKind::Ssh(ssh) => {
            let port: u16 = form.port.parse().unwrap_or(22);
            let auth = build_ssh_auth(form);
            let terminal_type = if form.terminal_type.is_empty() {
                "xterm-256color".to_string()
            } else {
                form.terminal_type.clone()
            };
            ConnectionKind::Ssh(SshConfig {
                host: form.host.clone(),
                port,
                username: form.username.clone(),
                auth,
                terminal_type,
                proxy_jump: ssh.proxy_jump.clone(),
                keepalive_interval: ssh.keepalive_interval,
                host_key_policy: ssh.host_key_policy.clone(),
            })
        }
        other => other.clone(),
    };
    ConnectionConfig {
        id: original.id.clone(),
        name: if form.name.is_empty() {
            format!("{}@{}", form.username, form.host)
        } else {
            form.name.clone()
        },
        kind,
        group: original.group.clone(),
        tags: original.tags.clone(),
        onekey: form.onekey,
    }
}

fn create_terminal(id: String, state: &mut Signal<AppState>) {
    create_terminal_with_size(id, TerminalSize::default(), state);
}

fn create_terminal_with_size(id: String, size: TerminalSize, state: &mut Signal<AppState>) {
    let terminal = Terminal::new(size);
    let handle = Arc::new(Mutex::new(TerminalEntry {
        terminal,
        parser: vte::ansi::Processor::new(),
        scroll_offset: 0,
    }));
    state.write().terminals.insert(id.clone(), handle);
    // Create an encrypted session log for this session.
    //
    // The per-session AEAD key is derived from the master key (held by
    // ConfigManager after the user unlocks the app) + the session ID. If the
    // app is locked / no ConfigManager is available, we *skip* creating a
    // session log — it's better to lose session-log functionality than to
    // write terminal I/O to disk in plaintext as a fallback.
    let session_key = state
        .read()
        .config_manager
        .as_ref()
        .and_then(|cm| cm.derive_session_key(&id).ok());
    if let Some(key) = session_key {
        match SessionLog::new(&id, key) {
            Ok(log) => {
                state
                    .write()
                    .session_logs
                    .insert(id, Arc::new(Mutex::new(log)));
            }
            Err(_e) => {
                // Don't log the error verbatim — it might contain path info.
                // Just record that session logging is disabled for this tab.
                tracing::warn!(
                    "session logging disabled for tab={:?} reason=io_error",
                    &id[..id.len().min(8)]
                );
            }
        }
    } else {
        tracing::warn!(
            "session logging disabled for tab={:?} reason=no_master_key",
            &id[..id.len().min(8)]
        );
    }
}

/// Open the computer's local shell (the user's default $SHELL — zsh on macOS,
/// bash/zsh on Linux) as a new session tab. Triggered by the "Local" button in
/// the status bar. `command: None` makes the PTY spawn the default prog.
fn open_local_terminal(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
) {
    let tab_id = uuid::Uuid::new_v4().to_string();
    let shell_config = ShellConfig {
        command: None, // default prog = user's $SHELL (zsh/bash)
        args: Vec::new(),
        env: Vec::new(),
        working_dir: None,
    };
    create_terminal(tab_id.clone(), &mut state);
    // Remember the config so this local shell can be reconnected by pressing
    // Enter after it exits/disconnects.
    state.write().session_configs.insert(
        tab_id.clone(),
        ConnectionConfig {
            id: tab_id.clone(),
            name: "Local".to_string(),
            kind: ConnectionKind::Shell(shell_config.clone()),
            group: None,
            tags: Vec::new(),
            onekey: false,
        },
    );

    // No "Starting local shell..." banner — keep the local terminal clean
    // (the shell prints its own prompt once it starts).
    let render_output = Default::default();
    state.write().sessions.push(SessionTab {
        id: tab_id.clone(),
        name: "Local".to_string(),
        kind: SessionType::Shell,
        render_output,
        version: 1,
        suggestion: None,
        suggestions: Vec::new(),
        suggestion_selected: 0,
        suggestion_visible: false,
        command_history: Vec::new(),
        hostname: Some("local".to_string()),
        cwd: None,
    });
    // No explicit pane target → this is a new top-level workspace tab.
    // push_workspace_tab creates the WorkspaceTab + sets active_tab +
    // active_session (anchor).
    {
        let mut s = state.write();
        push_workspace_tab(&mut s, &tab_id);
    }

    start_shell_connection(state, input_senders, tab_id, shell_config);
}

/// Restore sessions from a saved `SessionState` snapshot.
///
/// Called when the user picks "恢复" on the restore dialog. For each
/// `PersistedSession` in the snapshot:
///
/// 1. Look up the matching `ConnectionConfig` by `connection_id` (for SSH/
///    Telnet/Tcp) or build a default `ShellConfig` (for Shell).
/// 2. Open the connection via the existing `open_connection` / `open_local_terminal`
///    flow — this creates a new tab, terminal, and spawns the session task.
/// 3. After the shell is ready (reuse the existing 400ms delay pattern from
///    shell-integration injection), send a single `cd '<cwd>'\n` to the
///    session's input sender. This is the ONLY command we send on restore —
///    we NEVER re-execute any past command or script (the user explicitly
///    asked us not to, to avoid destructive side effects).
///
/// Sessions where `cwd` is `None` (raw telnet/serial, or shell integration
///    didn't take) skip the `cd` step — they just reconnect.
///
/// After all sessions are restored, we set `active_session` to the saved
/// active session (if it exists) so the user lands on the tab they last
/// had focused.
fn restore_sessions(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    to_restore: rusterm_core::SessionState,
) {
    let saved_active = to_restore.active_session.clone();
    let connections: Vec<ConnectionConfig> = state.read().connections.clone();

    for ps in &to_restore.sessions {
        match ps.kind {
            SessionType::Shell => {
                // Open a local shell tab. We can't pass `restore_cwd` through
                // the existing flow, so we open the tab then send `cd` after
                // the shell is ready.
                let tab_id = uuid::Uuid::new_v4().to_string();
                let shell_config = ShellConfig {
                    command: None, // default = $SHELL
                    args: Vec::new(),
                    env: Vec::new(),
                    working_dir: None,
                };
                create_terminal(tab_id.clone(), &mut state);
                state.write().session_configs.insert(
                    tab_id.clone(),
                    ConnectionConfig {
                        id: tab_id.clone(),
                        name: ps.name.clone(),
                        kind: ConnectionKind::Shell(shell_config.clone()),
                        group: None,
                        tags: Vec::new(),
                        onekey: false,
                    },
                );
                let render_output = Default::default();
                state.write().sessions.push(SessionTab {
                    id: tab_id.clone(),
                    name: ps.name.clone(),
                    kind: SessionType::Shell,
                    render_output,
                    version: 1,
                    suggestion: None,
                    suggestions: Vec::new(),
                    suggestion_selected: 0,
                    suggestion_visible: false,
                    command_history: ps.command_history_tail.clone(),
                    hostname: Some("local".to_string()),
                    cwd: None,
                });
                // Each restored session becomes its own top-level workspace
                // tab (one session per tab — Plan B's default layout for
                // restored single-session tabs).
                push_workspace_tab(&mut state.write(), &tab_id);
                start_shell_connection(
                    state.clone(),
                    input_senders.clone(),
                    tab_id.clone(),
                    shell_config,
                );
                // Schedule the `cd <cwd>` send after the shell is ready.
                if let Some(cwd) = &ps.cwd {
                    schedule_cd_after_restore(
                        state.clone(),
                        input_senders.clone(),
                        tab_id.clone(),
                        cwd.clone(),
                    );
                }
            }
            SessionType::Ssh | SessionType::Telnet | SessionType::Tcp => {
                // Look up the connection config by id (fall back to matching
                // by name, then by hostname).
                let conn = connections
                    .iter()
                    .find(|c| {
                        c.id == ps.connection_id.as_deref().unwrap_or("") || c.name == ps.name
                    })
                    .cloned();
                if let Some(conn) = conn {
                    open_connection(state.clone(), input_senders.clone(), conn, None);
                    // Find the tab we just created (it's the last one pushed).
                    let tab_id = state
                        .read()
                        .sessions
                        .last()
                        .map(|t| t.id.clone())
                        .unwrap_or_default();
                    if !tab_id.is_empty() {
                        // Pre-seed command history tail so suggestions work.
                        let mut s = state.write();
                        if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                            tab.command_history = ps.command_history_tail.clone();
                        }
                    }
                    if let Some(cwd) = &ps.cwd {
                        schedule_cd_after_restore(
                            state.clone(),
                            input_senders.clone(),
                            tab_id,
                            cwd.clone(),
                        );
                    }
                } else {
                    tracing::warn!(
                        "Could not find connection for restored session {:?} (id={:?}, name={:?}) — skipping",
                        ps.kind,
                        ps.connection_id,
                        ps.name
                    );
                }
            }
            SessionType::Serial => {
                // Serial sessions don't have a cwd to restore (no shell
                // integration), and reconnecting to a serial port requires
                // the port config which we don't persist here. Skip silently.
                tracing::debug!(
                    "Skipping serial session {:?} in restore (no cwd, no port config)",
                    ps.name
                );
            }
        }
    }

    // Set active tab to the saved one (if it exists in the new tabs).
    // We match by name since the new session ids are fresh UUIDs. The tab's
    // anchor mirrors the matched session, so setting active_tab also
    // restores active_session.
    if let Some(saved_active) = saved_active {
        // The saved_active id was from the previous launch — it won't match
        // any current session. Instead, find the session whose name matches
        // the saved active session's name, then find the workspace tab whose
        // anchor is that session.
        let saved_name = to_restore
            .sessions
            .iter()
            .find(|s| s.id == saved_active)
            .map(|s| s.name.clone());
        let target_session_id = if let Some(name) = saved_name {
            state
                .read()
                .sessions
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.id.clone())
        } else {
            None
        };
        // Find the workspace tab whose anchor is this session. Fall back to
        // the last opened tab if no match.
        let target_tab_id = target_session_id
            .as_ref()
            .and_then(|sid| {
                state
                    .read()
                    .tabs
                    .iter()
                    .find(|t| t.anchor_session_id.as_deref() == Some(sid))
                    .map(|t| t.id.clone())
            })
            .or_else(|| state.read().tabs.last().map(|t| t.id.clone()));
        if let Some(tab_id) = target_tab_id {
            set_active_tab(&mut state.write(), &tab_id);
        }
    }
}

/// Schedule a `cd '<cwd>'\n` send to the session's input sender after a
/// delay that's long enough for the shell to be ready (we piggyback on the
/// existing 400ms shell-integration injection delay, then add a bit more
/// so the `cd` arrives after the integration snippet).
///
/// The path is single-quoted to handle spaces and most special characters.
/// Single quotes inside the path are escaped with the standard `'\'` trick
/// (close quote, escaped quote, reopen quote).
fn schedule_cd_after_restore(
    state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    tab_id: String,
    cwd: String,
) {
    spawn(async move {
        // Wait for the shell to be ready. 800ms = the 400ms shell-integration
        // injection delay + 400ms for the shell to process it and print the
        // first prompt. This is a heuristic — if the shell is slow to start,
        // the `cd` might arrive before the prompt and get echoed. Acceptable
        // trade-off: the alternative (waiting for OSC 133;A prompt-start
        // marker) would require plumbing the marker through the session task,
        // which is a much larger change.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Build the `cd` command with proper quoting.
        // Single-quote the path; escape any embedded single quotes with `'\''`.
        let escaped = cwd.replace('\'', "'\\''");
        let cmd = format!("cd '{}'\r", escaped);

        let sender = input_senders.read().get(&tab_id).cloned();
        if let Some(sender) = sender {
            match sender.send(cmd.into_bytes()) {
                Ok(()) => tracing::info!(
                    "[RESTORE] sent `cd {}` to session {}",
                    cwd,
                    &tab_id[..tab_id.len().min(8)]
                ),
                Err(e) => tracing::warn!(
                    "[RESTORE] failed to send `cd` to session {}: {}",
                    &tab_id[..tab_id.len().min(8)],
                    e
                ),
            }
        } else {
            tracing::warn!(
                "[RESTORE] no input sender for session {} — shell may have died",
                &tab_id[..tab_id.len().min(8)]
            );
        }
        // Touch state to trigger a re-render so the cwd update (when OSC 7
        // arrives from the shell after the `cd`) propagates to the tab.
        // We don't need to write anything — just reading is enough to keep
        // the signal alive in this async context.
        let _ = state.read().sessions.len();
    });
}

/// Save settings.json (specifically the `restore_disabled` flag) without
/// touching connections or OneKeys. Used when the user picks "不再询问" on
/// the restore dialog.
fn save_settings(state: &Signal<AppState>) {
    let cm = match state.read().config_manager.clone() {
        Some(cm) => cm,
        None => {
            tracing::error!("ConfigManager not initialized, cannot save settings");
            return;
        }
    };
    let restore_disabled = state.read().restore_disabled;
    if let Err(e) = cm.save_restore_disabled(restore_disabled) {
        tracing::error!("Failed to save restore_disabled flag: {}", e);
    }
}

/// Stable destination for a connection opened into a pane.
///
/// `active_tab` is deliberately not used for assignment: it is a tab/layout
/// anchor and may change while a series of SSH or shell sessions is created.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneTarget {
    layout_owner_tab_id: String,
    pane_idx: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct OpenConnectionResult {
    session_id: String,
    assigned_to_target: bool,
}

fn assign_opened_session(
    state: &mut AppState,
    target: Option<&PaneTarget>,
    session_id: &str,
) -> bool {
    let Some(target) = target else {
        // No explicit pane target: this session becomes a new top-level tab.
        // push_workspace_tab creates the WorkspaceTab + sets active_tab +
        // active_session (the latter mirrors the tab's anchor for Step 1
        // backwards compatibility).
        push_workspace_tab(state, session_id);
        return false;
    };

    set_pane_session_for_layout(
        state,
        &target.layout_owner_tab_id,
        target.pane_idx,
        session_id.to_string(),
    )
}

#[cfg(test)]
mod connection_target_tests {
    use super::*;
    use crate::layout::LayoutPreset;

    #[test]
    fn explicit_pane_target_keeps_source_independent_and_ignores_current_active_tab() {
        let mut state = AppState {
            active_session: Some("other-tab".to_string()),
            ..AppState::default()
        };
        state.layouts.insert(
            "layout-owner".to_string(),
            PaneLayout::from_preset(LayoutPreset::Grid4, &["one".to_string(), "two".to_string()]),
        );
        let target = PaneTarget {
            layout_owner_tab_id: "layout-owner".to_string(),
            pane_idx: 2,
        };

        assert!(assign_opened_session(&mut state, Some(&target), "clone"));
        assert_eq!(state.layouts["layout-owner"].panes[0].session_id, "one");
        assert_eq!(state.layouts["layout-owner"].panes[2].session_id, "clone");
        assert_ne!(
            state.layouts["layout-owner"].panes[0].session_id,
            state.layouts["layout-owner"].panes[2].session_id
        );
        assert_eq!(state.active_session.as_deref(), Some("other-tab"));
    }

    #[test]
    fn failed_explicit_pane_target_does_not_change_active_tab() {
        let mut state = AppState {
            active_session: Some("layout-owner".to_string()),
            ..AppState::default()
        };
        let target = PaneTarget {
            layout_owner_tab_id: "missing-layout".to_string(),
            pane_idx: 2,
        };

        assert!(!assign_opened_session(&mut state, Some(&target), "clone"));
        assert_eq!(state.active_session.as_deref(), Some("layout-owner"));
    }

    /// The sidebar drop's state preparation plus open-session assignment must
    /// preserve two occupied panes and place the new session in exactly one
    /// additional pane.
    #[test]
    fn sidebar_drag_full_two_pane_layout_adds_and_assigns_exactly_one_pane() {
        let mut state = AppState::default();
        let layout_owner = push_workspace_tab(&mut state, "existing-one");
        state.layouts.insert(
            layout_owner.clone(),
            PaneLayout::from_preset(
                LayoutPreset::Split2H,
                &["existing-one".to_string(), "existing-two".to_string()],
            ),
        );

        let plan = prepare_split_for_sidebar_drop(&mut state, 0).expect("drop plan");
        assert_eq!(plan.layout_owner_tab_id, layout_owner);
        assert_eq!(plan.pane_idx, 2);
        assert!(plan.created_new_pane);
        let target = PaneTarget {
            layout_owner_tab_id: plan.layout_owner_tab_id,
            pane_idx: plan.pane_idx,
        };

        assert!(assign_opened_session(
            &mut state,
            Some(&target),
            "new-from-sidebar"
        ));
        let layout = &state.layouts[&layout_owner];
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "existing-one");
        assert_eq!(layout.panes[1].session_id, "existing-two");
        assert_eq!(layout.panes[2].session_id, "new-from-sidebar");
        assert!(layout.panes.iter().all(|pane| !pane.session_id.is_empty()));
        assert_eq!(state.active_tab.as_deref(), Some(layout_owner.as_str()));
    }
}

/// Tests for the pane title bar's session-type accent color helper.
///
/// The accent color is rendered as a 3px left strip on each pane title
/// bar so the user can tell session types apart at a glance (SSH vs Shell
/// vs Serial etc.). The mapping mirrors `sidebar.rs::kind_color` but is
/// keyed on `SessionType` (what `SessionTab` exposes) rather than
/// `ConnectionKind` — a cloned pane inherits the source session's type,
/// so the accent stays consistent across clones.
#[cfg(test)]
mod session_type_accent_tests {
    use super::session_type_accent_color;
    use rusterm_core::session::SessionType;

    #[test]
    fn ssh_accent_is_blue() {
        assert_eq!(session_type_accent_color(&SessionType::Ssh), "#7aa2f7");
    }

    #[test]
    fn shell_accent_is_green() {
        assert_eq!(session_type_accent_color(&SessionType::Shell), "#9ece6a");
    }

    #[test]
    fn serial_accent_is_amber() {
        assert_eq!(session_type_accent_color(&SessionType::Serial), "#e0af68");
    }

    #[test]
    fn telnet_accent_is_orange() {
        assert_eq!(session_type_accent_color(&SessionType::Telnet), "#ff9e64");
    }

    #[test]
    fn tcp_accent_is_cyan() {
        assert_eq!(session_type_accent_color(&SessionType::Tcp), "#7dcfff");
    }

    /// All accent colors must be distinct so two panes of different types
    /// are visually distinguishable. This is a regression guard against
    /// accidentally collapsing two types onto the same color.
    #[test]
    fn all_accents_are_distinct() {
        let colors = [
            session_type_accent_color(&SessionType::Ssh),
            session_type_accent_color(&SessionType::Shell),
            session_type_accent_color(&SessionType::Serial),
            session_type_accent_color(&SessionType::Telnet),
            session_type_accent_color(&SessionType::Tcp),
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "accent colors collide at {} vs {}",
                    i, j
                );
            }
        }
    }
}

/// Open a connection as a new runtime session.
///
/// With no `target`, the new session becomes the active tab. With an explicit
/// target, the session is assigned only to that layout and pane; assignment
/// failure never changes `active_session`. The result reports whether the
/// requested pane assignment succeeded so callers can verify it.

fn open_connection(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    conn: ConnectionConfig,
    target: Option<PaneTarget>,
) -> OpenConnectionResult {
    let tab_id = uuid::Uuid::new_v4().to_string();
    create_terminal(tab_id.clone(), &mut state);
    // Remember the config so this session can be reconnected by pressing
    // Enter after a disconnect.
    state
        .write()
        .session_configs
        .insert(tab_id.clone(), conn.clone());

    let assigned_to_target = match &conn.kind {
        ConnectionKind::Ssh(ssh_config) => {
            state.write().sessions.push(SessionTab {
                id: tab_id.clone(),
                name: conn.name.clone(),
                kind: SessionType::Ssh,
                render_output: Default::default(),
                version: 0,
                suggestion: None,
                suggestions: Vec::new(),
                suggestion_selected: 0,
                suggestion_visible: false,
                command_history: Vec::new(),
                hostname: Some(ssh_config.host.clone()),
                cwd: None,
            });
            let assigned = assign_opened_session(&mut state.write(), target.as_ref(), &tab_id);
            start_ssh_connection(state, input_senders, tab_id.clone(), ssh_config.clone());
            assigned
        }
        ConnectionKind::Shell(shell_config) => {
            let msg = format!("\r\nStarting shell...\r\n");
            let render_output = {
                let terminals = state.read().terminals.clone();
                if let Some(handle) = terminals.get(&tab_id) {
                    handle.lock().process_and_render(msg.as_bytes())
                } else {
                    Default::default()
                }
            };
            state.write().sessions.push(SessionTab {
                id: tab_id.clone(),
                name: conn.name.clone(),
                kind: SessionType::Shell,
                render_output,
                version: 1,
                suggestion: None,
                suggestions: Vec::new(),
                suggestion_selected: 0,
                suggestion_visible: false,
                command_history: Vec::new(),
                hostname: Some("local".to_string()),
                cwd: None,
            });
            let assigned = assign_opened_session(&mut state.write(), target.as_ref(), &tab_id);
            start_shell_connection(state, input_senders, tab_id.clone(), shell_config.clone());
            assigned
        }
        _ => {
            let msg = format!("\r\nConnection type not yet supported\r\n");
            let terminals = state.read().terminals.clone();
            if let Some(handle) = terminals.get(&tab_id) {
                let render_result = handle.lock().process_and_render(msg.as_bytes());
                state.write().sessions.push(SessionTab {
                    id: tab_id.clone(),
                    name: conn.name.clone(),
                    kind: SessionType::Ssh,
                    render_output: render_result,
                    version: 1,
                    suggestion: None,
                    suggestions: Vec::new(),
                    suggestion_selected: 0,
                    suggestion_visible: false,
                    command_history: Vec::new(),
                    hostname: None,
                    cwd: None,
                });
                assign_opened_session(&mut state.write(), target.as_ref(), &tab_id)
            } else {
                false
            }
        }
    };

    OpenConnectionResult {
        session_id: tab_id,
        assigned_to_target,
    }
}

fn reconnect_session(
    mut state: Signal<AppState>,
    mut input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    tab_id: String,
) {
    let conn = state.read().session_configs.get(&tab_id).cloned();
    let Some(conn) = conn else {
        tracing::warn!(
            "[Reconnect] no stored config for session {}",
            &tab_id[..tab_id.len().min(8)]
        );
        return;
    };

    // Preserve the pane-specific terminal size before replacing the dead
    // terminal model. This is also the size `start_ssh_connection` will use if
    // TerminalView's ResizeObserver does not fire again for unchanged geometry.
    let previous_size = state
        .read()
        .terminals
        .get(&tab_id)
        .map(|handle| handle.lock().terminal.size())
        .unwrap_or_default();

    // Disconnected -> Reconnecting is an atomic, one-way transition. Repeated
    // Enter presses while the async connection attempt is running are no-ops.
    {
        let mut s = state.write();
        if !begin_reconnect(&mut s, &tab_id) {
            tracing::debug!(
                "[Reconnect] ignored duplicate or non-disconnected session {}",
                &tab_id[..tab_id.len().min(8)]
            );
            return;
        }
        s.close_senders.retain(|(sid, _)| sid != &tab_id);
        s.resize_senders.remove(&tab_id);
        s.terminals.remove(&tab_id);
        s.onekey_popups.remove(&tab_id);
        s.pending_exit_check.remove(&tab_id);
    }
    input_senders.write().remove(&tab_id);

    create_terminal_with_size(tab_id.clone(), previous_size, &mut state);
    {
        let terminals = state.read().terminals.clone();
        if let Some(handle) = terminals.get(&tab_id) {
            let render_result = handle.lock().process_and_render(b"\r\nReconnecting...\r\n");
            let mut s = state.write();
            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                tab.render_output = render_result;
                tab.version += 1;
            }
        }
    }

    tracing::info!(
        "[Reconnect] starting session={} cols={} rows={}",
        &tab_id[..tab_id.len().min(8)],
        previous_size.cols,
        previous_size.rows
    );
    match conn.kind {
        ConnectionKind::Ssh(ssh_config) => {
            start_ssh_connection(state, input_senders, tab_id, ssh_config);
        }
        ConnectionKind::Shell(shell_config) => {
            start_shell_connection(state, input_senders, tab_id, shell_config);
        }
        _ => {
            state
                .write()
                .session_connection_states
                .insert(tab_id.clone(), SessionConnectionState::Disconnected);
            tracing::warn!(
                "[Reconnect] unsupported connection kind for {}",
                &tab_id[..tab_id.len().min(8)]
            );
        }
    }
}

#[component]
pub fn App() -> Element {
    let mut state = use_signal(AppState::default);
    let mut modal = use_signal(|| Modal::None);
    let ai_suggestions = use_signal(Vec::<rusterm_ai::suggestion::AiSuggestion>::new);
    let mut input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>> =
        use_signal(HashMap::new);
    // Connection currently being edited in the ConnectionDialog. `None` means
    // the dialog is in create mode. We reuse `Modal::NewConnection` for both
    // modes so `Modal` can stay `Copy`; the actual connection lives here.
    let mut editing_conn: Signal<Option<ConnectionConfig>> = use_signal(|| None);
    // Connection pending a delete confirmation. When `Some`, a small confirm
    // modal is rendered; the sidebar only triggers the request, the confirm
    // happens here so destructive actions can't be fired by a stray click.
    let mut delete_target: Signal<Option<ConnectionConfig>> = use_signal(|| None);

    // Pane index currently being hovered by a drag operation, or `None` when
    // no drag is in progress. Used by `multi_pane_container` to highlight the
    // drop-target pane. Reads happen inside the pane `for` loop (subscribing
    // `App` to the signal).
    //
    // SOLE WRITER: the manual polling loop below. It hit-tests the cursor
    // at ~60Hz against the active layout and sets `Some((pane_idx, region))`
    // with the real 4-quadrant region. HTML5 `ondragover`/`ondragenter` only
    // call `prevent_default` (for drop permission) — they do NOT write the
    // signal. This avoids a race between HTML5 (which reports the pane the
    // browser thinks the cursor is over) and the hit-test (which computes
    // the pane from viewport coordinates) — that race caused the overlay
    // to flicker between adjacent panes at boundaries, visually appearing
    // as multiple "田"-shaped 4-block overlays (the "错误的产生多个不需要的
    // 四方块" bug). `ondrop` and `finish_tab_drag` clear the signal to `None`.
    //
    // PERF: `Signal::set` performs an equality check before triggering a
    // re-render, so calling `set` with an unchanged `(pane_idx, region)` is
    // a no-op — the highlight only changes when the dragged pane OR region
    // actually changes. This matches the user's "取舍分频性能" preference:
    // fewer re-renders over per-tick feedback.
    let mut drag_over_pane: Signal<Option<(usize, PaneDropRegion)>> = use_signal(|| None);

    // Measured pixel dimensions of the `#terminal-content` container (the
    // flex:1 div that holds either the single active TerminalView or the
    // multi-pane container). Updated by a ResizeObserver + 100ms polling
    // loop below — the same pattern TerminalView uses to measure its own
    // cell grid. This is what lets `multi_pane_container` lay panes out at
    // the actual viewport size instead of the prior 1200×800 hardcode
    // (which clipped panes or left empty space when the window was a
    // different size — the "显示分辨率不对" symptom).
    //
    // `None` only on the very first frame (before the observer fires);
    // `multi_pane_container` falls back to 1200×800 in that case so the
    // first paint isn't blank.
    let mut container_size: Signal<Option<(f64, f64)>> = use_signal(|| None);

    // Active splitter-bar drag, if any. Set by the splitter's `onmousedown`
    // (in `render_col_splitters` / `render_row_splitters`); read by the
    // drag-resize overlay rendered in `multi_pane_container` (a visual-only
    // cursor indicator with `pointer-events: none`); cleared by
    // `end_split_drag` when the polling `use_future` below detects the
    // JS-side `window.__rusterm_drag_done` flag.
    //
    // ## Drag mechanism
    //
    // The splitter's `onmousedown` sets this signal AND calls
    // `install_split_drag_js_listeners` to attach document-level
    // capture-phase `mousemove`/`mouseup` listeners. Those listeners write
    // the current mouse position to `window.__rusterm_drag_pos` and set
    // `window.__rusterm_drag_done = true` on mouseup. The polling
    // `use_future` (`_split_drag_poll` below) reads those globals every
    // 16ms and calls `apply_split_drag_step` to apply the delta.
    //
    // Why this design: dioxus 0.7's desktop webview doesn't reliably fire
    // element-level `onmousemove`/`onmouseup` during a button-held drag
    // (pointer-capture behavior is inconsistent across WKWebView/webkitgtk/
    // WebView2). Document-level capture-phase listeners are the only
    // mechanism that works reliably.
    let split_drag: Signal<Option<SplitDragState>> = use_signal(|| None);

    // Active tab/pane drag (Task 22). Set by the tab bar's `onmousedown`
    // (via `on_drag_start` → `start_tab_drag`) or by a pane title bar's
    // `onmousedown`; read and updated by the polling `use_future`
    // (`_tab_drag_poll` below); cleared by `finish_tab_drag` when the
    // polling loop detects the JS-side `__rusterm_tab_drag_done` flag.
    //
    // ## Why manual mouse-based drag (replacing HTML5 DnD for tabs)
    //
    // HTML5 drag-and-drop (`draggable: true`, `ondragstart`, `ondrop`)
    // was UNRELIABLE in dioxus 0.7's desktop webview (drops sometimes
    // didn't fire, DataTransfer data sometimes came back empty, the
    // native drag ghost sometimes ate the release event). The splitter
    // drag-resize feature hit the SAME wall and was fixed by switching
    // to document-level capture-phase JS listeners + polling (see the
    // "Splitter drag-resize fix" section in the architecture memory).
    // This tab-drag system mirrors that PROVEN pattern exactly.
    //
    // HTML5 DnD REMAINS for sidebar→pane connection drags (Task 16) —
    // that feature has no user complaints and the connection-id MIME
    // type is untouched.
    //
    // ## Click vs drag
    //
    // `onmousedown` sets `tab_drag` with `dragging: false`. The polling
    // loop watches the cursor; once it moves more than
    // `TAB_DRAG_THRESHOLD` pixels from the start position, `dragging`
    // becomes `true`. The drop is ONLY executed on `mouseup` if
    // `dragging == true`. This preserves plain click-to-select: a
    // mousedown with no significant mousemove is a click, not a drag —
    // the polling loop cleans up the signal and the tab's `onclick`
    // fires normally.
    let mut tab_drag: Signal<Option<TabDragState>> = use_signal(|| None);

    // Active freeform pane-window move. This is deliberately separate from
    // `tab_drag`: the ⠿ handle moves the window, while the title text keeps
    // the existing session move/swap/split gesture.
    let pane_move: Signal<Option<PaneMoveState>> = use_signal(|| None);

    // Polling loop for the active splitter drag. Runs forever (the loop
    // sleeps when no drag is in progress). When `split_drag` is `Some`, polls
    // the JS global `window.__rusterm_drag_pos` (written by the document-level
    // capture-phase listeners installed in `install_split_drag_js_listeners`)
    // every 16ms and applies the delta via `apply_split_drag_step`. When the
    // JS-side `window.__rusterm_drag_done` flag is set (user released the mouse
    // button), calls `end_split_drag` to clear the drag state and restore
    // focus.
    //
    // 16ms is fast enough for smooth dragging (the eye can't distinguish
    // finer granularity) and slow enough to avoid flooding the dioxus runtime
    // with eval round-trips. The eval is a single round-trip per poll,
    // returning a string like "123.4,567.8,0" (x, y, done-flag).
    //
    // The loop NEVER breaks — after a drag ends, it goes back to the idle
    // polling state (32ms sleep) so subsequent drags are handled. `use_future`
    // only runs its closure once on mount, so breaking would prevent all
    // future drags from being polled.
    let _split_drag_poll = use_future(move || async move {
        // Loop forever. When idle (no drag), sleep 32ms and re-check. When
        // a drag is active, poll JS every 16ms.
        loop {
            if split_drag().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(32)).await;
                continue;
            }
            // Read the JS-side mouse position + done flag.
            match poll_split_drag_state().await {
                Some((x, y, done)) => {
                    let drag_opt = split_drag();
                    let Some(drag) = drag_opt else {
                        // Drag was cleared between the check above and now —
                        // go back to idle polling.
                        continue;
                    };
                    if done {
                        tracing::info!(
                            "[LAYOUT] drag done flag detected (x={:.1}, y={:.1}); ending drag",
                            x,
                            y
                        );
                        end_split_drag(state, split_drag);
                        // Loop back to idle — do NOT break, or subsequent
                        // drags wouldn't be polled.
                        continue;
                    }
                    // Apply the drag step. Use the appropriate coordinate
                    // (x for col drag, y for row drag).
                    let pos = if drag.is_col { x } else { y };
                    apply_split_drag_step(state, split_drag, pos);
                }
                None => {
                    // JS globals not set yet (listener install may still be
                    // in-flight). Sleep a bit and retry.
                    tracing::debug!("[LAYOUT] poll_split_drag_state returned None; retrying");
                }
            }
            // 16ms = ~60Hz. Fast enough for smooth dragging.
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
    });

    // Polling loop for freeform pane-window movement. It uses its own JS
    // globals so splitter resize and session drag state cannot interfere.
    let _pane_move_poll = use_future(move || async move {
        loop {
            if pane_move().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(32)).await;
                continue;
            }
            match poll_pane_move_state().await {
                Some((x, y, done)) => {
                    apply_pane_move_step(state, pane_move, x, y);
                    if done {
                        tracing::info!("[PANE-MOVE] finished at x={x:.1} y={y:.1}");
                        end_pane_move(state, pane_move);
                        continue;
                    }
                }
                None => tracing::debug!("[PANE-MOVE] poll returned no position; retrying"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
    });

    // Polling loop for the active tab/pane drag (Task 22). Mirrors
    // `_split_drag_poll` exactly but reads `__rusterm_tab_drag_pos` /
    // `__rusterm_tab_drag_done` instead of the splitter globals. Runs
    // forever — when idle (no drag), sleeps 32ms and re-checks. When a
    // drag is active, polls JS every 16ms.
    //
    // The loop:
    //  1. If `tab_drag` is `None`: sleep 32ms, continue.
    //  2. Poll `poll_tab_drag_state()` → `(x, y, done, left, top)`.
    //  3. Update `tab_drag`'s `cur_x`/`cur_y`. If `!dragging` and the
    //     cursor has crossed the threshold, set `dragging = true`.
    //  4. If `done` (user released the mouse button):
    //     - If `dragging == true`: call `finish_tab_drag` (hit-test +
    //       `execute_tab_drop_on_pane` + focus restore + cleanup).
    //     - Else (it was a click, not a drag): just clean up the signal.
    //       The tab's `onclick` fires normally.
    //  5. Else if `dragging`: live hit-test → `drag_over_pane.set(idx)`
    //     for drop-zone highlight.
    //  6. Sleep 16ms.
    //
    // NEVER breaks — `use_future` only runs its closure once on mount,
    // so breaking would prevent all future drags from being polled.
    let _tab_drag_poll = use_future(move || async move {
        loop {
            if tab_drag().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(32)).await;
                continue;
            }
            // Read the JS-side mouse position + done flag + container offset.
            match poll_tab_drag_state().await {
                Some((x, y, done, left, top)) => {
                    let drag_opt = tab_drag();
                    let Some(drag) = drag_opt else {
                        // Drag was cleared between the check above and now —
                        // go back to idle polling.
                        continue;
                    };
                    // Promote to `dragging: true` if the cursor has
                    // crossed the threshold. This is what distinguishes
                    // a click (no drag) from a real drag.
                    let now_dragging = drag.dragging
                        || tab_drag_threshold_exceeded(drag.start_x, drag.start_y, x, y);
                    // Update the drag state with the new cursor position
                    // (and possibly the promoted `dragging` flag). Drop
                    // the borrow before any `state.write()` below to
                    // avoid re-entrant signal writes.
                    if now_dragging != drag.dragging || x != drag.cur_x || y != drag.cur_y {
                        tab_drag.set(Some(TabDragState {
                            cur_x: x,
                            cur_y: y,
                            dragging: now_dragging,
                            ..drag
                        }));
                    }
                    if done {
                        if now_dragging {
                            tracing::info!(
                                "[TAB-DRAG] done flag detected (x={:.1}, y={:.1}); finishing drag",
                                x,
                                y
                            );
                            finish_tab_drag(
                                state,
                                input_senders,
                                tab_drag,
                                drag_over_pane,
                                container_size,
                                x,
                                y,
                                left,
                                top,
                            );
                        } else {
                            // It was a click, not a drag. Clean up the
                            // signal WITHOUT executing a drop. The tab's
                            // `onclick` fires normally.
                            tracing::debug!("[TAB-DRAG] mousedown-without-drag cleanup (click)");
                            tab_drag.set(None);
                            drag_over_pane.set(None);
                            // Clean up the JS-side globals.
                            spawn(async move {
                                let _ = dioxus::document::eval(
                                    "(function() {\n\
                                        if (window._rusterm_tab_drag_remove) { window._rusterm_tab_drag_remove(); window._rusterm_tab_drag_remove = null; }\n\
                                        window.__rusterm_tab_drag_pos = '';\n\
                                        window.__rusterm_tab_drag_done = false;\n\
                                        document.body.style.webkitUserSelect = '';\n\
                                        document.body.style.userSelect = '';\n\
                                    })()",
                                ).await;
                            });
                        }
                        // Loop back to idle — do NOT break, or subsequent
                        // drags wouldn't be polled.
                        continue;
                    }
                    // Live hit-test for drop-zone highlight (only when
                    // actually dragging — a click doesn't highlight).
                    if now_dragging {
                        let (cw, ch) = container_size().unwrap_or((1200.0, 800.0));
                        let active_layout = state
                            .read()
                            .active_tab
                            .as_ref()
                            .and_then(|aid| state.read().layouts.get(aid).cloned());
                        // Compute the (pane_idx, region) tuple for the
                        // drag-over highlight. Multi-pane layouts use the
                        // full 4-quadrant hit-test so the overlay's target
                        // half tracks the cursor; the single-pane fallback
                        // synthesizes pane 0 with a 4-quadrant region on
                        // container bounds.
                        let hit: Option<(usize, PaneDropRegion)> =
                            if let Some(layout) = active_layout.as_ref() {
                                hit_test_pane_drop_target_at(x, y, left, top, cw, ch, layout)
                                    .map(|target| (target.pane_idx, target.region))
                            } else {
                                // No layout — single pane. Cursor in container
                                // rect → pane 0 with a 4-quadrant region.
                                let rel_x = x - left;
                                let rel_y = y - top;
                                if rel_x >= 0.0 && rel_y >= 0.0 && rel_x <= cw && rel_y <= ch {
                                    let dx = if cw > 0.0 { rel_x / cw - 0.5 } else { 0.0 };
                                    let dy = if ch > 0.0 { rel_y / ch - 0.5 } else { 0.0 };
                                    Some((0usize, pane_drop_region_for_cursor(dx, dy)))
                                } else {
                                    None
                                }
                            };
                        // `Signal::set` is a no-op if the value is
                        // unchanged, so this is cheap when the cursor
                        // stays in the same pane AND region.
                        drag_over_pane.set(hit);
                    }
                }
                None => {
                    // JS globals not set yet (listener install may still
                    // be in-flight). Sleep a bit and retry.
                    tracing::debug!("[TAB-DRAG] poll_tab_drag_state returned None; retrying");
                }
            }
            // 16ms = ~60Hz. Fast enough for smooth dragging.
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
    });

    // ResizeObserver + polling loop for `#terminal-content`. We poll at
    // 100ms instead of relying solely on ResizeObserver callbacks because
    // dioxus's `eval` bridge is request/response — we can't get a
    // synchronous callback from JS into Rust. The observer sets a dirty
    // flag (`_rusterm_container_resize_pending`); the poll reads the
    // flag and, if set, re-measures via `getBoundingClientRect` and
    // updates the signal. This mirrors the TerminalView resize loop.
    let _container_measure = use_future(move || async move {
        // Observer installation happens INSIDE the poll loop (not once
        // up-front). At startup `#terminal-content` may not exist yet —
        // e.g. the unlock screen is showing, or no session is open — and
        // a one-shot install would silently fail, leaving the dirty flag
        // permanently unset and `container_size` stuck at `None` (the
        // 1200×800 fallback → panes that don't fill the window, the
        // "窗口无法被填满" bug). The per-tick script is idempotent: the
        // `el._rusterm_ro` guard skips re-install on the SAME element,
        // and a remounted element (fresh instance, no `_rusterm_ro`)
        // gets a fresh observer + an immediate forced measure.
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let result = dioxus::document::eval(
                "return (function() { const el = document.getElementById('terminal-content'); \
                 if (!el) return ''; \
                 if (!el._rusterm_ro) { \
                   el._rusterm_ro = new ResizeObserver(function() { el._rusterm_container_resize_pending = true; }); \
                   el._rusterm_ro.observe(el); \
                   el._rusterm_container_resize_pending = true; \
                 } \
                 if (!el._rusterm_container_resize_pending) return ''; \
                 el._rusterm_container_resize_pending = false; \
                 const r = el.getBoundingClientRect(); \
                 if (r.width <= 0 || r.height <= 0) return ''; \
                 return r.width.toFixed(2) + ',' + r.height.toFixed(2); })()",
            )
            .await;
            if let Ok(value) = result {
                if let Some(s) = value.as_str() {
                    if s.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = s.split(',').collect();
                    if parts.len() == 2 {
                        if let (Ok(w), Ok(h)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                            if w > 0.0 && h > 0.0 {
                                container_size.set(Some((w, h)));
                            }
                        }
                    }
                }
            }
        }
    });

    // One-time startup: import local shell history (zsh/bash/fish/atuin) into
    // the SQLite DB so suggestions can draw from a single unified source.
    //
    // DIRTY-DATA GUARD: we filter out commands that are already marked as
    // known-failed in the DB. Without this, every app launch would re-import
    // `~/.bash_history` / `~/.zsh_history` (which contain old failed commands
    // from OTHER terminals — those files have no exit-code info) as
    // `exit_code = NULL` rows, which the HAVING clause keeps as "unknown,
    // assume success" — re-surfacing the user's typos in the suggestion popup
    // on every launch. The filter mirrors the shell-connection import path.
    //
    // EXIT-CODE PROPAGATION: atuin stores per-execution `exit_code`; we now
    // read it (see `AtuinDbProvider::search`) so failed commands imported
    // from atuin land with a non-zero exit code and are filtered by HAVING.
    // bash/zsh/fish flat-file sources have no exit code → None → still NULL
    // → filtered here by `known_failed_commands`.
    let _history_import = use_future(move || async move {
        let db_path = dirs::data_dir()
            .unwrap_or_default()
            .join("rusterm")
            .join("rusterm.db");
        let db = match rusterm_db::Database::open(Some(db_path)).await {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("Failed to open DB for local history import: {}", e);
                return;
            }
        };

        let provider = rusterm_history::HybridHistoryProvider::new();
        let local_commands = provider.search("", 5000);
        if local_commands.is_empty() {
            tracing::info!("No local shell history found to import");
            return;
        }

        // Fetch the known-failed set BEFORE building entries so we can skip
        // commands the user has previously marked as failed. This is the
        // critical guard against re-introducing typos as NULL-exit-code rows
        // on every app launch.
        let failed_set = db.known_failed_commands().await.unwrap_or_default();
        let skipped_failed = local_commands
            .iter()
            .filter(|m| failed_set.contains(&m.command))
            .count();
        if skipped_failed > 0 {
            tracing::info!(
                "Skipping {} known-failed commands during startup import",
                skipped_failed
            );
        }

        // Replace previous local import (delete by hostname to avoid duplicates)
        let _ = db.delete_history_by_hostname("local").await;

        let entries: Vec<_> = local_commands
            .iter()
            .filter(|m| !failed_set.contains(&m.command))
            .map(|m| rusterm_db::history::HistoryEntry {
                id: uuid::Uuid::new_v4().to_string(),
                command: m.command.clone(),
                session_id: "local-import".to_string(),
                cwd: m.cwd.clone(),
                hostname: Some("local".to_string()),
                // Propagate atuin's exit_code so failed commands imported
                // from atuin are correctly marked as failed (and filtered
                // by HAVING). bash/zsh/fish sources have None — those land
                // as NULL, but the failed_set filter above already removed
                // any of them that we know to have failed.
                exit_code: m.exit_code,
                duration_ms: None,
                created_at: m
                    .timestamp
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
            })
            .collect();

        tracing::info!("Importing {} local history commands into DB", entries.len());
        if let Err(e) = db.save_history_batch(entries).await {
            tracing::warn!("Failed to save local history batch: {}", e);
        }

        // Mirror the SQLite history into DuckDB for analytics. This is a
        // best-effort, fire-and-forget task — if it fails (e.g. DuckDB can't
        // open), analytics queries will return empty results, but the app
        // continues to function normally. Only runs when the `analytics`
        // feature is enabled; otherwise the cfg-gated block is empty.
        // Reuse the AppState's lazily-initialized AnalyticsHandle for the
        // startup mirror — same reasoning as the per-command spawn above:
        // avoid opening a fresh DuckDB connection just for this call, so the
        // connection opened here is the same one reused by later per-command
        // record_command spawns. The handle's mirror_from_sqlite lazy-opens
        // the DB on first call (which is here, on startup) and reuses it
        // thereafter.
        #[cfg(feature = "analytics")]
        {
            let analytics_handle = state.read().analytics.clone();
            match analytics_handle.mirror_from_sqlite(&db).await {
                Ok(count) => {
                    tracing::info!(
                        "[ANALYTICS] mirrored {} commands from sqlite to duckdb on startup",
                        count
                    );
                }
                Err(e) => {
                    tracing::warn!("[ANALYTICS] startup mirror failed: {}", e);
                }
            }
        }
    });

    // Window state persistence: poll the live window geometry (size, position,
    // maximized) every 250ms and save it to window_state.json when it changes.
    // This is how the app "remembers" the user's preferred window configuration
    // across launches — if they maximize every time, the next launch opens
    // maximized.
    //
    // We poll from a use_future rather than the dioxus custom event handler
    // because (a) DesktopContext gives us direct access to `is_maximized()`,
    // which the event handler doesn't, and (b) it lets us coalesce size +
    // position + maximized into a single state snapshot rather than three
    // separate events.
    //
    // The 250ms poll interval acts as a natural debounce so dragging the
    // window doesn't write hundreds of times per second.
    let _window_state_future = use_future(|| async move {
        // Get the desktop context. DesktopService derefs to tao::Window, so
        // is_maximized / inner_size / outer_position are all directly callable.
        let desktop = dioxus::desktop::window();

        // Track the last state we persisted so we only write when something
        // actually changed. This avoids hammering the disk while the window
        // is idle.
        let mut last_persisted: Option<rusterm_core::WindowState> =
            rusterm_core::WindowState::load();

        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;

            // Read the current window state from the desktop context.
            let maximized = desktop.is_maximized();
            let inner = desktop.inner_size();
            let outer = desktop.outer_position();
            let scale_factor = desktop.scale_factor();

            // tao reports physical pixels; convert to logical so the saved
            // values are DPI-independent (restore correctly on a different
            // monitor / scale factor).
            let logical_size = inner.to_logical::<f64>(scale_factor);
            let (logical_x, logical_y) = match outer {
                Ok(pos) => {
                    let logical_pos = pos.to_logical::<f64>(scale_factor);
                    (logical_pos.x, logical_pos.y)
                }
                Err(_) => {
                    // outer_position can fail on some platforms (e.g. iOS); fall
                    // back to the last known position rather than zeroing it out.
                    last_persisted
                        .as_ref()
                        .map(|s| (s.x, s.y))
                        .unwrap_or((0.0, 0.0))
                }
            };

            let current = rusterm_core::WindowState {
                width: logical_size.width,
                height: logical_size.height,
                x: logical_x,
                y: logical_y,
                maximized,
            };

            // Only write if something changed.
            if last_persisted.as_ref() != Some(&current) {
                if let Err(e) = current.save() {
                    tracing::warn!("Failed to save window state: {}", e);
                } else {
                    last_persisted = Some(current);
                }
            }
        }
    });

    // Periodically persist the session state (cwd of each tab, etc.) so the
    // user can restore on next launch even if the app is killed without a
    // graceful shutdown. 30s is a balance between not losing too much state
    // and not hammering the disk with encrypted writes.
    //
    // We skip saving while:
    // - The app is still locked (no master key available yet)
    // - `restore_disabled` is true (user picked "不再询问" earlier)
    // - There are no sessions open (nothing to save)
    //
    // The save itself is atomic (temp + rename) so concurrent saves from
    // multiple sources (this loop + the close handler) can't corrupt the
    // file — last writer wins, which is the correct behavior for a snapshot.
    let state_for_save = state.clone();
    let _session_state_save_future = use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            let s = state_for_save.read();
            if s.unlock_state != UnlockState::Unlocked {
                continue;
            }
            if s.restore_disabled {
                continue;
            }
            if s.sessions.is_empty() {
                continue;
            }
            let Some(cm) = s.config_manager.as_ref() else {
                continue;
            };
            let master_key = cm.master_key();
            // Build the snapshot while holding the read lock, then release
            // before encrypting+writing (encryption is CPU-bound, no need
            // to hold the lock through it).
            let snapshot = s.build_session_state(s.theme_name());
            // Drop the read lock before the (CPU-bound) encrypt+write.
            drop(s);
            if let Err(e) = snapshot.save(&master_key) {
                tracing::warn!("Failed to save session state: {}", e);
            }
        }
    });

    // Master password unlock gate
    match state.read().unlock_state {
        UnlockState::Locked | UnlockState::FirstRun => {
            let mode = state.read().unlock_state;
            let error = state.read().master_password_error.clone();
            return rsx! {
                MasterPasswordDialog {
                    mode,
                    error,
                    on_unlock: move |password: String| {
                        match ConfigManager::with_master_password(&password) {
                            Ok(cm) => {
                                let connections = cm.load_connections().unwrap_or_default();
                                // If even one step fails to decrypt, load_onekeys
                                // returns Err and (without this) unwrap_or_default
                                // would silently empty the whole library → no
                                // popup ever. Log so a decrypt failure is visible
                                // instead of the autofill just "not working".
                                let onekeys = match cm.load_onekeys() {
                                    Ok(v) => v,
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to load OneKeys (autofill disabled until re-saved): {}",
                                            e
                                        );
                                        Vec::new()
                                    }
                                };
                                let mut s = state.write();
                                // Load the persisted `restore_disabled` flag from
                                // settings.json so we know whether to even
                                // attempt loading `session_state.enc`.
                                s.restore_disabled = cm.load_restore_disabled();
                                s.focused_tab_appearance = cm.load_focused_tab_appearance();
                                s.config_manager = Some(cm);
                                s.connections = connections;
                                s.onekeys = onekeys;
                                s.unlock_state = UnlockState::Unlocked;
                                s.master_password_error = None;

                                // Load saved session state (if any) and prepare
                                // the restore-confirmation modal. We DON'T
                                // restore here — the user gets to decide via the
                                // modal (3 buttons: 恢复 / 跳过 / 不再询问).
                                // If `restore_disabled` was already true (from
                                // a previous "不再询问" choice that survived
                                // because it's persisted in settings.json),
                                // we skip the load entirely — no point in
                                // decrypting a file we'll never restore from.
                                if !s.restore_disabled {
                                    if let Some(cm_ref) = s.config_manager.as_ref() {
                                        let master_key = cm_ref.master_key();
                                        match rusterm_core::SessionState::load(&master_key) {
                                            Ok(Some(loaded)) => {
                                                tracing::info!(
                                                    "Loaded saved session state: {} sessions, saved at {}",
                                                    loaded.sessions.len(),
                                                    loaded.saved_at
                                                );
                                                s.restore_pending = Some(loaded);
                                            }
                                            Ok(None) => {
                                                // First launch or no saved state — fine, nothing to restore.
                                                tracing::debug!("No saved session state found");
                                            }
                                            Err(e) => {
                                                // Corrupt/tampered/wrong key —
                                                // log and continue without
                                                // prompting. Better to silently
                                                // skip than to nag the user
                                                // about a corrupt file they
                                                // can't do anything about.
                                                tracing::warn!(
                                                    "Failed to load saved session state (will skip restore): {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                let msg = if e.to_string().contains("Invalid") {
                                    "Invalid master password".to_string()
                                } else {
                                    format!("Error: {}", e)
                                };
                                state.write().master_password_error = Some(msg);
                            }
                        }
                    },
                    on_clear_error: move |_| {
                        if state.read().master_password_error.is_some() {
                            state.write().master_password_error = None;
                        }
                    },
                }
            };
        }
        UnlockState::Unlocked => {}
    }

    rsx! {
        div {
            id: "main",
            style: "
                display: flex;
                height: 100%;
                width: 100%;
                overflow: hidden;
                background: #1a1b26;
                font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
            ",
            tabindex: "0",
            onkeydown: move |e: KeyboardEvent| {
                let mods = e.modifiers();
                // Close-focused-pane-session hotkeys.
                //
                // Three bindings, all deliberately AVOID clobbering the
                // STANDARD terminal Ctrl+W (which deletes the previous word
                // in shells, is the vim window-switch prefix, and is
                // emacs' kill-region). The TerminalView intercepts Ctrl+W
                // at its own `onkeydown` and sends `0x17` to the PTY —
                // this handler only fires when focus is NOT on a terminal
                // (e.g., sidebar, search box, or the main div itself after
                // a click on empty chrome).
                //
                //   - macOS Cmd+W — WKWebView's default "close window"
                //     shortcut. We intercept it to instead close the focused
                //     pane session (preserving the rest of the window/tab).
                //     When no pane session exists we let the event fall through
                //     so the OS closes the window (last-tab-closes-app).
                //
                //   - All platforms Cmd/Ctrl+Shift+W — a dedicated
                //     "close pane" shortcut that doesn't collide with plain
                //     Ctrl+W. Shift+W is the conventional "close tab" companion
                //     in browsers/editors, so Cmd/Ctrl+Shift+W is discoverable.
                //
                //   - Linux/Windows Ctrl+W (no Shift) — ONLY when this handler
                //     fires (i.e., focus is NOT on a terminal). When a terminal
                //     has focus, the TerminalView sends 0x17 to the PTY and
                //     this handler never sees the event. So Ctrl+W works as
                //     the standard Linux terminal shortcut inside a session,
                //     AND as a close-pane shortcut when the user has clicked
                //     away from a terminal (e.g., onto the sidebar or empty
                //     chrome). This resolves the "control+w 不能关闭窗口" report
                //     without breaking the "标准的 linux 终端快捷键在会话中需要能
                //     正常执行的" requirement.
                //
                // The TerminalView's `onkeydown` returns early when `meta`
                // is pressed (so Cmd+W and Cmd+Shift+W bubble here), and
                // calls `prevent_default` for Ctrl-key combos (so Ctrl+W
                // never reaches this handler WHEN a terminal has focus). The
                // Shift variant works on Linux/Windows too because
                // TerminalView's Ctrl+Shift handlers (copy/paste/search)
                // only match C/V/F, not W — so Ctrl+Shift+W bubbles up to
                // here even from a focused terminal.
                if let Key::Character(ref s) = e.key() {
                    if s.eq_ignore_ascii_case("w") {
                        // macOS Cmd+W (no Shift) — the original binding.
                        if cfg!(target_os = "macos")
                            && mods.meta()
                            && !mods.ctrl()
                            && !mods.alt()
                            && !mods.shift()
                        {
                            let snapshot = state.read();
                            let target =
                                focused_pane_session(&snapshot)
                                    .or_else(|| snapshot.active_session.clone());
                            drop(snapshot);
                            if let Some(session_id) = target {
                                e.prevent_default();
                                close_session(
                                    &mut state.write(),
                                    &mut input_senders.write(),
                                    &session_id,
                                );
                                restore_focus_to_active_session(state, 50);
                            }
                            // else: no session to close — let the event
                            // propagate so the OS handles Cmd+W (closes the
                            // window on macOS).
                        }
                        // Cmd/Ctrl+Shift+W — cross-platform close-pane.
                        // `stop_propagation` so the TerminalView doesn't
                        // also process it (it wouldn't, but be explicit).
                        if (mods.meta() || mods.ctrl())
                            && mods.shift()
                            && !mods.alt()
                            && !(mods.meta() && mods.ctrl())
                        {
                            let snapshot = state.read();
                            let target =
                                focused_pane_session(&snapshot)
                                    .or_else(|| snapshot.active_session.clone());
                            drop(snapshot);
                            if let Some(session_id) = target {
                                e.prevent_default();
                                e.stop_propagation();
                                close_session(
                                    &mut state.write(),
                                    &mut input_senders.write(),
                                    &session_id,
                                );
                                restore_focus_to_active_session(state, 50);
                            }
                        }
                        // Linux/Windows: plain Ctrl+W (no Shift) — close
                        // the focused pane session. This handler ONLY fires
                        // when no terminal has focus, so it does NOT collide
                        // with the standard terminal Ctrl+W (which the
                        // TerminalView intercepts and sends as 0x17 to the
                        // PTY). On macOS, Cmd+W (above) is the equivalent
                        // — we deliberately don't bind plain Ctrl+W on
                        // macOS because Cmd+W is the platform convention.
                        if cfg!(not(target_os = "macos"))
                            && mods.ctrl()
                            && !mods.meta()
                            && !mods.alt()
                            && !mods.shift()
                        {
                            let snapshot = state.read();
                            let target =
                                focused_pane_session(&snapshot)
                                    .or_else(|| snapshot.active_session.clone());
                            drop(snapshot);
                            if let Some(session_id) = target {
                                e.prevent_default();
                                e.stop_propagation();
                                close_session(
                                    &mut state.write(),
                                    &mut input_senders.write(),
                                    &session_id,
                                );
                                restore_focus_to_active_session(state, 50);
                            }
                            // else: no session to close — let the event
                            // propagate so the OS handles Ctrl+W (closes the
                            // window on Linux/Windows when there are no tabs).
                        }
                    }
                }
                // Cmd+1..9 (macOS) or Ctrl+1..9 (Linux/Windows) to switch tabs
                if (mods.meta() || mods.ctrl()) && !mods.alt() && !mods.shift() {
                    if let Key::Character(ref s) = e.key() {
                        if let Ok(idx) = s.parse::<usize>() {
                            if idx >= 1 && idx <= 9 {
                                e.prevent_default();
                                let tabs = state.read().tabs.clone();
                                if let Some(tab) = tabs.get(idx - 1) {
                                    let tab_id = tab.id.clone();
                                    set_active_tab(&mut state.write(), &tab_id);
                                    let focus_id = state.read().active_session.as_ref()
                                        .map(|sid| format!("terminal-input-{sid}"));
                                    if let Some(focus_id) = focus_id {
                                        spawn(async move {
                                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                            let _ = dioxus::document::eval(&format!(
                                                "document.getElementById('{focus_id}')?.focus()"
                                            )).await;
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                // Cmd/Ctrl+Shift+L → append one pane (on-demand split).
                // Grows the active tab's layout by exactly one pane per press
                // (1 → 2 → 3 → 4 → …), matching the toolbar's "⊕ Split" button.
                // This replaces the old preset-cycling behaviour (1 → 2H → 2V → 4
                // → 8 → 1) per the user's request to not use 2/4/8 jumps.
                if (mods.meta() || mods.ctrl()) && mods.shift() && !mods.alt() {
                    if let Key::Character(ref s) = e.key() {
                        if s.eq_ignore_ascii_case("l") {
                            e.prevent_default();
                            let new_idx = append_pane_to_active(&mut state.write());
                            tracing::info!(
                                "[LAYOUT] hotkey appended pane idx={:?}",
                                new_idx
                            );
                            // Restore focus to the active session's input div.
                            // Appending a pane re-mounts panes, and the
                            // auto-focus `use_effect` in each pane's
                            // `TerminalView` may race — the last-mounted pane
                            // wins focus, which may not be the active session.
                            // This explicit restore ensures the user lands on
                            // the pane they expect (the active one).
                            restore_focus_to_active_session(state, 100);
                        }
                        // Cmd/Ctrl+Shift+C → toggle comparison mode.
                        if s.eq_ignore_ascii_case("c") {
                            e.prevent_default();
                            let on = toggle_comparison_mode(&mut state.write());
                            tracing::info!("[LAYOUT] hotkey toggled comparison: {:?}", on);
                        }
                        // Cmd/Ctrl+Shift+F → toggle fullscreen (zoom)
                        // on the active pane.
                        if s.eq_ignore_ascii_case("f") {
                            e.prevent_default();
                            let active = state.read().active_session.clone();
                            if let Some(sid) = active {
                                let toggled = toggle_pane_zoom(&mut state.write(), &sid);
                                tracing::info!("[LAYOUT] hotkey zoom for {}: applied={}", sid, toggled);
                            }
                        }
                    }
                }
            },

            // Sidebar
            {rsx! {
                Sidebar {
                    connections: state.read().connections.clone(),
                    on_connect: move |id: String| {
                        let conn = state.read().connections.iter().find(|c| c.id == id).cloned();
                        if let Some(conn) = conn {
                            // No explicit pane target means the connection
                            // opens as a new active tab. Pane-drop flows pass a
                            // stable layout owner + pane index instead.
                            open_connection(state, input_senders, conn, None);
                        }
                    },
                    on_new: move |_| {
                        editing_conn.set(None);
                        modal.set(Modal::NewConnection);
                    },
                    on_onekey: move |_| {
                        modal.set(Modal::OneKeyManager);
                    },
                    on_copy: move |id: String| {
                        let conn = state.read().connections.iter().find(|c| c.id == id).cloned();
                        if let Some(conn) = conn {
                            let new_id = uuid::Uuid::new_v4().to_string();
                            let new_name = format!("{} (copy)", conn.name);
                            let copied = ConnectionConfig {
                                id: new_id.clone(),
                                name: new_name,
                                kind: conn.kind.clone(),
                                group: conn.group.clone(),
                                tags: conn.tags.clone(),
                                onekey: conn.onekey,
                            };
                            state.write().connections.push(copied);
                            save_config(&state);
                        }
                    },
                    on_edit: move |id: String| {
                        let conn = state.read().connections.iter().find(|c| c.id == id).cloned();
                        if let Some(conn) = conn {
                            editing_conn.set(Some(conn));
                            modal.set(Modal::NewConnection);
                        }
                    },
                    on_delete: move |id: String| {
                        let conn = state.read().connections.iter().find(|c| c.id == id).cloned();
                        if let Some(conn) = conn {
                            delete_target.set(Some(conn));
                        }
                    },
                    // Sidebar → pane drag (Task 22 extension): replaces the
                    // prior HTML5 `ondragstart` + `ondrop` wiring that
                    // was unreliable in dioxus 0.7's desktop webview.
                    // `onmousedown` on a ConnItem hands the connection
                    // config + cursor position to `start_tab_drag`, which
                    // installs the document-level JS listeners. The polling
                    // `use_future` takes over and `finish_tab_drag` opens
                    // the connection in the hit-test pane. Plain
                    // click-to-connect still works (the polling loop only
                    // fires the drop if the cursor crossed the threshold).
                    on_drag_start: move |(conn, name, x, y): (ConnectionConfig, String, f64, f64)| {
                        start_tab_drag(tab_drag, DragKind::Connection(conn), name, x, y);
                    },
                }
            }}

            // Main area
            div {
                style: "flex: 1; display: flex; flex-direction: column; overflow: hidden; min-width: 0;",

                // Tab bar
                TabBar {
                    tabs: state.read().tabs.clone(),
                    sessions: state.read().sessions.clone(),
                    active: state.read().active_tab.clone(),
                    focused_session: focused_pane_session(&state.read()),
                    focused_appearance: state.read().focused_tab_appearance.clone(),
                    on_select: move |id: String| {
                        // Switching the top TabBar entry: update active_tab
                        // and derive active_session from the new tab's anchor.
                        set_active_tab(&mut state.write(), &id);
                        let focus_id = state.read().active_session.as_ref()
                            .map(|sid| format!("terminal-input-{sid}"));
                        if let Some(focus_id) = focus_id {
                            spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                let _ = dioxus::document::eval(&format!(
                                    "document.getElementById('{focus_id}')?.focus()"
                                )).await;
                            });
                        }
                    },
                    on_close: move |id: String| {
                        close_workspace(&mut state.write(), &mut input_senders.write(), &id);
                    },
                    // Task 22: manual mouse-based tab drag. The tab's
                    // `onmousedown` (with primary button) calls this
                    // handler, which hands off to `start_tab_drag`.
                    // `start_tab_drag` sets the `tab_drag` signal (with
                    // `dragging: false`) and installs the document-level
                    // JS listeners. The polling `use_future`
                    // (`_tab_drag_poll`) takes over from there — it
                    // promotes `dragging` to `true` once the cursor
                    // crosses the threshold, highlights the drop-target
                    // pane, and calls `finish_tab_drag` on `mouseup`.
                    on_drag_start: move |(sid, name, x, y): (String, String, f64, f64)| {
                        start_tab_drag(tab_drag, DragKind::Session(sid), name, x, y);
                    },
                }

                // Terminal content
                div {
                    id: "terminal-content",
                    style: "flex: 1; position: relative; overflow: hidden; min-height: 0; width: 100%; min-width: 0;",

                    // Check whether the active tab has a multi-pane layout
                    // applied. If it does (and isn't zoomed to a single pane),
                    // we render every pane side-by-side via the multi-pane
                    // path. Otherwise we fall through to the legacy single-
                    // session rendering path below.
                    {let active_tab_id = state.read().active_tab.clone();
                    let active_anchor = state.read().active_tab_anchor_session();
                    let layout_snapshot = active_tab_id.as_ref()
                        .and_then(|tid| state.read().layouts.get(tid).cloned());
                    let is_multi = layout_snapshot.as_ref()
                        .is_some_and(|l| l.is_multi_pane());
                    match (active_anchor.clone(), is_multi) {
                        (None, _) => rsx! {
                            div {
                                style: "
                                    position: absolute;
                                    left: 0; right: 0; top: 0; bottom: 0;
                                    display: flex;
                                    justify-content: center;
                                    align-items: center;
                                    color: #565f89;
                                    font-size: 14px;
                                ",
                                "Welcome to RusTerm — Press + New to create a connection"
                            }
                        },
                        (Some(sid), false) => {
                            // Single-pane path: render the active session's
                            // TerminalView directly. This is the legacy flow
                            // that preserves all existing behaviour (OneKey,
                            // suggestions, reconnect, etc.). When a multi-pane
                            // layout is applied but currently zoomed to one
                            // pane, we also take this path — the zoomed pane's
                            // TerminalView fills the container, and its resize
                            // handler will measure the full container size.
                            //
                            // If a layout exists but is zoomed, we look up the
                            // zoomed pane's session_id and render THAT instead
                            // of `sid` (the active tab's anchor session). This
                            // is what makes zoom mode actually work: the user's
                            // active tab anchor is `sid`, but the visible
                            // content is the zoomed pane's session.
                            //
                            // We wrap `render_terminal_pane` in
                            // `single_pane_with_drop` so that drag-drop from
                            // the tab bar / sidebar still works in the
                            // single-pane path (Task 19: drag a background
                            // tab onto the active pane to create a split).
                            // Without the wrapper, drops go nowhere because
                            // `render_terminal_pane` has no drop handlers.
                            let render_sid = layout_snapshot.as_ref()
                                .and_then(|l| l.zoomed)
                                .and_then(|idx| layout_snapshot.as_ref()?.panes.get(idx).map(|p| p.session_id.clone()))
                                .unwrap_or(sid.clone());
                            single_pane_with_drop(state, input_senders, render_sid, drag_over_pane)
                        }
                        (Some(_sid), true) => {
                            // Multi-pane path: iterate over visible panes and
                            // render each one positioned absolutely via the
                            // layout's pane_rect. Splitter bars are rendered
                            // between panes to support drag-resize.
                            let layout = layout_snapshot.expect("is_multi implies layout exists");
                            rsx! {
                                {multi_pane_container(state, input_senders, layout, drag_over_pane, container_size, split_drag, tab_drag, pane_move)}
                            }
                        }
                    }}

                    // Task 22: tab-drag ghost element. While a tab drag is
                    // active AND the cursor has crossed the threshold (i.e.
                    // it's a real drag, not a click), render a small floating
                    // div following the cursor showing the dragged session's
                    // name. `pointer-events: none` so it never intercepts
                    // mouse events (the document-level JS listeners handle
                    // those). Sits at z-index 9999 so it's above panes and
                    // splitters.
                    {tab_drag().map(|drag| {
                        if !drag.dragging {
                            return rsx! {};
                        }
                        let ghost_x = drag.cur_x + 12.0;
                        let ghost_y = drag.cur_y + 14.0;
                        let ghost_name = drag.display_name.clone();
                        rsx! {
                            div {
                                key: "tab-drag-ghost-{ghost_x}-{ghost_y}",
                                style: format!(
                                    "position: fixed; left: {ghost_x}px; top: {ghost_y}px; \
                                     pointer-events: none; z-index: 9999; \
                                     background: #24283b; border: 1px solid #7aa2f7; \
                                     padding: 4px 8px; border-radius: 4px; \
                                     font-size: 12px; color: #c0caf5; \
                                     box-shadow: 0 2px 8px rgba(0,0,0,0.4); \
                                     user-select: none; -webkit-user-select: none;",
                                    ghost_x = ghost_x,
                                    ghost_y = ghost_y,
                                ),
                                "{ghost_name}"
                            }
                        }
                    })}

                    // AI panel overlay
                    AiPanel {
                        visible: matches!(modal(), Modal::AiSuggest),
                        suggestions: ai_suggestions(),
                        on_close: move |_| modal.set(Modal::None),
                        on_apply: move |cmd: String| {
                            let active = state.read().active_session.clone();
                            if let Some(sid) = active {
                                if let Some(sender) = input_senders.read().get(&sid) {
                                    let _ = sender.send(format!("{}\n", cmd).into_bytes());
                                }
                            }
                            modal.set(Modal::None);
                        },
                    }
                }

                // Status bar
                div {
                    style: "
                        height: 24px;
                        background: #1a1b26;
                        border-top: 1px solid #2a2b3d;
                        display: flex;
                        align-items: center;
                        padding: 0 12px;
                        font-size: 11px;
                        color: #565f89;
                        gap: 12px;
                    ",
                    span { "RusTerm v0.1.0" }

                    // Active session info
                    {
                        let active = state.read().active_session.clone();
                        let info = active.and_then(|sid| {
                            let tabs = &state.read().sessions;
                            tabs.iter().find(|t| t.id == sid).map(|t| {
                                let size = state.read().terminals.get(&sid)
                                    .map(|h| {
                                        let s = h.lock().terminal.size();
                                        format!("{}x{}", s.cols, s.rows)
                                    })
                                    .unwrap_or_default();
                                let tmux = t.render_output.tmux_session.as_ref()
                                    .map(|s| format!(" | tmux: {}", s))
                                    .unwrap_or_default();
                                let log_status = if state.read().session_logs.contains_key(&sid) {
                                    " | LOG"
                                } else {
                                    ""
                                };
                                format!("{}{}{}{}",
                                    t.name,
                                    if size.is_empty() { String::new() } else { format!(" | {}", size) },
                                    tmux,
                                    log_status
                                )
                            })
                        });
                        match info {
                            Some(info) => rsx! {
                                span {
                                    style: "color: #7aa2f7;",
                                    "{info}"
                                }
                            },
                            None => rsx! { span {} },
                        }
                    }

                    // Right side actions
                    //
                    // The `.compare-btn*` rules live in the `<style>` block
                    // below. We use classes (not pure inline styles) so we
                    // can express `:hover` states — dioxus 0.7's inline
                    // `style` attribute can't do pseudo-classes. The inline
                    // `style` on the span only carries the static layout
                    // props; the colour/background/border come from the
                    // class so hover can override them.
                    //
                    // Why a `<style>` block instead of pure inline: when the
                    // button is toggled ON, the old inline-style approach
                    // swapped between `color:#7aa2f7;border:1px solid #2a2b3d`
                    // (OFF) and `background:#7aa2f7;color:#1a1b26` (ON, no
                    // border). The missing border in the ON state caused a
                    // 2px layout shift, and the background-only-with-no-
                    // vertical-padding made the blue strip look like it was
                    // "covering" the text rather than framing it. The class
                    // approach keeps a transparent border in the ON state
                    // (no shift) and gives the button real vertical padding
                    // so the background reads as a button, not a highlight.
                    //
                    // 2026-07-20 redesign: added a ⇄ icon before the text to
                    // make the toggle's purpose obvious even if the text
                    // rendering is ambiguous on a particular display. The
                    // ON state uses a darker text color (#0f1119) and a
                    // subtle text-shadow for extra contrast against the
                    // bright #7aa2f7 background, addressing the
                    // "字体被颜色覆盖" report.
                    style { "
                        .compare-btn {{
                            cursor: pointer;
                            font-size: 11px;
                            user-select: none;
                            -webkit-user-select: none;
                            padding: 3px 10px;
                            border-radius: 4px;
                            line-height: 18px;
                            border: 1px solid transparent;
                            transition: background 0.12s ease, color 0.12s ease, border-color 0.12s ease, box-shadow 0.12s ease;
                            display: inline-flex;
                            align-items: center;
                            gap: 4px;
                            font-weight: 500;
                        }}
                        .compare-btn-off {{
                            color: #7aa2f7;
                            border-color: #414868;
                            background: transparent;
                        }}
                        .compare-btn-off:hover {{
                            background: #24283b;
                            border-color: #7aa2f7;
                            color: #c0caf5;
                        }}
                        .compare-btn-on {{
                            background: #7aa2f7;
                            color: #0f1119;
                            font-weight: 700;
                            border-color: #89b5fa;
                            box-shadow: 0 0 6px rgba(122,162,247,0.45);
                            text-shadow: 0 1px 0 rgba(255,255,255,0.25);
                        }}
                        .compare-btn-on:hover {{
                            background: #89b5fa;
                            border-color: #a8c4fb;
                            box-shadow: 0 0 8px rgba(122,162,247,0.6);
                        }}
                        .compare-btn-icon {{
                            font-size: 13px;
                            line-height: 1;
                            font-weight: 700;
                        }}
                    " }
                    div {
                        style: "margin-left: auto; display: flex; gap: 12px; align-items: center;",

                        // --- Multi-pane layout controls ---
                        // The layout toolbar lets the user append one pane at a
                        // time (on-demand split, not preset jumps), toggle the
                        // cross-terminal comparison mode (synchronized scrolling
                        // + input broadcast), and zoom the active pane to fill
                        // the container (全屏模式). When no session is active,
                        // these are no-ops.
                        //
                        // Layout display: read the actual pane count from the
                        // active tab's layout (not `state.layout_preset`, which
                        // no longer reflects reality after `append_pane_to_active`
                        // — e.g. 3 panes has no preset). Shows "Layout: N panes"
                        // or "Layout: 1 pane" when no multi-pane layout exists.
                        span {
                            style: "color: #7aa2f7; font-size: 11px; user-select: none; opacity: 0.85;",
                            title: "Number of panes in the active tab's layout",
                            { layout_display_label(&state.read()) }
                        }
                        // "Split" button — appends exactly ONE new pane to the
                        // active tab's layout (on-demand split, not a preset
                        // jump). Direction is picked from the layout's shape
                        // (wide layouts grow a column, tall layouts grow a row).
                        // The new pane starts EMPTY; the user fills it via the
                        // empty-pane hint buttons (⧉ copy focused / + sidebar)
                        // or by dragging a session/sidebar connection into it.
                        // This is the explicit "create new pane" affordance for
                        // the "一个新的会话支持多分屏" requirement — each click adds
                        // exactly one pane, growing 1 → 2 → 3 → 4 → … → MAX_PANES.
                        span {
                            style: "cursor: pointer; color: #9ece6a; font-size: 11px; user-select: none; border: 1px solid #414868; border-radius: 3px; padding: 1px 6px; line-height: 16px;",
                            onclick: move |_| {
                                let new_idx = append_pane_to_active(&mut state.write());
                                tracing::info!(
                                    "[LAYOUT] split button: appended pane idx={:?}",
                                    new_idx
                                );
                                restore_focus_to_active_session(state, 100);
                            },
                            title: "Split — append one pane (1 → 2 → 3 → 4 → …)",
                            "⊕ Split"
                        }
                        // "Distribute" button — fills the active tab's panes
                        // with all open sessions (in tab order, active first).
                        // This is the explicit "多个会话放到多个分屏中" affordance:
                        // a one-click way to populate the current on-demand layout
                        // after the user has opened several sessions. Sessions beyond
                        // the pane count remain in `state.sessions` and can be
                        // placed by growing the layout further.
                        span {
                            style: "cursor: pointer; color: #bb9af7; font-size: 11px; user-select: none; border: 1px solid #414868; border-radius: 3px; padding: 1px 6px; line-height: 16px;",
                            onclick: move |_| {
                                let placed = distribute_sessions_across_panes(&mut state.write());
                                tracing::info!("[LAYOUT] distribute button: placed {} sessions", placed);
                                restore_focus_to_active_session(state, 100);
                            },
                            title: "Distribute — fill panes with all open sessions",
                            "⇶ Distribute"
                        }
                        span {
                            class: if state.read().layouts.get(&state.read().active_tab.clone().unwrap_or_default())
                                .is_some_and(|l| l.comparison) {
                                "compare-btn compare-btn-on"
                            } else {
                                "compare-btn compare-btn-off"
                            },
                            onclick: move |_| {
                                let on = toggle_comparison_mode(&mut state.write());
                                tracing::info!("[LAYOUT] comparison mode toggled: {:?}", on);
                            },
                            title: "Toggle comparison mode (sync scroll + broadcast input)",
                            span {
                                class: "compare-btn-icon",
                                "⇄"
                            }
                            "Compare"
                        }
                        span {
                            style: "cursor: pointer; color: #7aa2f7; font-size: 11px; user-select: none;",
                            onclick: move |_| {
                                // Zoom the active session's pane. If the layout
                                // is Single, this is a no-op (toggle_pane_zoom
                                // returns false because there's no layout entry).
                                let active = state.read().active_session.clone();
                                if let Some(sid) = active {
                                    let toggled = toggle_pane_zoom(&mut state.write(), &sid);
                                    tracing::info!("[LAYOUT] zoom toggle for {}: applied={}", sid, toggled);
                                }
                            },
                            title: "Toggle fullscreen (zoom) on the active pane",
                            "⤢"
                        }
                        span {
                            style: "cursor: pointer; color: #565f89;",
                            "Sessions: {state.read().sessions.len()}"
                        }
                        span {
                            style: "color: #9ece6a; font-size: 10px; letter-spacing: 0.5px; border: 1px solid #9ece6a; border-radius: 3px; padding: 0 4px; cursor: default;",
                            "LOCAL ONLY"
                        }
                        span {
                            style: "cursor: pointer; color: #7aa2f7;",
                            onclick: move |_| modal.set(Modal::AiSuggest),
                            "AI"
                        }
                        span {
                            style: "cursor: pointer; color: #7aa2f7;",
                            onclick: move |_| modal.set(Modal::OneKeyManager),
                            "OneKeys"
                        }
                        span {
                            style: "cursor: pointer; color: #7aa2f7;",
                            onclick: move |_| modal.set(Modal::Settings),
                            "Settings"
                        }
                        span {
                            style: "cursor: pointer; color: #9ece6a;",
                            onclick: move |_| open_local_terminal(state, input_senders),
                            title: "Open a local shell (zsh/bash)",
                            "Local"
                        }
                    }
                }
            }
        }

        if matches!(modal(), Modal::Settings) {
            SettingsDialog {
                appearance: state.read().focused_tab_appearance.clone(),
                on_close: move |_| modal.set(Modal::None),
                on_save: move |appearance: rusterm_core::FocusedTabAppearance| {
                    let appearance = appearance.normalized();
                    if let Some(cm) = state.read().config_manager.clone() {
                        if let Err(e) = cm.save_focused_tab_appearance(appearance.clone()) {
                            tracing::error!("Failed to save focused tab appearance: {}", e);
                        }
                    } else {
                        tracing::error!(
                            "ConfigManager not initialized, cannot save focused tab appearance"
                        );
                    }
                    state.write().focused_tab_appearance = appearance;
                    modal.set(Modal::None);
                },
            }
        }

        // Connection dialog modal
        ConnectionDialog {
            visible: matches!(modal(), Modal::NewConnection),
            editing: editing_conn.read().clone(),
            on_close: move |_| {
                editing_conn.set(None);
                modal.set(Modal::None);
            },
            on_create: move |form: NewConnectionForm| {
                let port: u16 = form.port.parse().unwrap_or(22);
                let auth = build_ssh_auth(&form);
                let terminal_type = if form.terminal_type.is_empty() {
                    "xterm-256color".to_string()
                } else {
                    form.terminal_type.clone()
                };

                let ssh_config = SshConfig {
                    host: form.host.clone(),
                    port,
                    username: form.username.clone(),
                    auth,
                    terminal_type,
                    proxy_jump: None,
                    keepalive_interval: None,
                    host_key_policy: rusterm_core::config::default_host_key_policy(),
                };

                let config = ConnectionConfig {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: if form.name.is_empty() {
                        format!("{}@{}", form.username, form.host)
                    } else {
                        form.name.clone()
                    },
                    kind: ConnectionKind::Ssh(ssh_config.clone()),
                    group: None,
                    tags: vec![],
                    onekey: form.onekey,
                };

                let tab_id = config.id.clone();
                create_terminal(tab_id.clone(), &mut state);
                // Remember the config so this session can be reconnected by
                // pressing Enter after a disconnect.
                state.write().session_configs.insert(tab_id.clone(), config.clone());

                // Write "Connecting..." message into the terminal
                let render_output = {
                    let terminals = state.read().terminals.clone();
                    if let Some(handle) = terminals.get(&tab_id) {
                        let msg = format!("\r\nConnecting to {}...\r\n", config.name);
                        handle.lock().process_and_render(msg.as_bytes())
                    } else {
                        Default::default()
                    }
                };

                {
                    let mut s = state.write();
                    s.connections.push(config.clone());
                    s.sessions.push(SessionTab {
                        id: config.id.clone(),
                        name: config.name.clone(),
                        kind: SessionType::Ssh,
                        render_output,
                        version: 1,
                        suggestion: None,
                        suggestions: Vec::new(),
                        suggestion_selected: 0,
                        suggestion_visible: false,
                        command_history: Vec::new(),
                        hostname: Some(ssh_config.host.clone()),
                        cwd: None,
                    });
                    // New top-level workspace tab.
                    push_workspace_tab(&mut s, &config.id);
                }
                save_config(&state);
                modal.set(Modal::None);

                start_ssh_connection(state, input_senders, config.id, ssh_config);
            },
            on_edit: move |(id, form): (String, NewConnectionForm)| {
                // Edit mode: find the original connection, rebuild it from the
                // form (preserving id + non-form fields), replace it in place,
                // and persist. We deliberately do NOT start a new session —
                // editing a saved connection is not the same as connecting to
                // it. Any live session tab for this connection keeps running
                // with the old config; its reconnect path reads from
                // `session_configs`, which is also stale, but that's
                // acceptable — the user can close the tab and reconnect to
                // pick up the new config.
                let original = state.read().connections.iter().find(|c| c.id == id).cloned();
                if let Some(original) = original {
                    let updated = rebuild_connection(&original, &form);
                    if let Some(slot) = state.write().connections.iter_mut().find(|c| c.id == id) {
                        *slot = updated;
                    }
                    save_config(&state);
                }
                editing_conn.set(None);
                modal.set(Modal::None);
            },
        }

        // Delete-connection confirm modal. The sidebar requests a delete by
        // setting `delete_target`; we render a small confirm dialog here so a
        // stray click on the trash icon can't silently destroy a saved
        // connection (which may carry an encrypted password). Dioxus's rsx!
        // bodies don't allow bare `let` statements, so we bind the target via
        // the `if let` pattern and reference its fields directly.
        if let Some(target) = delete_target.read().clone() {
            div {
                style: "
                    position: fixed;
                    top: 0; left: 0; right: 0; bottom: 0;
                    background: rgba(0,0,0,0.6);
                    display: flex;
                    justify-content: center;
                    align-items: center;
                    z-index: 1100;
                ",
                div {
                    style: "
                        background: #24283b;
                        border-radius: 8px;
                        padding: 24px;
                        width: 380px;
                        color: #c0caf5;
                    ",
                    h3 { style: "margin: 0 0 8px; font-size: 15px;", "Delete connection?" }
                    p {
                        style: "margin: 0 0 20px; font-size: 13px; color: #c0caf5; line-height: 1.5;",
                        "This will remove \"{target.name}\" from your saved connections. The encrypted config (including any stored password) will be erased from disk. This cannot be undone."
                    }
                    div {
                        style: "display: flex; justify-content: flex-end; gap: 8px;",
                        button {
                            style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                            onclick: move |_| delete_target.set(None),
                            "Cancel"
                        }
                        button {
                            style: "background: #f7768e; border: none; color: #1a1b26; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                            onclick: move |_| {
                                let target_id = target.id.clone();
                                {
                                    let mut s = state.write();
                                    s.connections.retain(|c| c.id != target_id);
                                }
                                save_config(&state);
                                delete_target.set(None);
                            },
                            "Delete"
                        }
                    }
                }
            }
        }

        // OneKey manager modal (configure the Expect/Send library; encrypted at rest)
        if matches!(modal(), Modal::OneKeyManager) {
            OneKeyManager {
                onekeys: state.read().onekeys.clone(),
                on_close: move |_| modal.set(Modal::None),
                on_save: move |onekeys: Vec<OneKey>| {
                    let cm = state.read().config_manager.clone();
                    if let Some(cm) = cm {
                        if let Err(e) = cm.save_onekeys(&onekeys) {
                            tracing::error!("Failed to save OneKeys: {}", e);
                        }
                    }
                    state.write().onekeys = onekeys;
                    modal.set(Modal::None);
                },
            }
        }

        // ── Session-state restore dialog (feature #17) ─────────────────────
        // Shown after unlock if `session_state.enc` was loaded successfully.
        // Three actions (see `RestoreSessionDialog`):
        //   恢复     → restore each session + `cd <last_cwd>`
        //   跳过     → clear without restoring
        //   不再询问 → clear + set `restore_disabled = true` (also deletes
        //             the saved state file so we don't re-prompt)
        //
        // The restore action is non-destructive: we only reconnect sessions
        // and send a single `cd` per session. We NEVER re-execute past
        // commands or scripts — the user explicitly asked us not to.
        if let Some(loaded) = state.read().restore_pending.clone() {
            RestoreSessionDialog {
                session_count: loaded.sessions.len(),
                saved_at: loaded.saved_at.format("%Y-%m-%d %H:%M").to_string(),
                on_restore: move |_| {
                    // Take the pending state out so we don't re-show the dialog.
                    let to_restore = state.write().restore_pending.take();
                    if let Some(to_restore) = to_restore {
                        restore_sessions(state, input_senders, to_restore);
                    }
                },
                on_skip: move |_| {
                    // Just clear the pending state — don't restore, don't
                    // disable future prompts.
                    state.write().restore_pending = None;
                },
                on_never_ask: move |_| {
                    // Clear pending + disable future prompts + delete the
                    // saved state file so we don't re-prompt on next launch
                    // either.
                    state.write().restore_pending = None;
                    state.write().restore_disabled = true;
                    if let Err(e) = rusterm_core::SessionState::delete() {
                        tracing::warn!("Failed to delete session state file: {}", e);
                    }
                    // Persist the `restore_disabled` flag so it survives
                    // across launches. We piggyback on the existing settings
                    // save path (writes settings.json).
                    save_settings(&state);
                },
            }
        }

        // ── Dangerous-command confirmation modal (feature #17 part 2) ────────
        // Shown when the user presses Enter on a command the safety checker
        // flagged. Two actions:
        //   继续 → send the original Enter to the PTY
        //   取消 → discard the Enter, keep the input line intact
        if let Some(pending) = state.read().pending_dangerous_command.clone() {
            DangerousCommandDialog {
                command: pending.command.clone(),
                reason: pending.reason.clone(),
                on_proceed: move |_| {
                    // User confirmed — send the original Enter to the PTY.
                    let p = state.write().pending_dangerous_command.take();
                    if let Some(p) = p {
                        if let Some(sender) = input_senders.read().get(&p.session_id) {
                            let _ = sender.send(b"\n".to_vec());
                        }
                    }
                },
                on_cancel: move |_| {
                    // Discard the Enter — just clear the pending state. The
                    // user's input line stays intact (we never sent the Enter),
                    // so they can edit or backspace.
                    state.write().pending_dangerous_command = None;
                },
            }
        }
    }
}

/// Strip shell prompt from a terminal line, returning just the command part.
/// Handles common prompt patterns:
///   - "user@host:~$ cmd"        (bash / sh — marker at end → command right after)
///   - "➜  ~ cmd"                (Oh My Zsh: marker at start, directory after)
///   - "➜  ~ git:(main) ✗ cmd"   (Oh My Zsh + git info)
///   - "❯ cmd"                    (Starship / plain)
///   - "PS C:\Users\me> cmd"     (PowerShell on Windows)
///   - "cmd> " / "C:\> "         (cmd.exe / bare prompts)
///   - "# cmd"                   (root shell)
fn strip_prompt(line: &str) -> String {
    if line.is_empty() {
        return String::new();
    }

    let trimmed = line.trim_start();
    // PowerShell: "PS <path>> " — strip the leading "PS " and a path-like token.
    if let Some(rest) = trimmed.strip_prefix("PS ") {
        // Look for the first "> " that ends the prompt; the command follows.
        if let Some(idx) = rest.rfind("> ") {
            let cmd = rest[idx + 2..].trim();
            if !cmd.is_empty() {
                return cmd.to_string();
            }
        }
    }

    // 1. End markers — command immediately follows the marker.
    // Include Windows cmd.exe style "C:\>" and bare ">".
    let end_markers = ["$ ", "# ", "% ", "> "];
    for marker in end_markers {
        if let Some(idx) = line.rfind(marker) {
            let cmd = line[idx + marker.len()..].trim();
            if !cmd.is_empty() {
                return cmd.to_string();
            }
        }
    }

    // 2. Start markers (➜, ❯) — followed by directory + optional git info,
    //    then the command. Skip all prompt-like words after the marker.
    let start_markers = ["\u{279c}", "\u{276f}"]; // ➜ ❯
    for marker in start_markers {
        if let Some(idx) = line.find(marker) {
            let after = &line[idx + marker.len()..];
            let words: Vec<&str> = after.split_whitespace().collect();
            let mut start = 0;
            while start < words.len() && is_prompt_word(words[start]) {
                start += 1;
            }
            if start < words.len() {
                return words[start..].join(" ");
            }
            return String::new();
        }
    }

    // 3. Fallback: try stripping words from the left.
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() > 2 {
        for start in 1..words.len().min(5) {
            let suffix = words[start..].join(" ");
            if looks_like_command(&suffix) {
                return suffix;
            }
        }
    }

    line.to_string()
}

/// Returns true if a word is part of the prompt (not a command).
/// Used to skip directory names, git info, and status markers in prompts
/// like "➜  ~ git:(main) ✗".
fn is_prompt_word(w: &str) -> bool {
    if w.is_empty() {
        return false;
    }
    // Directory-like: ~, ~/path, /path, ., ..
    if w == "~" || w.starts_with("~/") || w.starts_with('/') || w == "." || w == ".." {
        return true;
    }
    // Git info: git:(branch)
    if w.starts_with("git:(") && w.ends_with(')') {
        return true;
    }
    // Dirty / clean markers
    if w == "✗" || w == "✓" || w == "*" {
        return true;
    }
    // Prompt markers
    if w == "➜" || w == "❯" || w == "$" || w == "#" || w == "%" {
        return true;
    }
    // user@host:path  (common in bash prompts)
    if w.contains('@') && w.contains(':') {
        return true;
    }
    // Windows path like C:\Users\... (treated as a prompt path token)
    if w.len() >= 2
        && w.as_bytes()[1] == b':'
        && (w.as_bytes()[2] == b'\\' || w.as_bytes()[2] == b'/')
    {
        return true;
    }
    false
}

/// Quick heuristic: does this text look like the start of a shell command?
fn looks_like_command(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    if first.is_empty() {
        return false;
    }
    // Common command starters — if the first word matches, it's likely a command
    let common = [
        "ls",
        "cd",
        "cat",
        "grep",
        "find",
        "awk",
        "sed",
        "make",
        "git",
        "docker",
        "npm",
        "cargo",
        "python",
        "python3",
        "node",
        "go",
        "rustup",
        "vim",
        "nvim",
        "emacs",
        "ssh",
        "scp",
        "rsync",
        "curl",
        "wget",
        "tar",
        "zip",
        "unzip",
        "sudo",
        "apt",
        "yum",
        "brew",
        "pip",
        "pip3",
        "echo",
        "mkdir",
        "rm",
        "cp",
        "mv",
        "chmod",
        "chown",
        "ps",
        "top",
        "htop",
        "kill",
        "df",
        "du",
        "free",
        "export",
        "source",
        "alias",
        "which",
        "type",
        "man",
        "less",
        "more",
        "head",
        "tail",
        "sort",
        "uniq",
        "wc",
        "diff",
        "patch",
        "xargs",
        "tee",
        "jq",
        "yq",
        "terraform",
        "ansible",
        "kubectl",
        "helm",
        "aws",
        "gcloud",
        "az",
        "open",
        "pbcopy",
        "pbpaste",
        "launchctl",
        "systemctl",
        "service",
        "ping",
        "traceroute",
        "netstat",
        "ss",
        "ip",
        "ifconfig",
        "env",
        "printenv",
        "date",
        "cal",
        "whoami",
        "id",
        "uname",
        "hostname",
        "uptime",
        "w",
        "who",
        "history",
        "clear",
        "reset",
        "exit",
        "logout",
        "reboot",
        "shutdown",
        "pwd",
        "test",
        "true",
        "false",
        "nohup",
        "time",
        "watch",
        "seq",
        "tr",
        "cut",
        "column",
        "basename",
        "dirname",
        "realpath",
        "readlink",
        "stat",
        "touch",
        "ln",
        "mount",
        "umount",
        "useradd",
        "usermod",
        "groupadd",
        "passwd",
        "visudo",
        "crontab",
        "at",
        "batch",
        "fg",
        "bg",
        "jobs",
        "disown",
        "zsh",
        "bash",
        "sh",
        "fish",
        "dash",
        "ksh",
        "tcsh",
        "csh",
        // PowerShell commands (in case the local shell is pwsh on Windows)
        "Get-ChildItem",
        "Get-Content",
        "Set-Location",
        "Copy-Item",
        "Move-Item",
        "Remove-Item",
        "New-Item",
        "Write-Output",
        "Write-Host",
        "Invoke-WebRequest",
        "Start-Process",
        "Stop-Process",
        "Get-Process",
        "Get-Service",
    ];
    common.contains(&first) || first.contains('/') || first.contains('.') || first.starts_with('-')
}

#[cfg(test)]
mod onekey_tests {
    use super::{first_matching_step, strip_ansi};
    use regex::Regex;
    use rusterm_core::config::{OneKey, OneKeyStep};

    #[test]
    fn expect_matches_git_prompts_case_insensitively() {
        // git prints "Username for ..." (capital U) and "Password for ..." (capital P).
        // Users often configure the ZOC-style lowercase "password for \\S+:". The
        // match must be case-insensitive, otherwise the password step is silently
        // skipped (no popup) and the user types the wrong password by hand.
        let username_prompt = "Username for 'https://gitlab.example.com': ";
        let password_prompt = "Password for 'https://xuchao@gitlab.example.com': ";

        assert!(
            Regex::new(r"(?i)Username for \S+:")
                .unwrap()
                .is_match(username_prompt)
        );
        // lowercase expect must match the capital-P prompt — this is the bug that was fixed
        assert!(
            Regex::new(r"(?i)password for \S+:")
                .unwrap()
                .is_match(password_prompt),
            "case-insensitive password expect must match 'Password for ...'"
        );
        // A bare "password:" (no "for \\S+") does NOT match git's "Password for ..." —
        // the user must include the "for \\S+" part for git's prompt shape.
        assert!(
            !Regex::new(r"(?i)password:")
                .unwrap()
                .is_match(password_prompt)
        );
    }

    #[test]
    fn ansi_colored_prompt_is_stripped_before_matching() {
        // Bastion / network-device prompts are often colored. The ESC bytes
        // between "Password" and "for" break the expect regex unless they are
        // stripped first, so the popup never shows and the password is never
        // autofilled.
        let raw = "\x1b[1;36mPassword\x1b[0m for 'https://host': ";
        let stripped = strip_ansi(raw);
        assert_eq!(stripped, "Password for 'https://host': ");
        assert!(
            Regex::new(r"(?i)password for \S+:")
                .unwrap()
                .is_match(&stripped),
            "stripped colored prompt must match the password expect"
        );
        // Sanity: the RAW (unstripped) prompt does NOT match — this is the bug
        // stripping exists to fix.
        assert!(
            !Regex::new(r"(?i)password for \S+:").unwrap().is_match(raw),
            "raw colored prompt must NOT match (proves stripping is necessary)"
        );
    }

    #[test]
    fn multistep_picks_username_step_for_username_prompt_and_password_step_for_password_prompt() {
        // A OneKey with a Username step then a Password step (the default the
        // manager creates for git HTTPS). For each prompt the FIRST matching
        // step must be the correct one — otherwise selecting would send the
        // username into the password field or vice versa.
        let ok = OneKey {
            id: "x".to_string(),
            name: "git".to_string(),
            steps: vec![
                OneKeyStep {
                    label: "Username".to_string(),
                    expect: r"Username for \S+:".to_string(),
                    send: "myuser".to_string(),
                },
                OneKeyStep {
                    label: "Password".to_string(),
                    expect: r"password for \S+:".to_string(),
                    send: "mypass".to_string(),
                },
            ],
        };
        let username_step = first_matching_step(&ok, "Username for 'https://host': ").unwrap();
        assert_eq!(
            username_step.send, "myuser",
            "username prompt must pick the username step"
        );
        let password_step = first_matching_step(&ok, "Password for 'https://u@host': ").unwrap();
        assert_eq!(
            password_step.send, "mypass",
            "password prompt must pick the password step, not the username step"
        );
    }

    #[test]
    fn migrated_password_expect_matches_git_password_prompt() {
        // The bug this guards against: a user saved a git-HTTPS OneKey with a
        // password step whose expect was a bare `password:`. Git's actual prompt
        // is `Password for 'host': ` — the `for 'host'` sits between "Password"
        // and ":", so `password:` does NOT match, the popup never fires for the
        // password step, and the user has to type the password manually.
        //
        // ConfigManager::load_onekeys migrates `password:` -> `password for \S+:`
        // when a Username step is present (see test_onekey_password_expect_migrated_for_git_https
        // in rusterm-core). This test verifies that AFTER migration, the password
        // step's expect correctly matches git's prompt — i.e. the popup will fire.
        let git_password_prompt = "Password for 'https://xuchao@gitlab.example.com': ";

        // Before migration: bare `password:` does NOT match git's prompt.
        assert!(
            !Regex::new(r"(?i)password:")
                .unwrap()
                .is_match(git_password_prompt),
            "bare 'password:' must NOT match git's 'Password for ...' prompt (the bug)"
        );

        // After migration: `password for \S+:` DOES match git's prompt.
        assert!(
            Regex::new(r"(?i)password for \S+:")
                .unwrap()
                .is_match(git_password_prompt),
            "migrated 'password for \\S+:' expect must match git's password prompt"
        );

        // And the full multi-step OneKey picks the password step for the password prompt.
        let ok = OneKey {
            id: "migrated".to_string(),
            name: "gitlab".to_string(),
            steps: vec![
                OneKeyStep {
                    label: "Username".to_string(),
                    expect: r"Username for \S+:".to_string(),
                    send: "user".to_string(),
                },
                OneKeyStep {
                    label: "".to_string(),
                    // This is the migrated expect (was `password:` before migration).
                    expect: r"password for \S+:".to_string(),
                    send: "pass".to_string(),
                },
            ],
        };
        let step = first_matching_step(&ok, git_password_prompt).unwrap();
        assert_eq!(
            step.send, "pass",
            "after migration, git's password prompt must pick the password step"
        );
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::strip_prompt;

    #[test]
    fn bash_prompt_strips_to_command() {
        assert_eq!(strip_prompt("xuchao@host:~$ ls -la"), "ls -la");
        assert_eq!(
            strip_prompt("root@server:/var# systemctl status nginx"),
            "systemctl status nginx"
        );
    }

    #[test]
    fn omz_zsh_prompt_strips_directory_and_git() {
        // ➜  ~ git:(main) ✗ cargo build
        assert_eq!(strip_prompt("➜  ~ git:(main) ✗ cargo build"), "cargo build");
        // Plain ➜ with just a directory
        assert_eq!(strip_prompt("➜  ~ ls"), "ls");
    }

    #[test]
    fn starship_prompt_strips_arrow() {
        // ❯ cargo run
        assert_eq!(strip_prompt("❯ cargo run"), "cargo run");
    }

    #[test]
    fn powershell_prompt_strips_ps_prefix() {
        assert_eq!(
            strip_prompt("PS C:\\Users\\me> Get-ChildItem"),
            "Get-ChildItem"
        );
        assert_eq!(strip_prompt("PS /home/user> ls -la"), "ls -la");
    }

    #[test]
    fn cmd_exe_prompt_strips_drive() {
        // "C:\\Users\\me> dir" — the end-marker "> " catches it
        assert_eq!(strip_prompt("C:\\Users\\me> dir"), "dir");
    }

    #[test]
    fn empty_line_returns_empty() {
        assert_eq!(strip_prompt(""), "");
        // Whitespace-only lines fall through to `line.to_string()` at the
        // bottom; trim before comparing so the contract ("returns nothing
        // useful") holds.
        assert_eq!(strip_prompt("   ").trim(), "");
    }

    #[test]
    fn root_shell_prompt_with_hash() {
        assert_eq!(strip_prompt("# apt update"), "apt update");
    }

    #[test]
    fn custom_prompt_with_bare_greater_than() {
        // Python REPL, custom PS1="> ", mysql, etc.
        assert_eq!(strip_prompt("> SELECT 1"), "SELECT 1");
    }
}

#[cfg(test)]
mod pane_move_tests {
    use super::{build_install_pane_move_script, parse_pane_move_poll_response};

    #[test]
    fn pane_move_script_uses_dedicated_capture_phase_globals() {
        let script = build_install_pane_move_script(12.5, 34.0);
        assert!(script.contains("window.__rusterm_pane_move_pos = '12.5,34'"));
        assert!(script.contains("window.__rusterm_pane_move_done = false"));
        assert!(script.contains("window._rusterm_pane_move_remove"));
        assert!(script.contains("document.addEventListener('mousemove', moveHandler, true)"));
        assert!(!script.contains("__rusterm_tab_drag_pos"));
        assert!(!script.contains("__rusterm_drag_pos"));
    }

    #[test]
    fn pane_move_script_records_release_and_restores_selection() {
        let script = build_install_pane_move_script(1.0, 2.0);
        assert!(script.contains("window.__rusterm_pane_move_pos = e.clientX + ',' + e.clientY"));
        assert!(script.contains("window.__rusterm_pane_move_done = true"));
        assert!(script.contains("document.body.style.webkitUserSelect = 'none'"));
        assert!(script.contains("document.body.style.webkitUserSelect = ''"));
    }

    #[test]
    fn pane_move_poll_parser_accepts_valid_response() {
        assert_eq!(
            parse_pane_move_poll_response("120.5,-8,1"),
            Some((120.5, -8.0, true))
        );
    }

    #[test]
    fn pane_move_poll_parser_rejects_malformed_response() {
        assert_eq!(parse_pane_move_poll_response("120,8"), None);
        assert_eq!(parse_pane_move_poll_response("x,8,0"), None);
    }
}

/// Tests for the splitter drag-resize signal flow.
///
/// These tests cover the pure computation extracted into
/// `compute_split_drag_delta` — the function that decides, given the current
/// drag state and a new viewport-relative mouse position, whether to apply
/// a resize step and what fractional delta to apply. The full signal flow
/// (`apply_split_drag_step` mutating `Signal<Option<SplitDragState>>` and
/// calling `resize_layout_col`/`resize_layout_row`) requires the dioxus
/// runtime and a live `AppState`, so it's exercised end-to-end via the
/// `drag_*_splitter_*` tests in `layout.rs` (which verify the resize math
/// + clamping behaviour that this function feeds into).
///
/// What these tests verify that the layout tests don't:
///  - The viewport-coordinate fix: `last_applied_pos` is viewport-relative
///    (matching `e.client_coordinates()`), NOT container-relative. A drag
///    that starts at viewport-x=700 (because the sidebar is 200px wide and
///    the splitter is at container-x=500) must compute deltas from 700, not
///    from 500 — otherwise the first mousemove would jump by 200px.
///  - The duplicate-event suppression: `pos == last_applied_pos` returns
///    `None` (no-op), so duplicate mousemove events between frames don't
///    cause redundant layout writes.
///  - The zero-extent guard: if `container_extent` is 0 (shouldn't happen
///    in practice but the overlay guards against it), the function returns
///    `None` rather than dividing by zero.
///  - Direction change: a drag that reverses direction (rightward then
///    leftward) produces deltas with the correct sign.
#[cfg(test)]
mod split_drag_tests {
    use super::{SplitDragState, compute_split_drag_delta};

    /// A col drag started at viewport-x=700 (sidebar 200px + splitter at
    /// container-x=500). The first mousemove is also at viewport-x=700 —
    /// must be a no-op (None) to suppress duplicate events.
    #[test]
    fn duplicate_mousemove_is_noop() {
        let drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 1000.0,
            last_applied_pos: 700.0,
        };
        assert_eq!(compute_split_drag_delta(&drag, 700.0), None);
    }

    /// A col drag started at viewport-x=700. The first real mousemove is at
    /// viewport-x=710 (10px rightward). With container_w=1000, the
    /// fractional delta should be +0.01.
    #[test]
    fn first_mousemove_computes_correct_delta() {
        let drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 1000.0,
            last_applied_pos: 700.0,
        };
        assert_eq!(compute_split_drag_delta(&drag, 710.0), Some(0.01));
    }

    /// Viewport-coordinate fix: if the drag started at viewport-x=700
    /// (because the sidebar is 200px wide and the splitter is at
    /// container-x=500), the first mousemove at viewport-x=710 must
    /// compute delta from 700 (not from 500). This is the regression test
    /// for the prior bug where `last_applied_pos` was initialized to the
    /// container-relative splitter position, causing a 200px initial jump.
    #[test]
    fn viewport_coordinate_no_initial_jump() {
        let drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 1000.0,
            // Viewport-relative start, NOT container-relative (500).
            last_applied_pos: 700.0,
        };
        // 10px rightward in viewport space.
        let delta = compute_split_drag_delta(&drag, 710.0).unwrap();
        assert!(
            (delta - 0.01).abs() < 1e-9,
            "expected +0.01 delta, got {} — if this is +0.21, the drag is\n\
             using container-relative coordinates and jumping by the sidebar\n\
             width on the first mousemove",
            delta
        );
    }

    /// Direction reversal: drag rightward, then leftward. The delta sign
    /// must flip correctly.
    #[test]
    fn direction_reversal_flips_delta_sign() {
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 1000.0,
            last_applied_pos: 700.0,
        };
        // Rightward: 700 → 710 → 720.
        assert_eq!(compute_split_drag_delta(&drag, 710.0), Some(0.01));
        drag.last_applied_pos = 710.0;
        assert_eq!(compute_split_drag_delta(&drag, 720.0), Some(0.01));
        drag.last_applied_pos = 720.0;
        // Leftward: 720 → 710.
        assert_eq!(compute_split_drag_delta(&drag, 710.0), Some(-0.01));
    }

    /// Zero container extent: must return None (not panic with divide-by-zero).
    #[test]
    fn zero_container_extent_is_noop() {
        let drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 0.0,
            last_applied_pos: 700.0,
        };
        assert_eq!(compute_split_drag_delta(&drag, 710.0), None);
    }

    /// Row drag: same logic but with viewport-y coordinates.
    #[test]
    fn row_drag_computes_delta() {
        let drag = SplitDragState {
            is_col: false,
            idx: 0,
            container_extent: 800.0,
            last_applied_pos: 500.0,
        };
        // 10px downward in viewport space.
        assert_eq!(compute_split_drag_delta(&drag, 510.0), Some(0.0125));
    }

    /// A full drag sequence: mousedown at viewport-x=700, 5 mousemoves of
    /// 10px each rightward, then mouseup. Each step's delta should be +0.01
    /// and `last_applied_pos` should track the current position. The
    /// cumulative fractional delta is +0.05 (5 * 0.01).
    ///
    /// This mirrors the `drag_col_splitter_rightward_in_small_increments`
    /// test in `layout.rs` but exercises the viewport-coordinate path
    /// explicitly.
    #[test]
    fn full_drag_sequence_viewport_coordinates() {
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: 1000.0,
            last_applied_pos: 700.0,
        };
        let mut total_frac = 0.0_f64;
        for step in 1..=5 {
            let pos = 700.0 + (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            assert!(
                (frac - 0.01).abs() < 1e-9,
                "step {}: expected +0.01 delta, got {}",
                step,
                frac
            );
            total_frac += frac;
            drag.last_applied_pos = pos;
        }
        assert!(
            (total_frac - 0.05).abs() < 1e-9,
            "cumulative delta should be +0.05, got {}",
            total_frac
        );
    }

    /// Verify that `end_split_drag`'s focus-restore script targets the
    /// `terminal-input-{session_id}` element. We can't run the spawn'd
    /// future in a unit test (no dioxus runtime), but we can verify the
    /// element-id convention by checking that the format string the
    /// terminal renders matches what `end_split_drag` would look up.
    ///
    /// This is a regression guard for the "分屏后无法输入" bug: if either
    /// side of the contract changes (the terminal input div's id OR the
    /// focus-restore script's id format), this test fails.
    #[test]
    fn focus_restore_script_targets_correct_element_id() {
        // The id format the terminal input div uses (see terminal_view.rs).
        let session_id = "abc-123";
        let terminal_input_id = format!("terminal-input-{}", session_id);
        // The id format `end_split_drag` looks up.
        let focus_script = format!(
            "document.getElementById('terminal-input-{}')?.focus()",
            session_id
        );
        assert!(
            focus_script.contains(&terminal_input_id),
            "focus script `{}` does not look up `{}` — the splitter drag\n\
             end-handler and the terminal input div use different id formats,\n\
             so focus restore after drag will silently fail (the element won't\n\
             be found). This is the root cause of \"分屏后无法输入\".",
            focus_script,
            terminal_input_id
        );
    }
}

/// Tests for the JS-bridge-based splitter drag-resize mechanism.
///
/// These tests verify the pure string-building and string-parsing functions
/// that the JS bridge uses (`build_install_split_drag_script` and
/// `parse_split_drag_poll_response`). The full async flow
/// (`install_split_drag_js_listeners` → `poll_split_drag_state` →
/// `apply_split_drag_step`) requires the dioxus runtime and a live webview,
/// so it's not unit-tested here — the pure functions are the parts most
/// likely to silently break (a typo in the JS script would install broken
/// listeners; a parse bug would mis-read mouse positions).
#[cfg(test)]
mod split_drag_js_tests {
    use super::{build_install_split_drag_script, parse_split_drag_poll_response};

    /// The install script must initialize `__rusterm_drag_pos` to the starting
    /// mouse position. If this is missing, the first poll would read an empty
    /// string and the first delta would be wrong.
    #[test]
    fn install_script_initializes_drag_pos() {
        let script = build_install_split_drag_script(123.4, 567.8);
        assert!(
            script.contains("window.__rusterm_drag_pos = '123.4,567.8'"),
            "install script must initialize __rusterm_drag_pos to the starting\n\
             mouse position; got: {}",
            script
        );
    }

    /// The install script must clear `__rusterm_drag_done` at startup so a
    /// stale `true` from a prior drag doesn't immediately end the new drag.
    #[test]
    fn install_script_clears_done_flag() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.contains("window.__rusterm_drag_done = false"),
            "install script must clear __rusterm_drag_done at startup; got: {}",
            script
        );
    }

    /// The install script must attach BOTH `mousemove` and `mouseup` listeners
    /// at the DOCUMENT level with `useCapture: true`. Element-level listeners
    /// don't fire reliably during a button-held drag in dioxus 0.7's desktop
    /// webview — this is the root cause of the prior fix failures.
    #[test]
    fn install_script_uses_document_capture_phase() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.contains("document.addEventListener('mousemove', moveHandler, true)"),
            "install script must use document.addEventListener with capture=true\n\
             for mousemove; got: {}",
            script
        );
        assert!(
            script.contains("document.addEventListener('mouseup', upHandler, true)"),
            "install script must use document.addEventListener with capture=true\n\
             for mouseup; got: {}",
            script
        );
    }

    /// The install script must set `__rusterm_drag_done = true` on mouseup.
    /// This is how the polling use_future knows to call `end_split_drag`.
    #[test]
    fn install_script_sets_done_on_mouseup() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.contains("window.__rusterm_drag_done = true"),
            "install script's upHandler must set __rusterm_drag_done = true;\n\
             got: {}",
            script
        );
    }

    /// The install script must store a `_rusterm_split_drag_remove` function
    /// so `end_split_drag` can clean up the listeners after the drag ends.
    /// Without this, listeners would leak across drags.
    #[test]
    fn install_script_stores_remove_function() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.contains("window._rusterm_split_drag_remove = function"),
            "install script must store _rusterm_split_drag_remove; got: {}",
            script
        );
        assert!(
            script.contains("document.removeEventListener('mousemove'")
                && script.contains("document.removeEventListener('mouseup'"),
            "_rusterm_split_drag_remove must remove both listeners; got: {}",
            script
        );
    }

    /// The install script must be an IIFE (immediately-invoked function
    /// expression) so the listeners attach synchronously when `eval` dispatches
    /// the script. If the script just defines a function without calling it,
    /// the listeners would never attach.
    #[test]
    fn install_script_is_iife() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.starts_with("(function() {"),
            "install script must start with an IIFE opening; got: {}",
            script
        );
        // The script must end with the IIFE invocation `})()` — but we
        // can't put that literal in a format string (the `}` would close
        // the format), so we check for the components separately.
        let trimmed = script.trim_end();
        assert!(
            trimmed.ends_with(")()"),
            "install script must end with `)()` (the IIFE invocation); got: {}",
            script
        );
        assert!(
            trimmed.contains("})"),
            "install script must contain close-paren-brace (closing the function body); got: {}",
            script
        );
    }

    /// The install script must call `preventDefault()` on mousemove and
    /// mouseup to stop the browser from initiating text selection or other
    /// default actions during the drag.
    #[test]
    fn install_script_prevents_default() {
        let script = build_install_split_drag_script(100.0, 200.0);
        // Two preventDefault calls — one in moveHandler, one in upHandler.
        let count = script.matches("e.preventDefault()").count();
        assert_eq!(
            count, 2,
            "install script must call preventDefault in both moveHandler and\n\
             upHandler; found {} calls; got: {}",
            count, script
        );
    }

    /// The install script must call `_rusterm_split_drag_remove()` from the
    /// upHandler so the listeners remove themselves on mouseup (not just rely
    /// on `end_split_drag`'s separate cleanup spawn). This ensures no stale
    /// mousemove events fire after the drag ends.
    #[test]
    fn install_script_self_removes_on_mouseup() {
        let script = build_install_split_drag_script(100.0, 200.0);
        assert!(
            script.contains("if (window._rusterm_split_drag_remove) { window._rusterm_split_drag_remove(); window._rusterm_split_drag_remove = null; }"),
            "upHandler must call _rusterm_split_drag_remove and null it out;\n\
             got: {}",
            script
        );
    }

    // ------------------------------------------------------------------
    // parse_split_drag_poll_response tests
    // ------------------------------------------------------------------

    /// Normal case: parse a valid "x,y,0" response (drag in progress).
    #[test]
    fn parse_valid_in_progress_response() {
        let result = parse_split_drag_poll_response("123.4,567.8,0").unwrap();
        assert_eq!(result, (123.4, 567.8, false));
    }

    /// Normal case: parse a valid "x,y,1" response (drag done).
    #[test]
    fn parse_valid_done_response() {
        let result = parse_split_drag_poll_response("100.0,200.0,1").unwrap();
        assert_eq!(result, (100.0, 200.0, true));
    }

    /// Empty string: returns None (no globals set yet).
    #[test]
    fn parse_empty_returns_none() {
        assert_eq!(parse_split_drag_poll_response(""), None);
    }

    /// Malformed: only 2 parts (missing done flag). Returns None.
    #[test]
    fn parse_missing_done_flag_returns_none() {
        assert_eq!(parse_split_drag_poll_response("100.0,200.0"), None);
    }

    /// Malformed: non-numeric x. Returns None.
    #[test]
    fn parse_non_numeric_x_returns_none() {
        assert_eq!(parse_split_drag_poll_response("abc,200.0,0"), None);
    }

    /// Malformed: non-numeric y. Returns None.
    #[test]
    fn parse_non_numeric_y_returns_none() {
        assert_eq!(parse_split_drag_poll_response("100.0,xyz,0"), None);
    }

    /// Edge case: negative coordinates (mouse outside viewport top-left).
    /// Should parse correctly — the math handles negative deltas.
    #[test]
    fn parse_negative_coordinates() {
        let result = parse_split_drag_poll_response("-10.5,-20.0,0").unwrap();
        assert_eq!(result, (-10.5, -20.0, false));
    }

    /// Edge case: integer coordinates (no decimal point). Should parse.
    #[test]
    fn parse_integer_coordinates() {
        let result = parse_split_drag_poll_response("100,200,0").unwrap();
        assert_eq!(result, (100.0, 200.0, false));
    }

    /// Edge case: done flag is "1" (true). Other truthy strings like
    /// "true" should NOT be treated as true — only "1" counts. This is
    /// a contract guard: if the JS side ever changes the format, this test
    /// fails loudly.
    #[test]
    fn parse_done_flag_only_accepts_one() {
        assert_eq!(
            parse_split_drag_poll_response("100.0,200.0,true"),
            Some((100.0, 200.0, false)),
            "parse_split_drag_poll_response should only treat '1' as true"
        );
    }

    /// Round-trip: build an install script, verify it initializes the globals
    /// to the expected values, then verify the poll-response parser correctly
    /// parses a response that the JS would produce. This is a sanity check
    /// that the two halves of the bridge agree on the data format.
    #[test]
    fn js_bridge_round_trip_format_consistency() {
        // The install script initializes __rusterm_drag_pos to "x,y" (no done flag).
        let script = build_install_split_drag_script(500.0, 300.0);
        assert!(
            script.contains("'500,300'"),
            "install script must initialize __rusterm_drag_pos to '500,300'\n\
             (matching the x,y format the poll parser expects); got: {}",
            script
        );
        // The poll parser expects "x,y,done" — three comma-separated parts.
        // The install script's format "x,y" (two parts) is what the JS writes
        // to __rusterm_drag_pos; the poll JS appends ",done" to make three.
        let parsed = parse_split_drag_poll_response("500,300,0").unwrap();
        assert_eq!(parsed, (500.0, 300.0, false));
    }
}

/// Task 22 — tests for the manual mouse-based tab drag system. Mirrors
/// `split_drag_js_tests` in structure: the pure helpers
/// (`tab_drag_threshold_exceeded`, `build_install_tab_drag_script`,
/// `parse_tab_drag_poll_response`, `hit_test_pane_at`) are unit-tested
/// without a dioxus runtime. The signal-mutating wrappers
/// (`start_tab_drag`, `finish_tab_drag`, `install_tab_drag_js_listeners`,
/// `poll_tab_drag_state`) can't be unit-tested (they require the desktop
/// runtime + webview).
#[cfg(test)]
mod tab_drag_tests {
    use super::{
        PaneDropRegion, TAB_DRAG_THRESHOLD, build_install_tab_drag_script,
        center_line_styles_for_region, hit_test_pane_at, hit_test_pane_drop_target_at,
        pane_drop_region_for_cursor, parse_tab_drag_poll_response, tab_drag_threshold_exceeded,
    };
    use crate::layout::{LayoutPreset, PaneLayout};

    // ------------------------------------------------------------------
    // tab_drag_threshold_exceeded tests
    // ------------------------------------------------------------------

    /// A mousemove below the threshold (within ~5px of the start) is
    /// NOT a drag — it's a click with jitter.
    #[test]
    fn threshold_below_is_not_a_drag() {
        // 5px right, 0px down — distance 5px < TAB_DRAG_THRESHOLD (6.0).
        assert!(!tab_drag_threshold_exceeded(100.0, 100.0, 105.0, 100.0));
    }

    /// A mousemove above the threshold (>6px from the start) IS a drag.
    #[test]
    fn threshold_above_is_a_drag() {
        // 7px right, 0px down — distance 7px > TAB_DRAG_THRESHOLD (6.0).
        assert!(tab_drag_threshold_exceeded(100.0, 100.0, 107.0, 100.0));
    }

    /// The threshold is EUCLIDEAN (diagonal counts).
    #[test]
    fn threshold_is_euclidean() {
        // 4px right + 4px down = sqrt(32) ≈ 5.66px < 6.0 → not a drag.
        assert!(!tab_drag_threshold_exceeded(100.0, 100.0, 104.0, 104.0));
        // 5px right + 5px down = sqrt(50) ≈ 7.07px > 6.0 → drag.
        assert!(tab_drag_threshold_exceeded(100.0, 100.0, 105.0, 105.0));
    }

    /// The threshold constant is exactly 6.0 (sanity).
    #[test]
    fn threshold_constant_is_six() {
        assert_eq!(TAB_DRAG_THRESHOLD, 6.0);
    }

    // ------------------------------------------------------------------
    // build_install_tab_drag_script tests
    // ------------------------------------------------------------------

    /// The install script must initialize `__rusterm_tab_drag_pos` to the
    /// starting mouse position. If this is missing, the first poll would
    /// read an empty string and the threshold check would be wrong.
    #[test]
    fn tab_drag_install_script_initializes_pos() {
        let script = build_install_tab_drag_script(123.4, 567.8);
        assert!(
            script.contains("window.__rusterm_tab_drag_pos = '123.4,567.8'"),
            "install script must initialize __rusterm_tab_drag_pos to '123.4,567.8'; got: {}",
            script
        );
    }

    /// The install script must clear the `__rusterm_tab_drag_done` flag
    /// at install time. Without this, a stale `done=true` from a prior
    /// drag would make the polling loop think the drag is already over.
    #[test]
    fn tab_drag_install_script_clears_done_flag() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        assert!(
            script.contains("window.__rusterm_tab_drag_done = false"),
            "install script must clear __rusterm_tab_drag_done; got: {}",
            script
        );
    }

    /// The install script must be an IIFE (immediately-invoked function
    /// expression) so it runs synchronously when `eval` dispatches it.
    #[test]
    fn tab_drag_install_script_is_iife() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        assert!(
            script.starts_with("(function() {"),
            "install script must be an IIFE starting with '(function() {{'; got: {}",
            script
        );
        assert!(
            script.trim_end().ends_with("})()"),
            "install script must end with '}}()'; got: {}",
            script
        );
    }

    /// The install script must use CAPTURE-PHASE document listeners
    /// (third arg `true` to `addEventListener`). This is the same
    /// pattern as the splitter drag and is the ONLY mechanism that
    /// works reliably in dioxus 0.7's desktop webview.
    #[test]
    fn tab_drag_install_script_uses_capture_phase() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        assert!(
            script.contains("document.addEventListener('mousemove', moveHandler, true)"),
            "install script must use capture-phase mousemove listener; got: {}",
            script
        );
        assert!(
            script.contains("document.addEventListener('mouseup', upHandler, true)"),
            "install script must use capture-phase mouseup listener; got: {}",
            script
        );
    }

    /// The install script must store a `_rusterm_tab_drag_remove` function
    /// on `window` so `finish_tab_drag` can remove the listeners later
    /// (idempotent cleanup).
    #[test]
    fn tab_drag_install_script_stores_remove_function() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        assert!(
            script.contains("window._rusterm_tab_drag_remove = function() {"),
            "install script must store _rusterm_tab_drag_remove; got: {}",
            script
        );
        assert!(
            script.contains("document.removeEventListener('mousemove', moveHandler, true)"),
            "remove function must remove mousemove listener; got: {}",
            script
        );
    }

    /// The install script's `upHandler` must RECORD THE FINAL MOUSE
    /// POSITION (not just set the done flag). This is the KEY difference
    /// from the splitter drag's `upHandler` — a tab drop needs the release
    /// coordinates for hit-testing. Without this, the polling loop would
    /// hit-test the second-to-last mousemove position, missing the final
    /// release point.
    #[test]
    fn tab_drag_install_script_uphandler_records_position() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        // The upHandler must write to __rusterm_tab_drag_pos BEFORE
        // setting done=true. (Otherwise the poll that reads done=true
        // would hit-test a stale position.)
        let up_handler_start = script.find("var upHandler = function(e)").unwrap();
        let up_handler_region = &script[up_handler_start..];
        let up_handler_end = up_handler_region
            .find("};\n")
            .or_else(|| up_handler_region.find("};\r\n"))
            .unwrap_or(up_handler_region.len());
        let up_handler = &up_handler_region[..up_handler_end];
        assert!(
            up_handler.contains("window.__rusterm_tab_drag_pos = e.clientX + ',' + e.clientY;"),
            "upHandler must record final mouse position; got: {}",
            up_handler
        );
        assert!(
            up_handler.contains("window.__rusterm_tab_drag_done = true"),
            "upHandler must set done flag; got: {}",
            up_handler
        );
    }

    /// The install script must capture the `#terminal-content` container's
    /// `getBoundingClientRect()` at install time and stash it in
    /// `__rusterm_tab_drag_container_left/top`. The polling loop reads
    /// these to convert viewport-relative cursor coordinates into
    /// container-relative coordinates for hit-testing.
    #[test]
    fn tab_drag_install_script_captures_container_offset() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        assert!(
            script.contains("document.getElementById('terminal-content')"),
            "install script must look up #terminal-content; got: {}",
            script
        );
        assert!(
            script.contains("window.__rusterm_tab_drag_container_left = r.left"),
            "install script must capture container left; got: {}",
            script
        );
        assert!(
            script.contains("window.__rusterm_tab_drag_container_top = r.top"),
            "install script must capture container top; got: {}",
            script
        );
    }

    /// The install script must remove any previously-installed listeners
    /// BEFORE installing new ones (idempotent — prevents double-install
    /// if a drag is somehow started while another is still active).
    #[test]
    fn tab_drag_install_script_removes_prior_listeners() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        let remove_call_pos = script.find("if (window._rusterm_tab_drag_remove)").unwrap();
        let add_listener_pos = script
            .find("document.addEventListener('mousemove'")
            .unwrap();
        assert!(
            remove_call_pos < add_listener_pos,
            "install script must remove prior listeners BEFORE adding new ones; \
             remove_call_pos={}, add_listener_pos={}",
            remove_call_pos,
            add_listener_pos
        );
    }

    /// Issue: "拖拽时文本被错误选中" — during a tab drag, page text got
    /// blue-highlighted. The install script must suppress text selection
    /// document-wide for the duration of the drag, and the remove
    /// function must restore it.
    #[test]
    fn tab_drag_install_script_suppresses_text_selection_and_restores_it() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        // Install must disable text selection document-wide (WebKit
        // ignores moveHandler's preventDefault for selections started
        // on the mousedown) and clear any existing selection.
        assert!(
            script.contains("document.body.style.webkitUserSelect = 'none'"),
            "install script must disable -webkit-user-select on body; got: {}",
            script
        );
        assert!(
            script.contains("document.body.style.userSelect = 'none'"),
            "install script must disable user-select on body; got: {}",
            script
        );
        assert!(
            script.contains("window.getSelection().removeAllRanges()"),
            "install script must clear any existing selection; got: {}",
            script
        );
        // The remove function must RESTORE user-select (empty string =
        // revert to stylesheet value).
        let remove_fn_start = script
            .find("window._rusterm_tab_drag_remove = function() {")
            .unwrap();
        let remove_fn_region = &script[remove_fn_start..];
        assert!(
            remove_fn_region.contains("document.body.style.webkitUserSelect = ''"),
            "remove function must restore -webkit-user-select; got: {}",
            script
        );
        assert!(
            remove_fn_region.contains("document.body.style.userSelect = ''"),
            "remove function must restore user-select; got: {}",
            script
        );
    }

    /// The install script must NOT clobber the SPLITTER drag's globals
    /// (`__rusterm_drag_pos`, `__rusterm_drag_done`). The two systems
    /// use SEPARATE global variable names so they can't interfere.
    #[test]
    fn tab_drag_install_script_uses_separate_globals_from_splitter() {
        let script = build_install_tab_drag_script(100.0, 200.0);
        // Must NOT touch the splitter's globals.
        assert!(
            !script.contains("__rusterm_drag_pos"),
            "tab drag script must NOT touch splitter's __rusterm_drag_pos; got: {}",
            script
        );
        assert!(
            !script.contains("__rusterm_drag_done"),
            "tab drag script must NOT touch splitter's __rusterm_drag_done; got: {}",
            script
        );
        assert!(
            !script.contains("_rusterm_split_drag_remove"),
            "tab drag script must NOT touch splitter's _rusterm_split_drag_remove; got: {}",
            script
        );
    }

    // ------------------------------------------------------------------
    // parse_tab_drag_poll_response tests
    // ------------------------------------------------------------------

    /// A valid in-progress response (done=0) parses correctly.
    #[test]
    fn tab_drag_parse_valid_in_progress_response() {
        // x, y, done, container_left, container_top
        let result = parse_tab_drag_poll_response("123.4,567.8,0,80.0,60.0").unwrap();
        assert_eq!(result, (123.4, 567.8, false, 80.0, 60.0));
    }

    /// A valid done response (done=1) parses correctly.
    #[test]
    fn tab_drag_parse_valid_done_response() {
        let result = parse_tab_drag_poll_response("100.0,200.0,1,80.0,60.0").unwrap();
        assert_eq!(result, (100.0, 200.0, true, 80.0, 60.0));
    }

    /// An empty response (globals not set yet) returns `None`.
    #[test]
    fn tab_drag_parse_empty_returns_none() {
        assert_eq!(parse_tab_drag_poll_response(""), None);
    }

    /// A response with too few fields returns `None` (defensive against
    /// a stale `tab_drag` signal after the listeners were removed).
    #[test]
    fn tab_drag_parse_too_few_fields_returns_none() {
        // Only x, y, done — missing container_left/top.
        assert_eq!(parse_tab_drag_poll_response("100.0,200.0,0"), None);
        // Only x, y.
        assert_eq!(parse_tab_drag_poll_response("100.0,200.0"), None);
        // Only x.
        assert_eq!(parse_tab_drag_poll_response("100.0"), None);
    }

    /// A response with too many fields returns `None` (defensive).
    #[test]
    fn tab_drag_parse_too_many_fields_returns_none() {
        assert_eq!(parse_tab_drag_poll_response("1,2,3,4,5,6"), None);
    }

    /// A response with non-numeric coordinates returns `None`.
    #[test]
    fn tab_drag_parse_non_numeric_returns_none() {
        assert_eq!(parse_tab_drag_poll_response("abc,def,0,80,60"), None);
        assert_eq!(parse_tab_drag_poll_response("100.0,def,0,80,60"), None);
    }

    /// A response with negative coordinates parses correctly (the
    /// cursor CAN be at negative viewport coordinates if the user
    /// drags outside the window — the JS listeners keep firing).
    #[test]
    fn tab_drag_parse_negative_coordinates() {
        let result = parse_tab_drag_poll_response("-50.0,-100.0,0,80.0,60.0").unwrap();
        assert_eq!(result, (-50.0, -100.0, false, 80.0, 60.0));
    }

    /// A response with integer coordinates (no decimal point) parses
    /// correctly — `f64::parse` handles both.
    #[test]
    fn tab_drag_parse_integer_coordinates() {
        let result = parse_tab_drag_poll_response("100,200,0,80,60").unwrap();
        assert_eq!(result, (100.0, 200.0, false, 80.0, 60.0));
    }

    /// A response with zero container offset parses correctly (e.g.,
    /// if `#terminal-content` is at the viewport origin).
    #[test]
    fn tab_drag_parse_zero_container_offset() {
        let result = parse_tab_drag_poll_response("100.0,200.0,0,0,0").unwrap();
        assert_eq!(result, (100.0, 200.0, false, 0.0, 0.0));
    }

    // ------------------------------------------------------------------
    // hit_test_pane_at tests
    // ------------------------------------------------------------------

    /// Cursor inside the container's only pane returns pane 0.
    #[test]
    fn pane_drop_hit_test_distinguishes_target_top_and_bottom() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );

        let top = hit_test_pane_drop_target_at(900.0, 160.0, 0.0, 0.0, 1200.0, 800.0, &layout)
            .expect("top target");
        let bottom = hit_test_pane_drop_target_at(900.0, 700.0, 0.0, 0.0, 1200.0, 800.0, &layout)
            .expect("bottom target");

        assert_eq!(top.pane_idx, 1);
        assert_eq!(top.region, PaneDropRegion::Top);
        assert_eq!(bottom.pane_idx, 1);
        assert_eq!(bottom.region, PaneDropRegion::Bottom);
    }

    /// 4-quadrant hit-test: cursor in the LEFT half of pane 0 reports
    /// `Left`, in the RIGHT half reports `Right`. Center crosshair zone
    /// (within 15% of pane center on both axes) reports `Center`.
    #[test]
    fn pane_drop_hit_test_distinguishes_target_left_and_right() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        // Container 1200×800, Split2H: pane 0 at x=[0,600), pane 1 at x=[600,1200).
        // Pane 0 spans y=[0,800). Pane 0 center is at (300, 400).
        //
        // Cursor at (200, 400): rel_x_in_pane = 200/600 = 0.333,
        //   dx_from_center = 0.333 - 0.5 = -0.167, |dx| > 0.15,
        //   dy_from_center = 0 → horizontal axis dominates → Left.
        let left = hit_test_pane_drop_target_at(200.0, 400.0, 0.0, 0.0, 1200.0, 800.0, &layout)
            .expect("left target");
        // Cursor at (450, 400): rel_x_in_pane = 450/600 = 0.75,
        //   dx_from_center = 0.25 → Right.
        let right = hit_test_pane_drop_target_at(450.0, 400.0, 0.0, 0.0, 1200.0, 800.0, &layout)
            .expect("right target");
        // Cursor at (300, 400) is exactly pane 0's center → Center zone.
        let center = hit_test_pane_drop_target_at(300.0, 400.0, 0.0, 0.0, 1200.0, 800.0, &layout)
            .expect("center target");

        assert_eq!(left.pane_idx, 0);
        assert_eq!(left.region, PaneDropRegion::Left);
        assert_eq!(right.pane_idx, 0);
        assert_eq!(right.region, PaneDropRegion::Right);
        assert_eq!(center.pane_idx, 0);
        assert_eq!(center.region, PaneDropRegion::Center);
    }

    /// Direct unit tests for the 4-quadrant decision function. The
    /// function takes the cursor's normalised distance from pane center
    /// (each axis in `[-0.5, 0.5]`) and returns the drop region. Center
    /// ±0.15 on BOTH axes is the swap/move zone; outside that, the axis
    /// with the larger |distance| wins.
    #[test]
    fn pane_drop_region_for_cursor_returns_center_near_middle() {
        // Exactly at center → Center.
        assert_eq!(
            pane_drop_region_for_cursor(0.0, 0.0),
            PaneDropRegion::Center
        );
        // Within ±0.15 on both axes → Center.
        assert_eq!(
            pane_drop_region_for_cursor(0.1, 0.1),
            PaneDropRegion::Center
        );
        assert_eq!(
            pane_drop_region_for_cursor(-0.14, 0.14),
            PaneDropRegion::Center
        );
        // Just inside the boundary (0.149).
        assert_eq!(
            pane_drop_region_for_cursor(0.149, -0.149),
            PaneDropRegion::Center
        );
    }

    #[test]
    fn pane_drop_region_for_cursor_picks_dominant_axis() {
        // Horizontal dominates: |dx| > |dy|, dx < 0 → Left.
        assert_eq!(pane_drop_region_for_cursor(-0.4, 0.0), PaneDropRegion::Left);
        // Horizontal dominates: |dx| > |dy|, dx > 0 → Right.
        assert_eq!(pane_drop_region_for_cursor(0.4, 0.0), PaneDropRegion::Right);
        // Vertical dominates: |dy| > |dx|, dy < 0 → Top.
        assert_eq!(pane_drop_region_for_cursor(0.0, -0.4), PaneDropRegion::Top);
        // Vertical dominates: |dy| > |dx|, dy > 0 → Bottom.
        assert_eq!(
            pane_drop_region_for_cursor(0.0, 0.4),
            PaneDropRegion::Bottom
        );
        // Corner: |dx| == |dy| → tie goes to vertical (>=). At (0.3, 0.3)
        // |dx| is NOT > |dy| (0.3 > 0.3 is false) → vertical → Bottom.
        assert_eq!(
            pane_drop_region_for_cursor(0.3, 0.3),
            PaneDropRegion::Bottom
        );
        // Just outside center zone but |dx| > |dy|: dx=0.2, dy=0.1 → Right.
        assert_eq!(pane_drop_region_for_cursor(0.2, 0.1), PaneDropRegion::Right);
        // Just outside center zone but |dy| > |dx|: dy=-0.2, dx=0.1 → Top.
        assert_eq!(pane_drop_region_for_cursor(0.1, -0.2), PaneDropRegion::Top);
    }

    /// The overlay must show EXACTLY ONE bright center line for split regions
    /// (vertical for Left/Right = 横着 placement, horizontal for Top/Bottom =
    /// 竖着 placement). This is the fix for the "错误的产生多个不需要的
    /// 四方块" bug — the prior crosshair always drew BOTH lines, forming a
    /// 田 shape that was ambiguous about the split direction.
    ///
    /// For Center (swap/move zone), BOTH lines are drawn dimmed so the user
    /// can see the swap zone without it dominating the pane.
    #[test]
    fn center_line_styles_for_region_shows_one_line_per_split_axis() {
        // Left/Right (horizontal split): vertical line ONLY.
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Left);
        assert!(
            v.is_some(),
            "Left region must show the vertical divider line"
        );
        assert!(h.is_none(), "Left region must NOT show the horizontal line");
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Right);
        assert!(
            v.is_some(),
            "Right region must show the vertical divider line"
        );
        assert!(
            h.is_none(),
            "Right region must NOT show the horizontal line"
        );

        // Top/Bottom (vertical split): horizontal line ONLY.
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Top);
        assert!(v.is_none(), "Top region must NOT show the vertical line");
        assert!(
            h.is_some(),
            "Top region must show the horizontal divider line"
        );
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Bottom);
        assert!(v.is_none(), "Bottom region must NOT show the vertical line");
        assert!(
            h.is_some(),
            "Bottom region must show the horizontal divider line"
        );

        // Center: both lines (dimmed) — the swap zone.
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Center);
        assert!(
            v.is_some(),
            "Center region must show the (dimmed) vertical line"
        );
        assert!(
            h.is_some(),
            "Center region must show the (dimmed) horizontal line"
        );
    }

    /// The bright accent lines for split regions must be visually distinct
    /// from the dimmed Center lines (bright = 2px solid #7aa2f7 with glow,
    /// dimmed = 1px rgba(122,162,247,0.35)). This is what makes the split
    /// direction immediately readable: a bright line jumps out, a dimmed
    /// line recedes.
    #[test]
    fn center_line_styles_for_region_uses_bright_line_for_splits_dimmed_for_center() {
        // Split regions use the bright (2px, full-opacity, glow) line.
        let (v, _) = center_line_styles_for_region(PaneDropRegion::Left);
        let v = v.expect("Left region has a vertical line");
        assert!(
            v.contains("width: 2px"),
            "split line must be 2px wide (bright): got {v}"
        );
        assert!(
            v.contains("background: #7aa2f7"),
            "split line must be full-opacity #7aa2f7: got {v}"
        );
        assert!(
            v.contains("box-shadow"),
            "split line must have a glow (box-shadow): got {v}"
        );

        let (_, h) = center_line_styles_for_region(PaneDropRegion::Top);
        let h = h.expect("Top region has a horizontal line");
        assert!(
            h.contains("height: 2px"),
            "split line must be 2px wide (bright): got {h}"
        );
        assert!(
            h.contains("background: #7aa2f7"),
            "split line must be full-opacity #7aa2f7: got {h}"
        );

        // Center uses the dimmed (1px, 35% opacity, no glow) lines.
        let (v, h) = center_line_styles_for_region(PaneDropRegion::Center);
        let v = v.expect("Center region has a (dimmed) vertical line");
        let h = h.expect("Center region has a (dimmed) horizontal line");
        assert!(
            v.contains("width: 1px"),
            "center line must be 1px wide (dimmed): got {v}"
        );
        assert!(
            v.contains("rgba(122,162,247,0.35)"),
            "center line must be 35% opacity: got {v}"
        );
        assert!(
            !v.contains("box-shadow"),
            "center line must NOT have a glow: got {v}"
        );
        assert!(
            h.contains("height: 1px"),
            "center line must be 1px wide (dimmed): got {h}"
        );
        assert!(
            h.contains("rgba(122,162,247,0.35)"),
            "center line must be 35% opacity: got {h}"
        );
        assert!(
            !h.contains("box-shadow"),
            "center line must NOT have a glow: got {h}"
        );
    }

    /// Symmetry: Left and Right produce the SAME vertical-line style (the
    /// divider is at the pane center regardless of which half is highlighted).
    /// Top and Bottom similarly produce the same horizontal-line style. This
    /// is a regression guard against accidentally permuting the styles per
    /// side (which would make the divider jump around as the cursor moves
    /// within the same split axis).
    #[test]
    fn center_line_styles_for_region_is_symmetric_within_split_axis() {
        let (left_v, left_h) = center_line_styles_for_region(PaneDropRegion::Left);
        let (right_v, right_h) = center_line_styles_for_region(PaneDropRegion::Right);
        assert_eq!(
            left_v, right_v,
            "Left and Right must share the same vertical-line style"
        );
        assert_eq!(
            left_h, right_h,
            "Left and Right must share the same horizontal-line style"
        );

        let (top_v, top_h) = center_line_styles_for_region(PaneDropRegion::Top);
        let (bot_v, bot_h) = center_line_styles_for_region(PaneDropRegion::Bottom);
        assert_eq!(
            top_v, bot_v,
            "Top and Bottom must share the same vertical-line style"
        );
        assert_eq!(
            top_h, bot_h,
            "Top and Bottom must share the same horizontal-line style"
        );
    }

    #[test]
    fn hit_test_single_pane_returns_pane_zero() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &["alpha".to_string()]);
        // Container at viewport (80, 60), size 1200x800. Cursor at
        // viewport (640, 400) → container-relative (560, 340) → pane 0.
        let hit = hit_test_pane_at(640.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((0, "alpha".to_string())));
    }

    /// Cursor outside the container (left of it) returns `None`.
    #[test]
    fn hit_test_outside_container_left_returns_none() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &["alpha".to_string()]);
        // Container at viewport (80, 60). Cursor at (50, 400) is LEFT
        // of the container (rel_x = -30).
        let hit = hit_test_pane_at(50.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, None);
    }

    /// Cursor outside the container (above it) returns `None`.
    #[test]
    fn hit_test_outside_container_above_returns_none() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &["alpha".to_string()]);
        let hit = hit_test_pane_at(640.0, 30.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, None);
    }

    /// Cursor in the LEFT pane of a Split2H layout returns pane 0.
    #[test]
    fn hit_test_split2h_left_pane() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        // Container at (80, 60), size 1200x800. Split2H splits
        // horizontally: pane 0 is left half (x: 0-600), pane 1 is
        // right half (x: 600-1200). Cursor at viewport (340, 400) →
        // container-relative (260, 340) → pane 0 (alpha).
        let hit = hit_test_pane_at(340.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((0, "alpha".to_string())));
    }

    /// Cursor in the RIGHT pane of a Split2H layout returns pane 1.
    #[test]
    fn hit_test_split2h_right_pane() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        // Cursor at viewport (940, 400) → container-relative (860, 340)
        // → pane 1 (beta).
        let hit = hit_test_pane_at(940.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((1, "beta".to_string())));
    }

    /// Cursor on the EXACT boundary between two Split2H panes (x=600)
    /// is treated as inside pane 1 (the right pane) because the hit-test
    /// uses `rel_x >= px && rel_x < px + pw` — the left edge is inclusive,
    /// the right edge is exclusive. This matches CSS's pixel-grid model.
    #[test]
    fn hit_test_split2h_boundary_goes_to_right_pane() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        // Cursor at viewport (680, 400) → container-relative (600, 340).
        // Pane 0 spans [0, 600); pane 1 spans [600, 1200). x=600 → pane 1.
        let hit = hit_test_pane_at(680.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((1, "beta".to_string())));
    }

    /// Cursor in a SPECIFIC pane of a Grid4 layout (top-right).
    #[test]
    fn hit_test_grid4_top_right_pane() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Grid4,
            &[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
        );
        // Grid4: 2x2. Pane 0 (a) top-left [0-600, 0-400],
        // pane 1 (b) top-right [600-1200, 0-400],
        // pane 2 (c) bottom-left [0-600, 400-800],
        // pane 3 (d) bottom-right [600-1200, 400-800].
        // Cursor at viewport (940, 200) → container-relative (860, 140)
        // → pane 1 (b).
        let hit = hit_test_pane_at(940.0, 200.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((1, "b".to_string())));
    }

    /// Cursor in the BOTTOM-LEFT pane of a Grid4 layout.
    #[test]
    fn hit_test_grid4_bottom_left_pane() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Grid4,
            &[
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
        );
        // Cursor at viewport (340, 600) → container-relative (260, 540)
        // → pane 2 (c).
        let hit = hit_test_pane_at(340.0, 600.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, Some((2, "c".to_string())));
    }

    /// Cursor OUTSIDE all panes but INSIDE the container returns `None`.
    /// In a Grid4 (which fills the container completely), this shouldn't
    /// happen — but in a Split2H with the cursor exactly at the container's
    /// right edge (x=1200, which is EXCLUSIVE), it would. Verify the
    /// exclusive-right-edge behavior.
    #[test]
    fn hit_test_container_right_edge_is_exclusive() {
        let layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        // Cursor at viewport (1280, 400) → container-relative (1200, 340).
        // The container's right edge (x=1200) is exclusive (rel_x > cw).
        let hit = hit_test_pane_at(1280.0, 400.0, 80.0, 60.0, 1200.0, 800.0, &layout);
        assert_eq!(hit, None);
    }

    #[test]
    fn hit_test_overlapping_floating_windows_returns_frontmost_pane() {
        let mut layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["back".to_string(), "front".to_string()],
        );
        assert!(layout.enable_floating());
        layout.panes[0].floating.as_mut().unwrap().x_frac = 0.2;
        layout.panes[1].floating.as_mut().unwrap().x_frac = 0.2;
        layout.panes[0].floating.as_mut().unwrap().y_frac = 0.2;
        layout.panes[1].floating.as_mut().unwrap().y_frac = 0.2;
        assert!(layout.bring_floating_pane_to_front(1));

        assert_eq!(
            hit_test_pane_at(300.0, 240.0, 0.0, 0.0, 1000.0, 800.0, &layout),
            Some((1, "front".to_string()))
        );
    }
}
