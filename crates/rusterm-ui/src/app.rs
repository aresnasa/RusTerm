use std::collections::HashMap;
use std::sync::Arc;

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
use crate::components::Sidebar;
use crate::components::TabBar;
use crate::components::TerminalView;
use crate::components::connection_dialog::NewConnectionForm;
use crate::layout::PaneLayout;
use crate::state::{
    AppState, Modal, OneKeyMatch, OneKeyPopupState, PendingDangerousCommand, SessionTab,
    TerminalEntry, UnlockState, cycle_layout_preset, move_session_to_leftmost,
    pane_index_for_active_session, resize_layout_col, resize_layout_row,
    set_pane_session_for_active, swap_pane_sessions, toggle_comparison_mode, toggle_pane_zoom,
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

/// Human-readable label for the current layout preset, shown in the status
/// bar's layout toolbar. Kept short so it fits in the status bar's small
/// footprint. Used instead of `{:?}` because dioxus's rsx! formatter
/// doesn't support `#?` and the default `Debug` output is too long.
fn layout_label(preset: crate::layout::LayoutPreset) -> &'static str {
    use crate::layout::LayoutPreset::*;
    match preset {
        Single => "1",
        Split2H => "2H",
        Split2V => "2V",
        Grid4 => "4",
        Grid8 => "8",
    }
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
            // Whether this session's channel has dropped (Enter → reconnect).
            let tab_disconnected = state.read().disconnected_sessions.contains(&tab.id);
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
                        // input only goes to this pane's session — the
                        // legacy non-broadcast path.
                        let broadcast_targets = crate::state::broadcast_targets(&state_for_cmd.read());
                        let is_broadcast = broadcast_targets.len() > 1
                            || (broadcast_targets.len() == 1 && broadcast_targets[0] != sid_clone);
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
                            "[ONEKEY-SELECT] session={} send_len={} first_byte={:?}",
                            &sid_for_ok_sel[..sid_for_ok_sel.len().min(8)],
                            send.len(),
                            send.as_bytes().first().copied()
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

/// State held while the user is drag-resizing a splitter bar. Set by the
/// splitter's `onmousedown`; cleared by the document-level `mouseup` handler
/// installed in `App`'s drag-poll future. The poll future reads
/// `document._rusterm_split_drag_mouse` (kept current by a JS `mousemove`
/// listener) on each tick and applies the delta to the active layout via
/// `resize_layout_col` / `resize_layout_row`.
///
/// We use a polling approach (rather than per-event `eval` callbacks)
/// because dioxus's `eval` bridge is async request/response — it can't
/// deliver high-frequency mouse events synchronously. Polling at 16ms
/// (~60fps) is smooth enough for splitter dragging and matches the
/// existing `TerminalView` resize-poll pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SplitDragState {
    /// `true` for a column splitter (vertical bar, drag adjusts column
    /// widths), `false` for a row splitter (horizontal bar, drag adjusts
    /// row heights).
    is_col: bool,
    /// Index of the column/row being resized (the left/top of the pair).
    idx: usize,
    /// Pixel position of the splitter bar at drag start. The current mouse
    /// position is compared against this to compute the drag delta.
    start_pos: f64,
    /// Container extent (width for col drag, height for row drag) at drag
    /// start, used to convert the pixel delta into a fractional delta.
    container_extent: f64,
    /// Last applied mouse position — used to detect when a new mousemove has
    /// landed so we don't re-apply the same delta. JS writes the current
    /// position to `document._rusterm_split_drag_mouse`; we read it, compare
    /// to `last_applied_pos`, and skip if unchanged.
    last_applied_pos: f64,
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
fn multi_pane_container(
    mut state: Signal<AppState>,
    input_senders: Signal<std::collections::HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    layout: PaneLayout,
    mut drag_over_pane: Signal<Option<usize>>,
    container_size: Signal<Option<(f64, f64)>>,
    split_drag: Signal<Option<SplitDragState>>,
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

    // Pre-collect the (pane_idx, session_id, rect, drop_session_id,
    // border_style, pane_title) tuples as owned values. The move closures inside each
    // pane's ondragover / ondrop / ondragstart handlers need to capture owned copies of
    // the session_id — the rsx! `for` loop body can't contain `let`
    // statements, so we pre-compute the owned clones (and the
    // drag-over-derived border style) here and destructure them in the for
    // pattern.
    //
    // We use a 6-tuple: `idx` is `usize` (Copy), so the ondrop closure can
    // capture it directly without a redundant clone. `session_id` is
    // consumed by the `key:` interpolation and `render_terminal_pane`, so a
    // second owned copy (`drop_session_id`) is needed for the move closure.
    // `border_style` is a `&'static str` (Copy) computed by reading
    // `drag_over_pane()` once per pane during Vec construction — this read
    // subscribes `App` to the signal, so any change triggers ONE re-render
    // of `App` (which rebuilds this Vec with the new values).
    // `pane_title` is the session's display name, shown in the pane's
    // drag-handle title bar.
    //
    // Each pane in the layout is BOTH a drag source (via the title bar's
    // `draggable: true` + `ondragstart`) and a drop target: the user can
    // drag an open session from the tab bar OR from another pane's title
    // bar onto a pane (moves or swaps the session into that pane), or drag
    // a sidebar connection onto a pane (opens a new session in that pane).
    // The drag source identifies itself via a custom MIME type in the
    // DragEvent's DataTransfer:
    //   - "application/x-rusterm-session-id"     → drag from tab bar or pane title
    //   - "application/x-rusterm-connection-id"  → drag from sidebar
    let pane_items: Vec<(
        usize,
        String,
        (f64, f64, f64, f64),
        String,
        &'static str,
        String,
        String,
    )> = visible
        .into_iter()
        .map(|(idx, sid, rect)| {
            let border = if drag_over_pane() == Some(idx) {
                "border: 2px solid #7aa2f7; box-sizing: border-box;"
            } else {
                "border: 2px solid transparent; box-sizing: border-box;"
            };
            // Look up the session's display name for the pane title bar.
            // Falls back to the session id if the session was closed
            // between the layout snapshot and this render (the renderer
            // treats an empty session_id as "no pane here").
            let title = state
                .read()
                .sessions
                .iter()
                .find(|t| t.id == sid)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| sid.clone());
            // `drag_sid` is a second clone for the ondragstart closure
            // (the first clone `drop_session_id` is consumed by the
            // ondrop closure; the original `sid` is consumed by the
            // `key:` interpolation and `render_terminal_pane`). rsx!
            // can't hold `let` bindings in the for body, so we pre-clone.
            let drag_sid = sid.clone();
            (idx, sid.clone(), rect, sid, border, title, drag_sid)
        })
        .collect();

    rsx! {
        div {
            id: "multi-pane-container",
            style: "position: absolute; left: 0; right: 0; top: 0; bottom: 0; overflow: hidden;",

            // Comparison-mode banner.
            {comparison_on.then(|| rsx! {
                div {
                    style: "
                        position: absolute;
                        top: 0; left: 0; right: 0;
                        height: 20px;
                        background: #7aa2f7;
                        color: #1a1b26;
                        font-size: 11px;
                        font-weight: 600;
                        display: flex;
                        align-items: center;
                        justify-content: center;
                        z-index: 100;
                        pointer-events: none;
                    ",
                    "⚠ Comparison mode ON — input is broadcast to all panes"
                }
            })}

            // Render each pane in its computed rectangle.
            //
            // PERF: `border_style` is pre-computed in `pane_items` by reading
            // `drag_over_pane()` once per pane during Vec construction. This
            // read subscribes `App` to the signal, so any change triggers
            // ONE re-render of `App` (not per-tick). The Signal equality
            // check prevents re-renders for no-op `set(Some(idx))` calls in
            // the high-frequency `ondragover` (~60Hz) handler — the
            // highlight only changes when the dragged pane actually changes.
            // This aligns with the user's frequency-vs-feedback
            // preference: fewer re-renders over per-tick feedback.
            for (idx, session_id, (x, y, w, h), drop_session_id, border_style, pane_title, drag_sid) in pane_items.into_iter() {
                div {
                    key: "pane-{idx}-{session_id}",
                    style: format!(
                        "position: absolute; left: {x}px; top: {y}px; width: {w}px; height: {h}px; overflow: hidden; display: flex; flex-direction: column; {border}",
                        x = x, y = y, w = w, h = h, border = border_style
                    ),
                    // `ondragover` must call prevent_default to signal
                    // that this element accepts drops. Without it, the
                    // browser fires `ondrop` with an empty DataTransfer
                    // (security restriction: drops without a dragover
                    // prevent_default are blocked).
                    //
                    // PERF: we also set `drag_over_pane` here. The Signal
                    // equality check makes this a no-op when the value is
                    // already `Some(idx)`, so the high-frequency dragover
                    // (~60Hz) does NOT trigger per-tick re-renders.
                    ondragover: move |e: DragEvent| {
                        e.prevent_default();
                        e.data_transfer().set_drop_effect("move");
                        drag_over_pane.set(Some(idx));
                    },
                    // `ondragenter` also needs prevent_default for
                    // cross-browser compatibility (some browsers
                    // require both dragenter AND dragover to be
                    // cancelled to allow drop). We also set
                    // `drag_over_pane` here — this is the event that
                    // actually changes the highlight when the cursor
                    // enters a new pane.
                    ondragenter: move |e: DragEvent| {
                        e.prevent_default();
                        drag_over_pane.set(Some(idx));
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
                        if let Some(dragged_sid) = dt.get_data("application/x-rusterm-session-id") {
                            if dragged_sid.is_empty() {
                                tracing::warn!("[DROP] empty session-id in drag data");
                                return;
                            }
                            // If the user dropped the session onto its
                            // own pane, it's a no-op.
                            if dragged_sid == drop_session_id {
                                tracing::debug!(
                                    "[DROP] session {} dropped onto its own pane — no-op",
                                    &dragged_sid[..dragged_sid.len().min(8)]
                                );
                                return;
                            }
                            // If the target pane is empty, move the
                            // session there (and clear the source pane).
                            // Otherwise, swap the two panes' sessions.
                            if drop_session_id.is_empty() {
                                // Find the source pane index, move the
                                // session to the target pane, clear the
                                // source.
                                let src_pane = {
                                    let s = state.read();
                                    pane_index_for_active_session(&s, &dragged_sid)
                                };
                                if let Some(src_idx) = src_pane {
                                    let mut s = state.write();
                                    set_pane_session_for_active(
                                        &mut s,
                                        idx,
                                        dragged_sid.clone(),
                                    );
                                    // Only clear the source pane if it's
                                    // different from the target (which
                                    // it always is here, but be
                                    // defensive).
                                    if src_idx != idx {
                                        set_pane_session_for_active(&mut s, src_idx, String::new());
                                    }
                                    tracing::info!(
                                        "[DROP] moved session {} from pane {} to pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        src_idx,
                                        idx
                                    );
                                } else {
                                    // Source session isn't in any pane
                                    // — just assign it to the target.
                                    let mut s = state.write();
                                    set_pane_session_for_active(
                                        &mut s,
                                        idx,
                                        dragged_sid,
                                    );
                                }
                            } else {
                                // Target pane has a session — swap.
                                let swapped = swap_pane_sessions(
                                    &mut state.write(),
                                    &dragged_sid,
                                    &drop_session_id,
                                );
                                if swapped {
                                    tracing::info!(
                                        "[DROP] swapped session {} with pane {}'s session {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx,
                                        &drop_session_id[..drop_session_id.len().min(8)]
                                    );
                                } else {
                                    tracing::warn!(
                                        "[DROP] swap failed for session {} → pane {}",
                                        &dragged_sid[..dragged_sid.len().min(8)],
                                        idx
                                    );
                                }
                            }
                            return;
                        }
                        // Check for the "drag from sidebar" MIME type —
                        // a connection is being opened in this pane.
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
                            // Open the connection in this pane. The
                            // `open_connection` helper handles creating
                            // the terminal, pushing the session tab, and
                            // assigning the new session_id to
                            // pane `idx` via
                            // `set_pane_session_for_active`.
                            open_connection(
                                state,
                                input_senders,
                                conn,
                                Some(idx),
                            );
                            return;
                        }
                        // Unknown MIME type — log and ignore.
                        tracing::debug!(
                            "[DROP] pane {} received drop with no recognized MIME type",
                            idx
                        );
                    },
                    // Title bar / drag handle. The bar is the only
                    // draggable part of the pane — the terminal content
                    // below must stay non-draggable so text selection
                    // and mouse clicks still work. `draggable: true`
                    // on this bar is what initiates the pane-drag.
                    div {
                        style: "
                            height: 18px;
                            background: #1f2335;
                            border-bottom: 1px solid #2a2b3d;
                            display: flex;
                            align-items: center;
                            padding: 0 8px;
                            font-size: 11px;
                            color: #c0caf5;
                            cursor: grab;
                            user-select: none;
                            flex-shrink: 0;
                            z-index: 10;
                        ",
                        draggable: true,
                        title: "Drag to move this session to another pane",
                        ondragstart: move |e: DragEvent| {
                            let dt = e.data_transfer();
                            let _ = dt.set_data(
                                "application/x-rusterm-session-id",
                                &drag_sid,
                            );
                            dt.set_drop_effect("move");
                            dt.set_effect_allowed("move");
                            tracing::debug!(
                                "[DRAG] pane drag started: session={:?}",
                                &drag_sid[..drag_sid.len().min(8)]
                            );
                        },
                        "{pane_title}"
                    },
                    // Terminal content area: fills the remaining height
                    // below the title bar. Wrapped in a flex:1 div so the
                    // title bar (above) stays at 18px and the terminal
                    // gets the rest. `position: relative` + `overflow:
                    // hidden` matches the single-pane path's container.
                    div {
                        style: "flex: 1; position: relative; overflow: hidden; min-height: 0;",
                        {render_terminal_pane(state, input_senders, session_id.clone())}
                    }
                }
            }

            // Vertical splitter bars between adjacent columns.
            {render_col_splitters(&layout, container_w, state, split_drag)}
            // Horizontal splitter bars between adjacent rows.
            {render_row_splitters(&layout, container_h, state, split_drag)}
        }
    }
}

