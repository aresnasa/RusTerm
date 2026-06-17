use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use rusterm_core::config::{ConnectionConfig, ConnectionKind, ShellConfig, SshAuth, SshConfig};
use rusterm_core::config_manager::ConfigManager;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::SessionType;
use rusterm_core::session_log::SessionLog;
use rusterm_core::terminal::{Terminal, TerminalSize};

use crate::components::Sidebar;
use crate::components::TabBar;
use crate::components::TerminalView;
use crate::components::ConnectionDialog;
use crate::components::AiPanel;
use crate::components::MasterPasswordDialog;
use crate::components::connection_dialog::NewConnectionForm;
use crate::state::{AppState, Modal, SessionTab, TerminalEntry, UnlockState};

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
        for attempt in 0..10 {
            let delay = if attempt < 3 { 50 } else { 100 };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            if let Ok(result) = dioxus::document::eval(&format!(
                "(function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return ''; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return ''; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const w = rect.width - padH - bw; const h = rect.height - padV - bh; if (w <= 0 || h <= 0) return ''; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor((h - 1) / ch)); if (cols > 1 && rows > 1) return cols + ',' + rows; return ''; }})()"
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

        // Resize local terminal to measured size before connecting
        if measured_size.cols > 1 && measured_size.rows > 1 {
            let terminals = state.read().terminals.clone();
            if let Some(handle) = terminals.get(&tab_id) {
                handle.lock().terminal.resize(measured_size.cols, measured_size.rows);
            }
        }

        let (event_tx, mut event_rx) =
            tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
        let client = rusterm_ssh::SshClient::new(ssh_config, event_tx.clone());

        match client
            .connect(tab_id.clone(), measured_size)
            .await
        {
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
                        let _ = session.resize_tx.send((size.cols, size.rows, size.pixel_width, size.pixel_height));
                    }
                }

                let _session_guard = session;

                // Pre-seed session history from local shell history
                {
                    let provider = rusterm_history::HybridHistoryProvider::new();
                    let initial_history: Vec<String> = provider.search("", 3000)
                        .into_iter()
                        .map(|m| m.command)
                        .collect();
                    if !initial_history.is_empty() {
                        let mut s = state.write();
                        if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                            tab.command_history = initial_history;
                        }
                    }
                }

                let _conn_guard = ssh_session;

                while let Some(event) = event_rx.recv().await {
                    match event {
                        SessionEvent::Output(id, data) => {
                            // Log output
                            {
                                let logs = state.read().session_logs.clone();
                                if let Some(log) = logs.get(&id) {
                                    log.lock().log_output(&data);
                                }
                            }
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let render_result = handle.lock().process_and_render(&data);
                                let mut s = state.write();
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::Disconnected(id, reason) => {
                            input_senders.write().remove(&id);
                            let msg = format!("\r\n--- Disconnected: {} ---\r\n", reason);
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let render_result = handle.lock().process_and_render(msg.as_bytes());
                                let mut s = state.write();
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::RemoteHistory(id, commands) => {
                            tracing::info!("[SSH] received remote history: {} commands for {}", commands.len(), &id[..id.len().min(8)]);
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                tab.command_history = commands;
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
    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::unbounded_channel::<SessionEvent>();

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

            // Pre-populate local shell history from native history files
            {
                let provider = rusterm_history::HybridHistoryProvider::new();
                let local_history: Vec<String> = provider.search("", 2000)
                    .into_iter()
                    .map(|m| m.command)
                    .collect();
                if !local_history.is_empty() {
                    let mut s = state.write();
                    if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == tab_id) {
                        tab.command_history = local_history;
                    }
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
                                let render_result = handle.lock().process_and_render(&data);
                                let mut s = state.write();
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::Disconnected(id, reason) => {
                            input_senders.write().remove(&id);
                            let msg = format!("\r\n--- Disconnected: {} ---\r\n", reason);
                            let terminals = state.read().terminals.clone();
                            if let Some(handle) = terminals.get(&id) {
                                let render_result = handle.lock().process_and_render(msg.as_bytes());
                                let mut s = state.write();
                                if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                    tab.render_output = render_result;
                                    tab.version += 1;
                                }
                            }
                        }
                        SessionEvent::RemoteHistory(id, commands) => {
                            let mut s = state.write();
                            if let Some(tab) = s.sessions.iter_mut().find(|t| t.id == id) {
                                tab.command_history = commands;
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
    // Create session log
    if let Ok(log) = SessionLog::new(&id) {
        state.write().session_logs.insert(id, Arc::new(Mutex::new(log)));
    }
}

#[component]
pub fn App() -> Element {
    let mut state = use_signal(AppState::default);
    let mut modal = use_signal(|| Modal::None);
    let ai_suggestions = use_signal(Vec::<rusterm_ai::suggestion::AiSuggestion>::new);
    let mut input_senders: Signal<HashMap<String, mpsc::UnboundedSender<Vec<u8>>>> =
        use_signal(HashMap::new);

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
                                state.write().config_manager = Some(cm);
                                state.write().connections = connections;
                                state.write().unlock_state = UnlockState::Unlocked;
                                state.write().master_password_error = None;
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
                                    let senders = input_senders;
                                    let mut state_for_cmd = state;
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
                                                // Log input
                                                {
                                                    let logs = state_for_cmd.read().session_logs.clone();
                                                    if let Some(log) = logs.get(&sid_clone) {
                                                        log.lock().log_input(&data);
                                                    }
                                                }
                                                if let Some(sender) = senders.read().get(&sid_clone) {
                                                    let _ = sender.send(data);
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

                                                        // 1. Session command history (most recent, prefix match, top 3)
                                                        let session_hist = state_for_cmd.read().sessions
                                                            .iter().find(|t| t.id == sid_sug)
                                                            .map(|t| t.command_history.clone())
                                                            .unwrap_or_default();

                                                        for cmd in session_hist.iter().rev() {
                                                            if cmd.to_lowercase().starts_with(&cmd_lower)
                                                                && cmd.len() > cmd_part.len()
                                                                && !seen.contains(cmd.to_lowercase().as_str())
                                                            {
                                                                seen.insert(cmd.to_lowercase().clone());
                                                                all_suggestions.push(cmd.clone());
                                                                if all_suggestions.len() >= 3 { break; }
                                                            }
                                                        }

                                                        // 2. Local shell history files (atuin/zsh/bash/fish, top 5)
                                                        {
                                                            let provider = rusterm_history::HybridHistoryProvider::new();
                                                            let results = provider.search(&cmd_part, 5);
                                                            for m in results {
                                                                if m.command.to_lowercase().starts_with(&cmd_lower)
                                                                    && m.command.len() > cmd_part.len()
                                                                    && !seen.contains(m.command.to_lowercase().as_str())
                                                                {
                                                                    seen.insert(m.command.to_lowercase().clone());
                                                                    all_suggestions.push(m.command);
                                                                }
                                                            }
                                                        }

                                                        // 3. SQLite FTS5 (cross-session global, top 5)
                                                        {
                                                            let db_path = dirs::data_dir()
                                                                .unwrap_or_default()
                                                                .join("rusterm")
                                                                .join("rusterm.db");
                                                            if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                                                if let Ok(results) = db.search_history(&cmd_part, 5).await {
                                                                    for entry in results {
                                                                        if entry.command.to_lowercase().starts_with(&cmd_lower)
                                                                            && entry.command.len() > cmd_part.len()
                                                                            && !seen.contains(entry.command.to_lowercase().as_str())
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
                                                        all_suggestions.truncate(8);

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
                                                            let suffix = all_suggestions[0][cmd_part.len()..].to_string();
                                                            let show_dropdown = all_suggestions.len() > 1;
                                                            state_for_cmd.write().sessions.iter_mut()
                                                                .find(|t| t.id == sid_sug)
                                                                .map(|tab| {
                                                                    tab.suggestion = Some(suffix);
                                                                    tab.suggestions = all_suggestions;
                                                                    tab.suggestion_visible = show_dropdown;
                                                                    tab.suggestion_selected = 0;
                                                                });
                                                        }
                                                    });
                                                }
                                            },
                                            on_command: move |_: String| {
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
                                                    if !cmd.is_empty() {
                                                        // Add to session command history (for inline suggestions)
                                                        state_for_cmd.write().sessions.iter_mut()
                                                            .find(|t| t.id == sid_for_cmd)
                                                            .map(|tab| {
                                                                // Avoid duplicates at the end
                                                                if tab.command_history.last() != Some(&cmd) {
                                                                    tab.command_history.push(cmd.clone());
                                                                }
                                                            });

                                                        // Also persist to DB
                                                        let sid = sid_for_cmd.clone();
                                                        spawn(async move {
                                                            let db_path = dirs::data_dir()
                                                                .unwrap_or_default()
                                                                .join("rusterm")
                                                                .join("rusterm.db");
                                                            if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                                                let entry = rusterm_db::history::HistoryEntry {
                                                                    id: uuid::Uuid::new_v4().to_string(),
                                                                    command: cmd,
                                                                    session_id: sid,
                                                                    cwd: None,
                                                                    hostname: None,
                                                                    exit_code: None,
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
                    });
                    s.active_session = Some(config.id.clone());
                }
                save_config(&state);
                modal.set(Modal::None);

                start_ssh_connection(state, input_senders, config.id, ssh_config);
            },
        }
    }
}

/// Strip shell prompt from a terminal line, returning just the command part.
/// Handles common prompt patterns like "user@host:~$ cmd", "[user@host]$ cmd",
/// "❯ cmd", etc. Falls back to trying word-boundary suffixes.
fn strip_prompt(line: &str) -> String {
    if line.is_empty() { return String::new(); }

    // Try common prompt-ending markers (dollar+space, hash+space, etc.)
    let prompt_markers = ["$ ", "# ", "% ", "> ", "\u{276f} ", "\u{279c} "];
    for marker in prompt_markers {
        if let Some(idx) = line.rfind(marker) {
            let cmd = line[idx + marker.len()..].trim();
            if !cmd.is_empty() {
                return cmd.to_string();
            }
        }
    }

    // Fallback: try stripping words from the left.
    // A prompt typically has 2-5 words before the command.
    // Check if any suffix looks like a command (starts with a common pattern).
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() > 2 {
        // Try from word index 1 onwards — skip at least the first prompt word
        for start in 1..words.len().min(5) {
            let suffix = words[start..].join(" ");
            // Heuristic: if this suffix starts with a common command prefix, use it
            if looks_like_command(&suffix) {
                return suffix;
            }
        }
    }

    line.to_string()
}

/// Quick heuristic: does this text look like the start of a shell command?
fn looks_like_command(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    if first.is_empty() { return false; }
    // Common command starters — if the first word matches, it's likely a command
    let common = [
        "ls", "cd", "cat", "grep", "find", "awk", "sed", "make", "git", "docker",
        "npm", "cargo", "python", "python3", "node", "go", "rustup", "vim", "nvim",
        "emacs", "ssh", "scp", "rsync", "curl", "wget", "tar", "zip", "unzip",
        "sudo", "apt", "yum", "brew", "pip", "pip3", "echo", "mkdir", "rm", "cp",
        "mv", "chmod", "chown", "ps", "top", "htop", "kill", "df", "du", "free",
        "export", "source", "alias", "which", "type", "man", "less", "more",
        "head", "tail", "sort", "uniq", "wc", "diff", "patch", "xargs", "tee",
        "jq", "yq", "terraform", "ansible", "kubectl", "helm", "aws", "gcloud",
        "az", "open", "pbcopy", "pbpaste", "launchctl", "systemctl", "service",
        "ping", "traceroute", "netstat", "ss", "ip", "ifconfig", "env", "printenv",
        "date", "cal", "whoami", "id", "uname", "hostname", "uptime", "w", "who",
        "history", "clear", "reset", "exit", "logout", "reboot", "shutdown",
    ];
    common.contains(&first) || first.contains('/') || first.contains('.') || first.starts_with('-')
}
