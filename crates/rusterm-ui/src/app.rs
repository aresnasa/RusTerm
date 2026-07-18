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
use crate::components::MasterPasswordDialog;
use crate::components::OneKeyManager;
use crate::components::Sidebar;
use crate::components::TabBar;
use crate::components::TerminalView;
use crate::components::connection_dialog::NewConnectionForm;
use crate::state::{
    AppState, Modal, OneKeyMatch, OneKeyPopupState, SessionTab, TerminalEntry, UnlockState,
    move_session_to_leftmost,
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

                // Send initial resize to sync PTY with actual terminal size
                {
                    let terminals = state.read().terminals.clone();
                    if let Some(handle) = terminals.get(&tab_id) {
                        let size = handle.lock().terminal.size();
                        let _ = session.resize_tx.send((
                            size.cols,
                            size.rows,
                            size.pixel_width,
                            size.pixel_height,
                        ));
                    }
                }

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

                // Inject shell integration (OSC 133) so the shell reports each
                // command's exit code. Additive (appends to precmd_functions /
                // PROMPT_COMMAND) so it won't clobber the user's prompt.
                // NOTE: do NOT send a trailing Ctrl+L (0x0c) to hide the echoed
                // setup line — Ctrl+L clears the WHOLE screen, which wipes the
                // MOTD/session content into scrollback and leaves a blank
                // terminal after every connect. The one-time setup echo is left
                // visible (cosmetic) rather than blanking the session.
                {
                    let integration_tx = session.input_tx.clone();
                    let int_sid = tab_id.clone();
                    let mut setup: Vec<u8> = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#.as_bytes().to_vec();
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
                                let (render_result, exit_code) = {
                                    let mut entry = handle.lock();
                                    (
                                        entry.process_and_render(&data),
                                        entry.terminal.take_exit_code(),
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
                                    }
                                }
                                if let Some((cmd, db_id, hostname)) = committed {
                                    let sid = id.clone();
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
                                                created_at: chrono::Utc::now().to_rfc3339(),
                                            };
                                            if let Err(e) = db.save_history(entry).await {
                                                tracing::warn!("Failed to save history: {}", e);
                                            }
                                        }
                                    });
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
                let mut setup: Vec<u8> = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#.as_bytes().to_vec();
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
                                let (render_result, exit_code) = {
                                    let mut entry = handle.lock();
                                    (
                                        entry.process_and_render(&data),
                                        entry.terminal.take_exit_code(),
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
                                    }
                                }
                                if let Some((cmd, db_id, hostname)) = committed {
                                    let sid = id.clone();
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
                                                created_at: chrono::Utc::now().to_rfc3339(),
                                            };
                                            if let Err(e) = db.save_history(entry).await {
                                                tracing::warn!("Failed to save history: {}", e);
                                            }
                                        }
                                    });
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
    });
    state.write().active_session = Some(tab_id.clone());

    start_shell_connection(state, input_senders, tab_id, shell_config);
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
    let _history_import = use_future(|| async move {
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
                                s.config_manager = Some(cm);
                                s.connections = connections;
                                s.onekeys = onekeys;
                                s.unlock_state = UnlockState::Unlocked;
                                s.master_password_error = None;
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
            },

            // Sidebar
            {rsx! {
                Sidebar {
                    connections: state.read().connections.clone(),
                    on_connect: move |id: String| {
                        let conn = state.read().connections.iter().find(|c| c.id == id).cloned();
                        if let Some(conn) = conn {
                            let tab_id = uuid::Uuid::new_v4().to_string();
                            create_terminal(tab_id.clone(), &mut state);
                            // Remember the config so this session can be
                            // reconnected by pressing Enter after a disconnect.
                            state.write().session_configs.insert(tab_id.clone(), conn.clone());

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
                                    });
                                    state.write().active_session = Some(tab_id.clone());
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
                                    });
                                    state.write().active_session = Some(tab_id.clone());
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
                                        });
                                        state.write().active_session = Some(tab_id.clone());
                                    }
                                }
                            }
                        }
                    },
                    on_new: move |_| {
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

                    match state.read().active_session {
                        None => rsx! {
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
                        Some(ref sid) => {
                            let tabs = &state.read().sessions;
                            match tabs.iter().find(|t| t.id == *sid) {
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
                                    let ok_popup = state.read().onekey_popups.get(&tab.id).cloned().unwrap_or_default();
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
                                                    entry.terminal.resize(cols, rows);
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
                                                // Log input
                                                {
                                                    let logs = state_for_cmd.read().session_logs.clone();
                                                    if let Some(log) = logs.get(&sid_clone) {
                                                        log.lock().log_input(&data);
                                                    }
                                                }
                                                let send_ok = senders.read().get(&sid_clone).cloned();
                                                if let Some(sender) = send_ok {
                                                    match sender.send(data) {
                                                        Ok(()) => tracing::info!("[INPUT] sent to PTY ok"),
                                                        Err(e) => tracing::warn!("[INPUT] FAILED to send to PTY: {}", e),
                                                    }
                                                } else {
                                                    tracing::warn!("[INPUT] no sender for session — PTY channel is dead");
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

                                                        if state_for_cmd.read().suggestion_epoch != epoch {
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
                    }

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
            on_close: move |_| modal.set(Modal::None),
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
                    });
                    s.active_session = Some(config.id.clone());
                }
                save_config(&state);
                modal.set(Modal::None);

                start_ssh_connection(state, input_senders, config.id, ssh_config);
            },
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