/// Render vertical splitter bars between adjacent columns. Drag the bar to
/// resize the two columns it separates (smooth, continuous — not the prior
/// 5%-step click). Right-click still does a 5% shrink for keyboard-free
/// fine-tuning.
fn render_col_splitters(
    layout: &PaneLayout,
    container_w: f64,
    mut state: Signal<AppState>,
    mut split_drag: Signal<Option<SplitDragState>>,
) -> Element {
    let cols = layout.cols();
    if cols < 2 {
        return rsx! {};
    }
    // Collect into an owned Vec of (col_idx, x_px) tuples. The `for` loop
    // in rsx! can't contain `let` bindings (dioxus 0.7 macro
    // limitation), so we pre-compute owned values here and destructure
    // them in the pattern. `f64` is `Copy`, so this is cheap.
    let boundaries: Vec<(usize, f64)> = {
        let mut out = Vec::new();
        let mut acc = 0.0_f64;
        for (i, frac) in layout.col_fracs.iter().enumerate() {
            if i > 0 {
                out.push((i - 1, acc * container_w));
            }
            acc += frac;
        }
        out
    };
    rsx! {
        for (col_idx, x_val) in boundaries.into_iter() {
            div {
                key: "col-split-{col_idx}",
                style: format!(
                    "position: absolute; left: {x_val}px; top: 0; bottom: 0; width: 6px; \
                     margin-left: -3px; cursor: col-resize; background: #2a2b3d; z-index: 50; \
                     transition: background 0.1s;",
                ),
                // Begin a drag-resize. We capture the splitter's pixel
                // position and the container width at drag start, then
                // install document-level mousemove/mouseup listeners via
                // `eval`. The mousemove listener writes the current
                // clientX to `document._rusterm_split_drag_mouse`; the
                // poll loop in `App` reads that and applies the delta.
                // The mouseup listener clears the drag flag and the
                // `split_drag` signal.
                //
                // `e.prevent_default()` is needed to stop the browser
                // from initiating a text-selection drag (which would
                // hijack the mousemove events and suppress our
                // listener).
                onmousedown: move |e: MouseEvent| {
                    e.prevent_default();
                    if container_w <= 0.0 {
                        return;
                    }
                    split_drag.set(Some(SplitDragState {
                        is_col: true,
                        idx: col_idx,
                        start_pos: x_val,
                        container_extent: container_w,
                        last_applied_pos: x_val,
                    }));
                    // Install the document-level listeners. We write the
                    // starting position immediately so the first poll
                    // sees a valid value (otherwise the first ~16ms of
                    // mousemove events would be lost).
                    let script = "(function() { \
                        document._rusterm_split_drag_mouse = null; \
                        document._rusterm_split_drag_active = true; \
                        document._rusterm_split_drag_axis = 'x'; \
                        function onMove(ev) { \
                            if (!document._rusterm_split_drag_active) return; \
                            document._rusterm_split_drag_mouse = ev.clientX; \
                        } \
                        function onUp(ev) { \
                            document._rusterm_split_drag_active = false; \
                            document._rusterm_split_drag_mouse = null; \
                            document.removeEventListener('mousemove', onMove); \
                            document.removeEventListener('mouseup', onUp); \
                            document.body.style.userSelect = ''; \
                            document.body.style.cursor = ''; \
                        } \
                        document.body.style.userSelect = 'none'; \
                        document.body.style.cursor = 'col-resize'; \
                        document.addEventListener('mousemove', onMove); \
                        document.addEventListener('mouseup', onUp); \
                    })()";
                    spawn(async move {
                        let _ = dioxus::document::eval(script).await;
                    });
                    tracing::debug!(
                        "[LAYOUT] col-split drag started: idx={} start_x={:.1} container_w={:.1}",
                        col_idx, x_val, container_w
                    );
                },
                oncontextmenu: move |e: MouseEvent| {
                    e.prevent_default();
                    let delta = -0.05;
                    if resize_layout_col(&mut state.write(), col_idx, delta) {
                        tracing::info!("[LAYOUT] col {} shrunk by {:.2}", col_idx, -delta);
                    }
                },
                title: "Drag to resize columns, right-click to shrink left",
            }
        }
    }
}

/// Render horizontal splitter bars between adjacent rows. Drag the bar to
/// resize the two rows it separates (smooth, continuous). Right-click still
/// does a 5% shrink.
fn render_row_splitters(
    layout: &PaneLayout,
    container_h: f64,
    mut state: Signal<AppState>,
    mut split_drag: Signal<Option<SplitDragState>>,
) -> Element {
    let rows = layout.rows();
    if rows < 2 {
        return rsx! {};
    }
    let mut boundaries = Vec::new();
    let mut acc = 0.0_f64;
    for (i, frac) in layout.row_fracs.iter().enumerate() {
        if i > 0 {
            boundaries.push(acc * container_h);
        }
        acc += frac;
    }
    // Collect into owned (row_idx, y_px) tuples — see the col-splitter
    // comment for why.
    let boundaries: Vec<(usize, f64)> = boundaries
        .into_iter()
        .enumerate()
        .map(|(i, y)| (i, y))
        .collect();
    rsx! {
        for (row_idx, y_val) in boundaries.into_iter() {
            div {
                key: "row-split-{row_idx}",
                style: format!(
                    "position: absolute; top: {y_val}px; left: 0; right: 0; height: 6px; \
                     margin-top: -3px; cursor: row-resize; background: #2a2b3d; z-index: 50; \
                     transition: background 0.1s;",
                ),
                onmousedown: move |e: MouseEvent| {
                    e.prevent_default();
                    if container_h <= 0.0 {
                        return;
                    }
                    split_drag.set(Some(SplitDragState {
                        is_col: false,
                        idx: row_idx,
                        start_pos: y_val,
                        container_extent: container_h,
                        last_applied_pos: y_val,
                    }));
                    let script = "(function() { \
                        document._rusterm_split_drag_mouse = null; \
                        document._rusterm_split_drag_active = true; \
                        document._rusterm_split_drag_axis = 'y'; \
                        function onMove(ev) { \
                            if (!document._rusterm_split_drag_active) return; \
                            document._rusterm_split_drag_mouse = ev.clientY; \
                        } \
                        function onUp(ev) { \
                            document._rusterm_split_drag_active = false; \
                            document._rusterm_split_drag_mouse = null; \
                            document.removeEventListener('mousemove', onMove); \
                            document.removeEventListener('mouseup', onUp); \
                            document.body.style.userSelect = ''; \
                            document.body.style.cursor = ''; \
                        } \
                        document.body.style.userSelect = 'none'; \
                        document.body.style.cursor = 'row-resize'; \
                        document.addEventListener('mousemove', onMove); \
                        document.addEventListener('mouseup', onUp); \
                    })()";
                    spawn(async move {
                        let _ = dioxus::document::eval(script).await;
                    });
                    tracing::debug!(
                        "[LAYOUT] row-split drag started: idx={} start_y={:.1} container_h={:.1}",
                        row_idx, y_val, container_h
                    );
                },
                oncontextmenu: move |e: MouseEvent| {
                    e.prevent_default();
                    let delta = -0.05;
                    if resize_layout_row(&mut state.write(), row_idx, delta) {
                        tracing::info!("[LAYOUT] row {} shrunk by {:.2}", row_idx, -delta);
                    }
                },
                title: "Drag to resize rows, right-click to shrink top",
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

/// Scan new terminal output for OneKey expect-pattern matches. If any OneKey's
/// expect regex matches and the session's popup isn't already showing, show the
/// popup with the matching entries. Persists across focus changes (only new
/// output triggers this — focus changes produce no output, so no re-scan).
fn check_onekey_match(mut state: Signal<AppState>, session_id: &str, data: &[u8]) {
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
    // Strip ANSI/VT escapes first. Real credential prompts are sometimes
    // colored by the remote (bastion login screens, network-device banners),
    // e.g. `\x1b[1;36mPassword\x1b[0m for 'host': `. Without stripping, the
    // escape bytes between "Password" and "for" break the expect regex → no
    // popup → the password is never autofilled.
    let text = strip_ansi(&String::from_utf8_lossy(data));
    // Only match against the LAST non-empty line of the new output. A prompt
    // (e.g. "Username for …: ") is the last line — the shell is waiting for
    // input. Matching the whole chunk would spuriously fire on injected output
    // (the history import dumps ~77KB of old commands, some of which may
    // contain "password"/"username"), popping up at the wrong time and sending
    // a credential into the wrong place.
    let last_line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
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
                "(function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return ''; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return ''; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const h = rect.height - padV - bh; if (h <= 0) return ''; let w; const sd = document.getElementById('{scroll_cid}'); if (sd && sd.lastElementChild) {{ w = sd.lastElementChild.getBoundingClientRect().width; }} else {{ w = rect.width - padH - bw; }} if (w <= 0) return ''; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor(h / ch)); if (cols > 1 && rows > 1) return cols + ',' + rows; return ''; }})()"
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

        // NOTE: we intentionally do NOT resize the local terminal to
        // measured_size here. The resize future in TerminalView already
        // measures the real container size and resizes the local terminal
        // (and it's more reliable — start_ssh's own measurement can fail and
        // fall back to 80x24, which would overwrite the correct size). The
        // initial PTY resize below sends terminal.size(), which the resize
        // future has already set correctly.

        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let host_for_import = ssh_config.host.clone();
        let client = rusterm_ssh::SshClient::new(ssh_config, event_tx.clone());

        match client.connect(tab_id.clone(), measured_size).await {
            Ok((session, ssh_session)) => {
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

                // Send initial resize to sync PTY with the measured container
                // size. We use `measured_size` (not `terminal.size()`) because the
                // local terminal model is still at its 80x24 default — the
                // TerminalView resize future hasn't fired yet (it needs ~100ms
                // to poll the DOM). Sending `terminal.size()` here would briefly
                // shrink the remote PTY back to 80x24, causing remote output to
                // re-wrap incorrectly until the resize future corrects it. The
                // TerminalView's on_resize handler will keep both the local model
                // and the remote PTY in sync after the first measurement lands.
                //
                // Pixel dims: measured_size.pixel_width is 0 (the connect-time
                // measurement only computes cols/rows, not pixels). That's OK —
                // xterm-pty spec treats pixel dims as advisory; the cols/rows are
                // what matter for line wrapping. The subsequent resize from
                // TerminalView carries the real pixel dims.
                let _ = session.resize_tx.send((
                    measured_size.cols,
                    measured_size.rows,
                    measured_size.pixel_width,
                    measured_size.pixel_height,
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

                // Inject shell integration (OSC 133 + OSC 7) so the shell
                // reports each command's exit code AND its working directory.
                // Additive (appends to precmd_functions / PROMPT_COMMAND) so it
                // won't clobber the user's prompt.
                //
                // OSC 133;D — exit code (existing).
                // OSC 7     — `file://<host><cwd>` (new); used by session-state
                //             restore to know which dir to `cd` back to on
                //             next launch. We never re-execute past commands,
                //             only send a single `cd` per session on restore.
                //
                // NOTE: do NOT send a trailing Ctrl+L (0x0c) to hide the echoed
                // setup line — Ctrl+L clears the WHOLE screen, which wipes the
                // MOTD/session content into scrollback and leaves a blank
                // terminal after every connect. The one-time setup echo is left
                // visible (cosmetic) rather than blanking the session.
                {
                    let integration_tx = session.input_tx.clone();
                    let int_sid = tab_id.clone();
                    let mut setup: Vec<u8> = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; printf '\e]7;file://%s%s\e\\' "${HOSTNAME:-localhost}" "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#.as_bytes().to_vec();
                    setup.push(b'\n');
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        let _ = integration_tx.send(setup);
                        tracing::info!("[SSH] injected shell integration for {}", int_sid);
                    });
                }

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
                                s.disconnected_sessions.insert(id.clone());
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
                let msg = format!("Connection failed: {}\n", e);
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
                                s.disconnected_sessions.insert(id.clone());
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
            let msg = format!("Shell failed: {}\n", e);
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
    let terminal = Terminal::new(TerminalSize::default());
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
    state.write().active_session = Some(tab_id.clone());

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

    // Set active session to the saved one (if it exists in the new tabs).
    // We match by name since the new tab ids are fresh UUIDs.
    if let Some(saved_active) = saved_active {
        // The saved_active id was from the previous launch — it won't match
        // any current tab. Instead, find the tab whose name matches the saved
        // active session's name, or fall back to the last opened tab.
        let saved_name = to_restore
            .sessions
            .iter()
            .find(|s| s.id == saved_active)
            .map(|s| s.name.clone());
        let target_id = if let Some(name) = saved_name {
            state
                .read()
                .sessions
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.id.clone())
        } else {
            None
        };
        state.write().active_session =
            target_id.or_else(|| state.read().sessions.last().map(|t| t.id.clone()));
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

/// Open a connection (SSH / Shell / etc.) as a new session tab.
///
/// This is the shared connection-opening helper used by:
/// - The sidebar's `on_connect` handler (click a connection to open it).
/// - The drag-and-drop drop handler on a pane (drag a sidebar connection
///   onto a pane to open it in that pane).
///
/// ## The `target_pane_idx` parameter
///
/// - `None`: the new session is opened as a new active tab. This is the
///   legacy "click to connect" flow — the new tab becomes active and the
///   single-pane render path displays it.
/// - `Some(pane_idx)`: the new session is opened AND its session_id is
///   assigned to pane `pane_idx` in the active tab's layout (via
///   `set_pane_session_for_active`). This is the drag-and-drop flow —
///   the new session replaces whatever was displayed in the target pane.
///   The new session's tab is still pushed to `state.sessions` (so it
///   appears in the tab bar and can be dragged later), but
///   `active_session` is NOT changed (the user's active tab stays as
///   whatever they were looking at when they dragged).
///
/// If `target_pane_idx` is `Some` but there's no active layout (the
/// user dragged onto a single-pane tab), the function falls back to
/// the `None` path — opens a new active tab. This is the graceful
/// degradation for "drag onto a pane that doesn't exist yet".
fn open_connection(
    mut state: Signal<AppState>,
    input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>>,
    conn: ConnectionConfig,
    target_pane_idx: Option<usize>,
) {
    let tab_id = uuid::Uuid::new_v4().to_string();
    create_terminal(tab_id.clone(), &mut state);
    // Remember the config so this session can be reconnected by pressing
    // Enter after a disconnect.
    state
        .write()
        .session_configs
        .insert(tab_id.clone(), conn.clone());

    match &conn.kind {
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
            // If a target pane was specified, assign the new session to
            // that pane. Otherwise, make it the active session (legacy
            // "click to connect" flow).
            if let Some(idx) = target_pane_idx {
                if !set_pane_session_for_active(&mut state.write(), idx, tab_id.clone()) {
                    // No layout / out-of-range pane — fall back to
                    // making the new session active.
                    state.write().active_session = Some(tab_id.clone());
                }
            } else {
                state.write().active_session = Some(tab_id.clone());
            }
            start_ssh_connection(state, input_senders, tab_id, ssh_config.clone());
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
            if let Some(idx) = target_pane_idx {
                if !set_pane_session_for_active(&mut state.write(), idx, tab_id.clone()) {
                    state.write().active_session = Some(tab_id.clone());
                }
            } else {
                state.write().active_session = Some(tab_id.clone());
            }
            start_shell_connection(state, input_senders, tab_id, shell_config.clone());
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
                if let Some(idx) = target_pane_idx {
                    if !set_pane_session_for_active(&mut state.write(), idx, tab_id.clone()) {
                        state.write().active_session = Some(tab_id.clone());
                    }
                } else {
                    state.write().active_session = Some(tab_id.clone());
                }
            }
        }
    }
}

/// Reconnect a disconnected session: tear down the dead PTY/senders, create a
/// fresh terminal, and re-run the SSH/shell connection using the stored config.
/// Triggered by pressing Enter while a session is in `disconnected_sessions`.
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

    // Clear the disconnected flag + tear down the dead session's senders/terminal.
    {
        let mut s = state.write();
        s.disconnected_sessions.remove(&tab_id);
        s.close_senders.retain(|(sid, _)| sid != &tab_id);
        s.resize_senders.remove(&tab_id);
        s.terminals.remove(&tab_id);
    }
    input_senders.write().remove(&tab_id);

    // Fresh terminal + "Reconnecting..." message.
    create_terminal(tab_id.clone(), &mut state);
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

    match conn.kind {
        ConnectionKind::Ssh(ssh_config) => {
            start_ssh_connection(state, input_senders, tab_id, ssh_config);
        }
        ConnectionKind::Shell(shell_config) => {
            start_shell_connection(state, input_senders, tab_id, shell_config);
        }
        _ => {
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
    // `App` to the signal); writes happen in `ondragenter`/`ondragover`/`ondrop`.
    //
    // PERF: `Signal::set` performs an equality check before triggering a
    // re-render, so calling `set(Some(idx))` when the value is already
    // `Some(idx)` is a no-op. This lets us call `set` in the high-frequency
    // `ondragover` handler (~60Hz) without causing per-tick re-renders — the
    // highlight only changes when the dragged pane actually changes. This
    // matches the user's "取舍分频性能" preference: fewer re-renders over
    // per-tick feedback.
    let drag_over_pane: Signal<Option<usize>> = use_signal(|| None);

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

    // Active splitter-bar drag, if any. Set by the splitter's `onmousedown`;
    // read by the drag-poll future below; cleared by the JS `mouseup` handler
    // (which also sets `document._rusterm_split_drag_active = false`). The poll
    // future detects the JS-side clear and mirrors it here.
    //
    // Why a signal + poll instead of per-event eval callbacks: dioxus's eval
    // bridge is async request/response — it can't deliver high-frequency
    // mousemove events synchronously. Polling at 16ms (~60fps) is smooth
    // enough for splitter dragging and mirrors the TerminalView resize-poll
    // pattern.
    let mut split_drag: Signal<Option<SplitDragState>> = use_signal(|| None);

    // Drag-poll loop: while a splitter drag is in progress, read the current
    // mouse position from `document._rusterm_split_drag_mouse` (kept current
    // by the JS `mousemove` listener installed in the splitter's
    // `onmousedown`), compute the delta from the last applied position, and
    // call `resize_layout_col` / `resize_layout_row` to apply it. The JS
    // `mouseup` handler clears the drag flag; we detect that and clear the
    // Rust-side signal to match.
    //
    // Poll cadence: 16ms (~60fps) only while a drag is in progress — when
    // `split_drag` is `None` we `continue` immediately (after the sleep), so
    // the loop is effectively idle between drags. This matches the existing
    // TerminalView resize-poll pattern (100ms there, but splitter dragging
    // benefits from higher cadence for smoothness).
    let _split_drag_poll = use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            let Some(drag) = split_drag() else {
                continue;
            };
            // Read the current mouse position + active flag from JS.
            let result = dioxus::document::eval(
                "(function() { \
                    if (!document._rusterm_split_drag_active) return 'inactive'; \
                    if (document._rusterm_split_drag_mouse === null || \
                        document._rusterm_split_drag_mouse === undefined) return 'pending'; \
                    return document._rusterm_split_drag_mouse.toFixed(2); \
                })()",
            )
            .await;
            let mut apply_delta: Option<f64> = None;
            let mut drag_ended = false;
            if let Ok(value) = result {
                if let Some(s) = value.as_str() {
                    if s == "inactive" {
                        // JS-side mouseup fired — clear the signal.
                        drag_ended = true;
                    } else if s != "pending" {
                        if let Ok(pos) = s.parse::<f64>() {
                            if pos != drag.last_applied_pos {
                                apply_delta = Some(pos);
                            }
                        }
                    }
                }
            }
            if drag_ended {
                split_drag.set(None);
                continue;
            }
            let Some(new_pos) = apply_delta else {
                continue;
            };
            // Convert the pixel delta to a fractional delta and apply it.
            // Positive delta (mouse moved right/down) grows the earlier
            // column/row and shrinks the later one — matches the
            // `resize_col`/`resize_row` contract.
            let pixel_delta = new_pos - drag.last_applied_pos;
            let frac_delta = if drag.container_extent > 0.0 {
                pixel_delta / drag.container_extent
            } else {
                0.0
            };
            let applied = if drag.is_col {
                resize_layout_col(&mut state.write(), drag.idx, frac_delta)
            } else {
                resize_layout_row(&mut state.write(), drag.idx, frac_delta)
            };
            if applied {
                // Update `last_applied_pos` so we only fire on actual
                // changes. We DON'T update `start_pos` — the delta is
                // computed relative to the last applied position, not the
                // drag origin, which gives smoother behavior when the
                // minimum-frac guard rejects a delta (the next delta is
                // computed from where the mouse actually is, not from
                // where it would have been if the rejected delta had
                // applied).
                split_drag.set(Some(SplitDragState {
                    last_applied_pos: new_pos,
                    ..drag
                }));
            }
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
        // Install the observer first.
        let observer_script = "(function() { const el = document.getElementById('terminal-content'); \
             if (!el || el._rusterm_ro) return; \
             el._rusterm_ro = new ResizeObserver(function() { el._rusterm_container_resize_pending = true; }); \
             el._rusterm_ro.observe(el); })()";
        let _ = dioxus::document::eval(observer_script).await;
        // Force an initial measurement on the first tick so we don't wait
        // 100ms for the first ResizeObserver callback.
        let _ = dioxus::document::eval(
            "(function() { const el = document.getElementById('terminal-content'); \
             if (el) el._rusterm_container_resize_pending = true; })()",
        )
        .await;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let result = dioxus::document::eval(
                "(function() { const el = document.getElementById('terminal-content'); \
                 if (!el || !el._rusterm_container_resize_pending) return ''; \
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
                // Cmd+1..9 (macOS) or Ctrl+1..9 (Linux/Windows) to switch tabs
                let mods = e.modifiers();
                if (mods.meta() || mods.ctrl()) && !mods.alt() && !mods.shift() {
                    if let Key::Character(ref s) = e.key() {
                        if let Ok(idx) = s.parse::<usize>() {
                            if idx >= 1 && idx <= 9 {
                                e.prevent_default();
                                let tabs = state.read().sessions.clone();
                                if let Some(tab) = tabs.get(idx - 1) {
                                    let tab_id = tab.id.clone();
                                    state.write().active_session = Some(tab_id.clone());
                                    let focus_id = format!("terminal-input-{tab_id}");
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
                // Cmd/Ctrl+Shift+L → cycle pane layout preset
                // (1 → 2H → 2V → 4 → 8 → 1).
                if (mods.meta() || mods.ctrl()) && mods.shift() && !mods.alt() {
                    if let Key::Character(ref s) = e.key() {
                        if s.eq_ignore_ascii_case("l") {
                            e.prevent_default();
                            let next = cycle_layout_preset(&mut state.write());
                            if let Some(p) = next {
                                tracing::info!("[LAYOUT] hotkey cycled to {:?}", p);
                            }
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
                            // `target_pane_idx: None` → open as a new
                            // active tab (the legacy "click to connect"
                            // flow). The drag-and-drop drop handler
                            // calls `open_connection` with
                            // `Some(pane_idx)` to open the connection
                            // in a specific pane instead.
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
                }
            }}

            // Main area
            div {
                style: "flex: 1; display: flex; flex-direction: column; overflow: hidden; min-width: 0;",

                // Tab bar
                TabBar {
                    tabs: state.read().sessions.clone(),
                    active: state.read().active_session.clone(),
                    on_select: move |id: String| {
                        state.write().active_session = Some(id.clone());
                        let focus_id = format!("terminal-input-{id}");
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let _ = dioxus::document::eval(&format!(
                                "document.getElementById('{focus_id}')?.focus()"
                            )).await;
                        });
                    },
                    on_close: move |id: String| {
                        input_senders.write().remove(&id);
                        if let Some((_, tx)) = state.read().close_senders.iter().find(|(sid, _)| sid == &id).cloned() {
                            let _ = tx.send(());
                        }
                        state.write().close_senders.retain(|(sid, _)| sid != &id);
                        state.write().resize_senders.remove(&id);
                        state.write().terminals.remove(&id);
                        state.write().sessions.retain(|s| s.id != id);
                        // --- Layout cleanup ---
                        // Remove the closed session's own layout entry (if it
                        // was a tab anchor) and clear the session from any
                        // other tab's layout panes (so a dangling reference
                        // doesn't try to render a dead session).
                        state.write().layouts.remove(&id);
                        for (_, layout) in state.write().layouts.iter_mut() {
                            for pane in layout.panes.iter_mut() {
                                if pane.session_id == id {
                                    pane.session_id = String::new();
                                }
                            }
                        }
                        let first_id = state.read().sessions.first().map(|s| s.id.clone());
                        if state.read().active_session.as_ref() == Some(&id) {
                            state.write().active_session = first_id;
                        }
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
                    {let active_id = state.read().active_session.clone();
                    let layout_snapshot = active_id.as_ref()
                        .and_then(|sid| state.read().layouts.get(sid).cloned());
                    let is_multi = layout_snapshot.as_ref()
                        .is_some_and(|l| l.is_multi_pane());
                    match (active_id, is_multi) {
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
                            // of `sid` (the active session). This is what makes
                            // zoom mode actually work: the user's active tab
                            // is `sid`, but the visible content is the zoomed
                            // pane's session.
                            let render_sid = layout_snapshot.as_ref()
                                .and_then(|l| l.zoomed)
                                .and_then(|idx| layout_snapshot.as_ref()?.panes.get(idx).map(|p| p.session_id.clone()))
                                .unwrap_or(sid);
                            render_terminal_pane(state, input_senders, render_sid)
                        }
                        (Some(_sid), true) => {
                            // Multi-pane path: iterate over visible panes and
                            // render each one positioned absolutely via the
                            // layout's pane_rect. Splitter bars are rendered
                            // between panes to support drag-resize.
                            let layout = layout_snapshot.expect("is_multi implies layout exists");
                            rsx! {
                                {multi_pane_container(state, input_senders, layout, drag_over_pane, container_size, split_drag)}
                            }
                        }
                    }}

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
                    div {
                        style: "margin-left: auto; display: flex; gap: 12px; align-items: center;",

                        // --- Multi-pane layout controls ---
                        // The layout toolbar lets the user cycle the active tab's
                        // pane preset (Single → Split2H → Split2V → Grid4 → Grid8),
                        // toggle the cross-terminal comparison mode (synchronized
                        // scrolling + input broadcast), and zoom the active pane
                        // to fill the container (全屏模式). When no session is
                        // active, these are no-ops (cycle returns None).
                        span {
                            style: "cursor: pointer; color: #7aa2f7; font-size: 11px; user-select: none;",
                            onclick: move |_| {
                                let next = cycle_layout_preset(&mut state.write());
                                if let Some(p) = next {
                                    tracing::info!("[LAYOUT] cycled to {:?}", p);
                                }
                            },
                            title: "Cycle pane layout (1 → 2H → 2V → 4 → 8 → 1)",
                            "Layout: {layout_label(state.read().layout_preset)}"
                        }
                        span {
                            style: format!(
                                "cursor: pointer; font-size: 11px; user-select: none; padding: 0 6px; border-radius: 3px; {};",
                                if state.read().layouts.get(&state.read().active_session.clone().unwrap_or_default())
                                    .is_some_and(|l| l.comparison) {
                                    "background: #7aa2f7; color: #1a1b26;"
                                } else {
                                    "color: #7aa2f7; border: 1px solid #2a2b3d;"
                                }
                            ),
                            onclick: move |_| {
                                let on = toggle_comparison_mode(&mut state.write());
                                tracing::info!("[LAYOUT] comparison mode toggled: {:?}", on);
                            },
                            title: "Toggle comparison mode (sync scroll + broadcast input)",
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
                            style: "cursor: pointer; color: #9ece6a;",
                            onclick: move |_| open_local_terminal(state, input_senders),
                            title: "Open a local shell (zsh/bash)",
                            "Local"
                        }
                    }
                }
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
                    s.active_session = Some(config.id.clone());
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
