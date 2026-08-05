//! Floating, draggable agent chat box (issue #122).
//!
//! In floating mode the panel is anchored to the bottom-left of the window
//! by default and can be repositioned by dragging its title bar (clamped to
//! the window bounds). A title-bar toggle cycles floating → right dock →
//! bottom dock; in the docked modes `app.rs` renders the panel inside
//! `#main` so it merges into the main window layout instead of overlapping
//! the terminals. The panel doubles as a command
//! palette: typing `/` as the first character switches the input into
//! command-search mode, fuzzy-filtering the user's command history (from the
//! DuckDB-backed `rusterm_db::history` store) plus a small built-in list of
//! app commands. Pressing `Tab` or `Escape` hands focus back to the active
//! terminal pane — the "Tab into terminal" gesture.
//!
//! Agent configuration (provider / model / base URL / system prompt) is edited
//! via a popover rendered as a scrollable overlay (so the Save button can
//! never be clipped by the panel bounds) and persisted to
//! `PersistedConfig::chat`. API
//! keys are NOT stored in config — they're entered per-session in the popover
//! and held only in memory for the lifetime of the app (matching the project's
//! "never persist secrets in settings.json" policy; a future change can route
//! them through the keychain like OneKey credentials).
//!
//! ## Drag mechanism
//!
//! Mirrors the proven document-capture + polling pattern already used by
//! splitter resize, tab drag, and freeform pane-window move (see
//! `app.rs::_pane_move_poll`). The title bar's `onmousedown` installs
//! capture-phase `mousemove`/`mouseup` listeners on `document` that write the
//! cursor position to a JS global; a `use_future` polls that global at ~60Hz
//! and updates `chat_settings.position`. `mouseup` sets a done flag so the
//! loop stops. This avoids dioxus 0.7's unreliable element-level drag events.

use dioxus::prelude::*;

use rusterm_core::config::{
    AgentConfig, ChatAgentProvider, ChatDock, ChatPosition, ChatSettings, SkinSettings,
};

use crate::state::{AppState, ChatCommandEntry, ChatCommandSource, ChatMessage, ChatRole};

/// Default panel size (logical px). Overridden by persisted size once the
/// user has resized (TODO: resize handle — for now the size is fixed at these
/// defaults unless the config already carries a non-zero value).
const DEFAULT_CHAT_WIDTH: f64 = 380.0;
const DEFAULT_CHAT_HEIGHT: f64 = 360.0;
/// Padding from the container edge when the panel is first anchored to the
/// bottom-left.
const DEFAULT_EDGE_PADDING: f64 = 12.0;
/// Fixed title-bar height (logical px) so the agent-config popover overlay
/// can anchor directly below it.
const CHAT_TITLE_HEIGHT: f64 = 33.0;

/// Built-in app commands surfaced in the `/` palette. These are intentionally
/// a tiny, high-value starter set — the palette is extensible later.
const APP_COMMANDS: &[(&str, &str)] = &[
    ("new connection", "Open the new-connection dialog"),
    ("toggle split", "Toggle split-pane layout mode"),
    ("toggle zoom", "Zoom the focused pane"),
    ("toggle comparison", "Toggle synchronized multi-pane input"),
    ("toggle chat", "Show/hide this chat panel"),
    ("focus terminal", "Move keyboard focus back to the terminal"),
];

#[component]
pub fn ChatPanel(
    state: Signal<AppState>,
    /// Current UI skin. The panel root is rendered OUTSIDE `#main` (where the
    /// `--skin-*` custom properties are declared), so it must re-declare them
    /// itself for its inline styles / `render_message` bubbles to resolve
    /// instead of falling back to browser defaults.
    skin: SkinSettings,
    on_save_chat: EventHandler<ChatSettings>,
    on_focus_terminal: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    // Same OS-theme read as the main window chrome (see App / SettingsDialog).
    let system_is_dark = matches!(
        dioxus::desktop::window().theme(),
        dioxus::desktop::tao::window::Theme::Dark
    );
    let skin_style = crate::skin::css_variables(&skin, system_is_dark);

    // ALL hooks must run unconditionally and in the same order every render
    // (Dioxus hooks rules). The visibility check happens AFTER the hooks so
    // toggling chat_visible can't desync the hook index.

    // Local UI signals that don't belong on AppState (transient popover state).
    let mut show_agent_config = use_signal(|| false);
    let mut draft_api_key = use_signal(String::new);
    let mut drag_active = use_signal(|| false);
    // Agent-config popover draft fields. Lifted here (not in the helper) so
    // they're real Dioxus signals — hooks can't run inside a plain fn.
    let mut draft_name = use_signal(String::new);
    let mut draft_model = use_signal(String::new);
    let mut draft_base_url = use_signal(String::new);
    let mut draft_prompt = use_signal(String::new);

    // ── Drag polling loop ────────────────────────────────────────────────
    // Installs document-level capture listeners on mousedown and polls the
    // JS cursor global at ~60Hz until mouseup. See module docs for the
    // rationale (dioxus 0.7 element-level drag events are unreliable).
    let _drag_poll = use_future(move || async move {
        loop {
            if !drag_active() {
                tokio::time::sleep(std::time::Duration::from_millis(32)).await;
                continue;
            }
            match poll_chat_drag().await {
                Some((x, y, done)) => {
                    // Copy the drag offset + current panel size out of the
                    // read guard BEFORE taking a write guard — `state.read()`
                    // returns a temporary guard that can't overlap
                    // `state.write()`.
                    let (offset, pw, ph) = {
                        let s = state.read();
                        let (pw, ph) = panel_size(&s.chat_settings);
                        (s.chat_drag_offset, pw, ph)
                    };
                    if let Some((off_x, off_y)) = offset {
                        // Clamp to the window bounds so the panel can never
                        // be dragged off-screen (where its position would
                        // then persist and the panel would be "lost").
                        let (win_w, win_h) = window_logical_size();
                        let new_pos = clamp_position(
                            ChatPosition {
                                x: x - off_x,
                                y: y - off_y,
                            },
                            pw,
                            ph,
                            win_w,
                            win_h,
                        );
                        state.write().chat_settings.position = new_pos;
                    }
                    if done {
                        tracing::info!("[CHAT] drag finished at ({:.1}, {:.1})", x, y);
                        drag_active.set(false);
                        state.write().chat_drag_offset = None;
                        // Remove the document-capture listeners so subsequent
                        // mouse moves don't keep writing to the global.
                        spawn(async move {
                            let _ = dioxus::document::eval(
                                "(function() {\n\
                                    if (window.__rusterm_chat_drag_cleanup) {\n\
                                        window.__rusterm_chat_drag_cleanup();\n\
                                        window.__rusterm_chat_drag_cleanup = null;\n\
                                    }\n\
                                    window.__rusterm_chat_drag_pos = '';\n\
                                    window.__rusterm_chat_drag_done = '';\n\
                                })()",
                            )
                            .await;
                        });
                        let updated = state.read().chat_settings.clone();
                        on_save_chat.call(updated);
                        continue;
                    }
                }
                None => tracing::debug!("[CHAT] drag poll returned no position; retrying"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
    });

    // The panel is unmounted when hidden so its message log doesn't
    // accumulate while the user isn't looking at it. (Hooks above still ran.)
    let visible = state.read().chat_visible;
    if !visible {
        return rsx! {};
    }

    let settings = state.read().chat_settings.clone();
    let messages = state.read().chat_messages.clone();
    let input_text = state.read().chat_input.clone();
    let command_mode = state.read().chat_command_mode;
    let command_results = state.read().chat_command_results.clone();
    let command_selected = state.read().chat_command_selected;
    let status = state.read().chat_status.clone();

    let (width, height) = panel_size(&settings);
    let position = resolved_position(&settings, width, height);
    // Pre-compute i18n strings so the rsx! interpolations don't need nested
    // quotes (which break the rsx! format parser).
    let placeholder_text = crate::i18n::t("chat.placeholder").to_string();
    let send_label = if command_mode {
        crate::i18n::t("chat.run")
    } else {
        crate::i18n::t("chat.send")
    };
    let status_text = status
        .clone()
        .unwrap_or_else(|| crate::i18n::t("chat.hint").to_string());

    let start_drag = move |e: MouseEvent| {
        // Docked panels participate in the window layout — dragging their
        // title bar must not start the floating-panel drag loop.
        if state.read().chat_settings.dock != ChatDock::Floating {
            return;
        }
        // Capture the offset between the cursor and the panel origin so the
        // panel doesn't jump to put the cursor at its top-left corner. We
        // use client (viewport) coordinates to match the JS-side
        // `e.clientX`/`e.clientY` the document-capture listeners write.
        let (cur_x, cur_y) = (e.client_coordinates().x, e.client_coordinates().y);
        e.prevent_default();
        e.stop_propagation();
        state.write().chat_drag_offset = Some((cur_x - position.x, cur_y - position.y));
        drag_active.set(true);
        // Install document-level capture-phase listeners that write the cursor
        // position to a JS global. The `use_future` above polls that global at
        // ~60Hz and updates `chat_settings.position`. Fire-and-forget via
        // `spawn` (dioxus 0.7 element-level drag events are unreliable —
        // document capture is the proven pattern, see app.rs pane-move).
        let script = format!(
            "(function(){{\
               var mv=function(e){{window.__rusterm_chat_drag_pos=e.clientX+','+e.clientY;}};\
               var up=function(e){{window.__rusterm_chat_drag_done='1';window.__rusterm_chat_drag_pos=e.clientX+','+e.clientY;}};\
               window.__rusterm_chat_drag_pos='{x},{y}';window.__rusterm_chat_drag_done='';\
               document.addEventListener('mousemove',mv,true);\
               document.addEventListener('mouseup',up,true);\
               window.__rusterm_chat_drag_cleanup=function(){{\
                 document.removeEventListener('mousemove',mv,true);\
                 document.removeEventListener('mouseup',up,true);\
               }};\
             }})()",
            x = cur_x,
            y = cur_y,
        );
        spawn(async move {
            let _ = dioxus::document::eval(&script).await;
        });
    };

    let on_input_change = move |e: FormEvent| {
        let value = e.value();
        let mode = value.starts_with('/');
        {
            let mut s = state.write();
            s.chat_input = value.clone();
            s.chat_command_mode = mode;
            if mode {
                // Seed the dropdown synchronously with the built-in app
                // commands so there's zero-latency feedback. The DB-backed
                // history results are merged in async below.
                s.chat_command_results = builtin_commands(&value[1..]);
                s.chat_command_selected = 0;
            } else {
                s.chat_command_results.clear();
            }
        }
        if mode {
            let query = value[1..].to_string();
            let mut state_clone = state;
            spawn(async move {
                let history = query_history(&query).await.unwrap_or_default();
                let mut s = state_clone.write();
                // Only update if the user is still in command mode for the
                // same query — they may have kept typing or exited the mode.
                if !s.chat_command_mode || !s.chat_input.starts_with('/') {
                    return;
                }
                let current_query = s.chat_input[1..].trim().to_ascii_lowercase();
                if current_query != query.trim().to_ascii_lowercase() {
                    return;
                }
                // Merge: app commands first (already present), then history,
                // dedup by command text.
                let mut seen: std::collections::HashSet<String> = s
                    .chat_command_results
                    .iter()
                    .map(|e| e.command.clone())
                    .collect();
                for entry in history {
                    if seen.insert(entry.command.clone()) {
                        s.chat_command_results.push(ChatCommandEntry {
                            command: entry.command,
                            source: ChatCommandSource::History,
                        });
                    }
                    if s.chat_command_results.len() >= 50 {
                        break;
                    }
                }
            });
        }
    };

    let on_input_keydown = move |e: KeyboardEvent| {
        let mods = e.modifiers();
        // Tab or Escape → hand focus back to the terminal ("Tab into terminal").
        if e.key() == Key::Tab || e.key() == Key::Escape {
            e.prevent_default();
            e.stop_propagation();
            on_focus_terminal.call(());
            return;
        }
        let command_mode_now = state.read().chat_command_mode;
        if command_mode_now {
            let results = state.read().chat_command_results.clone();
            let count = results.len();
            match e.key() {
                Key::ArrowDown => {
                    e.prevent_default();
                    if count > 0 {
                        let mut s = state.write();
                        s.chat_command_selected = (s.chat_command_selected + 1) % count;
                    }
                }
                Key::ArrowUp => {
                    e.prevent_default();
                    if count > 0 {
                        let mut s = state.write();
                        s.chat_command_selected = (s.chat_command_selected + count - 1) % count;
                    }
                }
                Key::Enter => {
                    e.prevent_default();
                    let selected = state.read().chat_command_selected;
                    if let Some(entry) = results.get(selected).cloned() {
                        // Insert the chosen command into the active terminal
                        // and hand focus over. This is the "command palette →
                        // terminal" flow.
                        run_command_in_terminal(state, &entry.command);
                        let mut s = state.write();
                        s.chat_input.clear();
                        s.chat_command_mode = false;
                        s.chat_command_results.clear();
                        s.chat_command_selected = 0;
                    }
                    on_focus_terminal.call(());
                }
                _ => {}
            }
            return;
        }
        // Normal chat mode.
        if e.key() == Key::Enter && !mods.shift() {
            e.prevent_default();
            send_message(state);
        }
    };

    let on_send_click = move |_| {
        if state.read().chat_command_mode {
            let results = state.read().chat_command_results.clone();
            let selected = state.read().chat_command_selected;
            if let Some(entry) = results.get(selected).cloned() {
                run_command_in_terminal(state, &entry.command);
                let mut s = state.write();
                s.chat_input.clear();
                s.chat_command_mode = false;
                s.chat_command_results.clear();
            }
            on_focus_terminal.call(());
        } else {
            send_message(state);
        }
    };

    let mut on_close = move |_| {
        // Mirror the hidden state into `chat_settings.visible` BEFORE saving —
        // otherwise the persisted settings still say `visible: true` and the
        // panel reappears on the next launch ("config doesn't save").
        let mut s = state.write();
        s.chat_visible = false;
        s.chat_settings.visible = false;
        let updated = s.chat_settings.clone();
        drop(s);
        on_save_chat.call(updated);
    };

    let active_agent = settings.active_agent().cloned();
    let active_agent_name = active_agent
        .as_ref()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| "(no agent)".to_string());
    let dock = settings.dock;
    let floating = dock == ChatDock::Floating;

    // Root style depends on the dock mode: floating panels are `position:
    // fixed` overlays placed at the dragged coords; docked panels are plain
    // flex children of `#main` (rendered inside it by `app.rs`) so they push
    // the terminal content aside — the "merge into main window" behavior.
    // `position: relative` on the docked roots anchors the agent-config
    // popover overlay. Title-bar height is fixed so the popover's `top`
    // offset lines up.
    let root_style = match dock {
        ChatDock::Floating => format!(
            "{skin_style}position:fixed;left:{}px;top:{}px;width:{}px;height:{}px;\
             z-index:200;border:1px solid var(--skin-border-strong);border-radius:8px;\
             box-shadow:0 8px 32px rgba(0,0,0,0.35);",
            position.x, position.y, width, height
        ),
        ChatDock::Right => format!(
            "{skin_style}position:relative;width:{}px;height:100%;flex:none;\
             border-left:1px solid var(--skin-border-strong);",
            width
        ),
        ChatDock::Bottom => format!(
            "{skin_style}position:relative;width:100%;height:{}px;flex:none;\
             border-top:1px solid var(--skin-border-strong);",
            height
        ),
    };
    let title_cursor = if floating { "grab" } else { "default" };

    rsx! {
        div {
            id: "rusterm-chat-panel",
            style: "{root_style}
                background: var(--skin-surface);
                display: flex;
                flex-direction: column;
                color: var(--skin-text);
                overflow: hidden;
                font-size: 12px;
            ",

            // ── Title bar (drag handle in floating mode) ──────────────────
            div {
                style: "
                    display: flex;
                    align-items: center;
                    justify-content: space-between;
                    height: {CHAT_TITLE_HEIGHT}px;
                    box-sizing: border-box;
                    padding: 0 10px;
                    background: var(--skin-bg);
                    border-bottom: 1px solid var(--skin-border);
                    cursor: {title_cursor};
                    user-select: none;
                    -webkit-user-select: none;
                ",
                onmousedown: start_drag,
                div { style: "display:flex;align-items:center;gap:8px;min-width:0;",
                    span {
                        style: "font-weight:600;font-size:12px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;",
                        { crate::i18n::t("chat.title") }
                    }
                    span {
                        style: "font-size:10px;color:var(--skin-text-muted);white-space:nowrap;",
                        "· {active_agent_name}"
                    }
                }
                div { style: "display:flex;align-items:center;gap:6px;",
                    // Merge-into-main-window toggle: cycles floating → right
                    // dock → bottom dock. Persisted via on_save_chat, so the
                    // layout survives restarts like every other chat setting.
                    button {
                        style: agent_button_style(),
                        title: crate::i18n::t("chat.dock_tooltip"),
                        onclick: move |e| {
                            e.stop_propagation();
                            let mut s = state.write();
                            s.chat_settings.dock = s.chat_settings.dock.next();
                            let updated = s.chat_settings.clone();
                            drop(s);
                            on_save_chat.call(updated);
                        },
                        if floating { "⊞" } else { "⧉" }
                    }
                    button {
                        style: agent_button_style(),
                        title: "Configure agent",
                        onclick: move |e| {
                            e.stop_propagation();
                            let opening = !*show_agent_config.peek();
                            if opening {
                                // Seed the draft fields from the active agent
                                // so the popover opens with current values.
                                let a = state.read().chat_settings.active_agent().cloned();
                                if let Some(a) = a {
                                    draft_name.set(a.name);
                                    draft_model.set(a.model);
                                    draft_base_url.set(a.base_url);
                                    draft_prompt.set(a.system_prompt);
                                }
                                draft_api_key.set(String::new());
                            }
                            show_agent_config.set(opening);
                        },
                        "⚙"
                    }
                    button {
                        style: agent_button_style(),
                        title: "Close (Tab/Esc returns to terminal)",
                        onclick: move |e| {
                            e.stop_propagation();
                            on_close(());
                        },
                        "×"
                    }
                }
            }

            // ── Agent selector ────────────────────────────────────────────
            div {
                style: "padding:6px 10px;border-bottom:1px solid var(--skin-border);background:var(--skin-bg);",
                select {
                    style: "width:100%;background:var(--skin-surface);color:var(--skin-text);border:1px solid var(--skin-border);border-radius:4px;padding:3px 6px;font-size:11px;",
                    value: "{settings.active_agent_id.clone().unwrap_or_default()}",
                    onchange: move |e| {
                        let id = e.value();
                        let mut s = state.write();
                        s.chat_settings.active_agent_id = Some(id);
                        let updated = s.chat_settings.clone();
                        drop(s);
                        on_save_chat.call(updated);
                    },
                    for agent in &settings.agents {
                        option {
                            value: "{agent.id}",
                            selected: settings.active_agent_id.as_deref() == Some(agent.id.as_str()),
                            "{agent.name} ({provider_label(agent.provider)})"
                        }
                    }
                }
            }

            // ── Agent config popover ──────────────────────────────────────
            // Rendered as an overlay anchored below the title bar (not inline
            // in the selector row). The panel's fixed height + overflow:hidden
            // used to clip the popover so the 保存 button was unreachable;
            // the overlay owns a scroll region so the button is always
            // clickable regardless of panel height.
            if *show_agent_config.peek() {
                div {
                    style: "position:absolute;top:{CHAT_TITLE_HEIGHT}px;left:0;right:0;bottom:0;z-index:20;background:var(--skin-surface);overflow-y:auto;",
                    { render_agent_config(state.clone(), active_agent.clone(), draft_name, draft_model, draft_base_url, draft_prompt, draft_api_key, show_agent_config, on_save_chat.clone()) }
                }
            }

            // ── Message log ───────────────────────────────────────────────
            div {
                style: "flex:1;overflow-y:auto;padding:8px 10px;display:flex;flex-direction:column;gap:6px;background:var(--skin-surface);",
                for msg in &messages {
                    { render_message(msg) }
                }
                if messages.is_empty() {
                    div {
                        style: "color:var(--skin-text-muted);text-align:center;padding:24px 8px;line-height:1.6;",
                        { crate::i18n::t("chat.empty") }
                    }
                }
            }

            // ── Command palette dropdown ──────────────────────────────────
            if command_mode && !command_results.is_empty() {
                div {
                    style: "max-height:140px;overflow-y:auto;border-top:1px solid var(--skin-border);background:var(--skin-bg);",
                    // Clone each entry into an owned value before the closure —
                    // `onmousedown` captures by move and needs `'static`.
                    for (idx, entry) in command_results.iter().enumerate() {{
                        let cmd_owned = entry.command.clone();
                        let src_label = source_label(entry.source);
                        rsx! {
                            div {
                                key: "{entry.command}-{idx}",
                                style: format!(
                                    "padding:5px 10px;cursor:pointer;font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;{}",
                                    if idx == command_selected {
                                        "background:var(--skin-accent);color:var(--skin-bg);".to_string()
                                    } else {
                                        String::new()
                                    }
                                ),
                                onmousedown: move |e| {
                                    e.prevent_default();
                                    run_command_in_terminal(state, &cmd_owned);
                                    let mut s = state.write();
                                    s.chat_input.clear();
                                    s.chat_command_mode = false;
                                    s.chat_command_results.clear();
                                    s.chat_command_selected = 0;
                                    on_focus_terminal.call(());
                                },
                                span {
                                    style: "font-family:'JetBrains Mono',monospace;",
                                    "{cmd_owned}"
                                }
                                span {
                                    style: "margin-left:8px;font-size:9px;color:var(--skin-text-muted);",
                                    "{src_label}"
                                }
                            }
                        }
                    }}
                }
            }

            // ── Input row ─────────────────────────────────────────────────
            div {
                style: "padding:8px 10px;border-top:1px solid var(--skin-border);background:var(--skin-bg);",
                textarea {
                    id: "rusterm-chat-input",
                    style: "width:100%;height:54px;resize:none;background:var(--skin-surface);color:var(--skin-text);border:1px solid var(--skin-border);border-radius:4px;padding:6px 8px;font-size:12px;font-family:inherit;box-sizing:border-box;",
                    placeholder: "{placeholder_text}",
                    value: "{input_text}",
                    oninput: on_input_change,
                    onkeydown: on_input_keydown,
                }
                div { style: "display:flex;align-items:center;justify-content:space-between;margin-top:6px;",
                    span {
                        style: "font-size:10px;color:var(--skin-text-muted);",
                        "{status_text}"
                    }
                    button {
                        style: "background:var(--skin-accent);color:var(--skin-bg);border:0;border-radius:4px;padding:4px 12px;font-size:11px;font-weight:600;cursor:pointer;",
                        onclick: on_send_click,
                        "{send_label}"
                    }
                }
            }
        }
    }
}

/// Render a single chat bubble. User messages align right, assistant left.
fn render_message(msg: &ChatMessage) -> Element {
    let (align, bubble_bg, bubble_color) = match msg.role {
        ChatRole::User => ("flex-end", "var(--skin-accent)", "var(--skin-bg)"),
        ChatRole::Assistant => ("flex-start", "var(--skin-bg)", "var(--skin-text)"),
        ChatRole::System => ("center", "transparent", "var(--skin-text-muted)"),
    };
    rsx! {
        div {
            style: "align-self:{align};max-width:85%;",
            div {
                style: format!(
                    "padding:6px 10px;border-radius:8px;font-size:12px;line-height:1.5;white-space:pre-wrap;word-break:break-word;background:{};color:{};{}",
                    bubble_bg,
                    bubble_color,
                    if msg.role == ChatRole::System {
                        "font-style:italic;".to_string()
                    } else {
                        String::new()
                    }
                ),
                "{msg.content}"
            }
        }
    }
}

/// Inline agent configuration popover. Lets the user edit the active agent's
/// model / base URL / system prompt and paste an API key (held in memory
/// only — see module docs on the secret policy).
///
/// The draft signals are lifted into the parent `ChatPanel` (Dioxus hooks
/// can't run inside a plain helper fn) and passed in here.
fn render_agent_config(
    mut state: Signal<AppState>,
    agent: Option<AgentConfig>,
    mut draft_name: Signal<String>,
    mut draft_model: Signal<String>,
    mut draft_base_url: Signal<String>,
    mut draft_prompt: Signal<String>,
    mut draft_api_key: Signal<String>,
    mut show_agent_config: Signal<bool>,
    on_save_chat: EventHandler<ChatSettings>,
) -> Element {
    let agent = match agent {
        Some(a) => a,
        None => {
            return rsx! {
                div { style: "padding:8px;color:var(--skin-text-muted);font-size:11px;",
                    { crate::i18n::t("chat.no_agent") }
                }
            };
        }
    };
    let agent_id = agent.id.clone();

    rsx! {
        div {
            style: "padding:8px;display:flex;flex-direction:column;gap:6px;background:var(--skin-surface);",
            label { style: label_style(), { crate::i18n::t("chat.agent_name") }
                input {
                    style: input_style(),
                    value: "{draft_name}",
                    oninput: move |e| draft_name.set(e.value()),
                }
            }
            label { style: label_style(), { crate::i18n::t("chat.agent_model") }
                input {
                    style: input_style(),
                    value: "{draft_model}",
                    oninput: move |e| draft_model.set(e.value()),
                }
            }
            label { style: label_style(), { crate::i18n::t("chat.agent_base_url") }
                input {
                    style: input_style(),
                    placeholder: "(default)",
                    value: "{draft_base_url}",
                    oninput: move |e| draft_base_url.set(e.value()),
                }
            }
            label { style: label_style(), { crate::i18n::t("chat.agent_api_key") }
                input {
                    style: input_style(),
                    r#type: "password",
                    placeholder: "(in-memory only)",
                    value: "{draft_api_key}",
                    oninput: move |e| draft_api_key.set(e.value()),
                }
            }
            label { style: label_style(), { crate::i18n::t("chat.agent_system_prompt") }
                textarea {
                    style: "{input_style()}height:48px;resize:none;",
                    value: "{draft_prompt}",
                    oninput: move |e| draft_prompt.set(e.value()),
                }
            }
            div { style: "display:flex;gap:6px;justify-content:flex-end;",
                button {
                    style: "background:var(--skin-accent);color:var(--skin-bg);border:0;border-radius:4px;padding:4px 12px;font-size:11px;cursor:pointer;",
                    onclick: move |_| {
                        // Saving must produce an immediate, visible change:
                        // apply the draft, close the overlay (so the header
                        // summary row showing the new values is revealed) and
                        // surface a status message. Without this the click
                        // felt like a no-op because the overlay covered the
                        // status row at the bottom of the panel.
                        let mut s = state.write();
                        let mut saved_name = String::new();
                        if let Some(a) = s.chat_settings.agents.iter_mut().find(|a| a.id == agent_id) {
                            a.name = draft_name();
                            a.model = draft_model();
                            a.base_url = draft_base_url();
                            a.system_prompt = draft_prompt();
                            saved_name = a.name.clone();
                        }
                        let mut feedback = format!("{} · {}", crate::i18n::t("chat.saved"), saved_name);
                        // API key held in memory only (TODO: keychain).
                        if !draft_api_key().is_empty() {
                            feedback.push_str(&format!(" ({})", crate::i18n::t("chat.api_key_in_memory")));
                        }
                        s.chat_status = Some(feedback);
                        let updated = s.chat_settings.clone();
                        drop(s);
                        show_agent_config.set(false);
                        tracing::info!(target: "rusterm.chat", "agent config saved: {saved_name}");
                        on_save_chat.call(updated);
                    },
                    { crate::i18n::t("chat.save") }
                }
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn panel_size(settings: &ChatSettings) -> (f64, f64) {
    let w = if settings.width > 0.0 {
        settings.width
    } else {
        DEFAULT_CHAT_WIDTH
    };
    let h = if settings.height > 0.0 {
        settings.height
    } else {
        DEFAULT_CHAT_HEIGHT
    };
    (w, h)
}

/// Live window inner size in logical px (the panel's CSS uses logical px).
/// Mirrors the window-state persistence code in app.rs.
/// `dioxus::desktop::window()` derefs to tao::Window, available synchronously
/// on desktop.
fn window_logical_size() -> (f64, f64) {
    let desktop = dioxus::desktop::window();
    let logical = desktop
        .inner_size()
        .to_logical::<f64>(desktop.scale_factor());
    (logical.width, logical.height)
}

/// Clamp a panel position so the whole panel stays inside a `win_w × win_h`
/// window. Prevents the panel from being dragged (or restored) off-screen.
fn clamp_position(p: ChatPosition, w: f64, h: f64, win_w: f64, win_h: f64) -> ChatPosition {
    ChatPosition {
        x: p.x.clamp(0.0, (win_w - w).max(0.0)),
        y: p.y.clamp(0.0, (win_h - h).max(0.0)),
    }
}

/// Resolve the panel position: use the persisted drag position if set,
/// otherwise anchor to the bottom-left of the window. The bottom-left
/// anchor is computed from the live window's inner height so it actually
/// sits at the bottom regardless of window size. Once the user drags, the
/// persisted `position` takes over. A persisted position is clamped back
/// into the window (the window may have shrunk since it was saved).
fn resolved_position(settings: &ChatSettings, w: f64, h: f64) -> ChatPosition {
    if settings.position.is_set() {
        let (win_w, win_h) = window_logical_size();
        clamp_position(settings.position, w, h, win_w, win_h)
    } else {
        let (_win_w, win_h) = window_logical_size();
        ChatPosition {
            x: DEFAULT_EDGE_PADDING,
            y: (win_h - h - DEFAULT_EDGE_PADDING).max(DEFAULT_EDGE_PADDING),
        }
    }
}

/// Built-in app commands for the palette, filtered by a substring query.
/// Synchronous so the dropdown shows *something* immediately while the
/// DB-backed history query is in flight.
fn builtin_commands(query: &str) -> Vec<ChatCommandEntry> {
    let q = query.trim().to_ascii_lowercase();
    APP_COMMANDS
        .iter()
        .filter(|(cmd, _)| q.is_empty() || cmd.contains(&q))
        .map(|(cmd, _)| ChatCommandEntry {
            command: cmd.to_string(),
            source: ChatCommandSource::AppCommand,
        })
        .collect()
}

/// Async query against the DuckDB-backed command history store. Uses the
/// frecency-ranked `search_history` so frequently-used commands float to the
/// top. Returns `None` if the DB can't be opened (locked / first launch) —
/// the palette then falls back to app commands only.
async fn query_history(query: &str) -> Option<Vec<rusterm_db::history::HistoryEntry>> {
    let path = dirs::data_dir().map(|d| d.join("rusterm").join("rusterm.db"))?;
    let db = rusterm_db::Database::open(Some(path)).await.ok()?;
    let q = if query.trim().is_empty() { "" } else { query };
    db.search_history(q, 30).await.ok()
}

/// Send the current input as a user message to the active agent. The v1
/// implementation appends the user turn to the log and records a placeholder
/// assistant acknowledgement — a real LLM round-trip is wired in once the
/// agent's API key is resolved from the secret store. This keeps the UI fully
/// functional for the command-palette flow (which doesn't need an LLM) while
/// making the chat surface visibly responsive.
fn send_message(mut state: Signal<AppState>) {
    let text = state.read().chat_input.trim().to_string();
    if text.is_empty() {
        return;
    }
    let mut s = state.write();
    s.chat_messages.push(ChatMessage {
        role: ChatRole::User,
        content: text.clone(),
    });
    s.chat_input.clear();
    s.chat_status = Some(crate::i18n::t("chat.thinking").to_string());
    drop(s);

    // Kick off the (currently stubbed) agent turn. We spawn so the UI stays
    // responsive; the future pushes the assistant message when it completes.
    spawn(async move {
        // TODO(issue #122 follow-up): route through rusterm_ai::SuggestionEngine
        // or a new chat-completions client, keyed off the active agent's
        // provider/model and the in-memory API key.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut s = state.write();
        s.chat_messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: crate::i18n::t("chat.stub_reply").to_string(),
        });
        s.chat_status = None;
    });
}

/// Insert a command into the active terminal's input (NOT auto-run — the user
/// can review it before pressing Enter). This mirrors how the existing
/// suggestion popup hands a chosen command to the terminal.
fn run_command_in_terminal(state: Signal<AppState>, command: &str) {
    let active_sid = crate::state::focused_pane_session(&state.read())
        .or_else(|| state.read().active_session.clone());
    let Some(sid) = active_sid else {
        tracing::warn!("[CHAT] no active session to run command in");
        return;
    };
    // Focus the terminal input and set its value via JS. We use the same
    // element-id convention as `restore_focus_to_active_session`.
    let escaped = command.replace('\\', "\\\\").replace('\'', "\\'");
    let script = format!(
        "(function(){{\
           var el=document.getElementById('terminal-input-{sid}');\
           if(!el){{return;}}\
           el.focus();\
           var setter=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value')\
             ||Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value');\
           if(setter&&setter.set){{setter.set.call(el,'{escaped}');}}\
           else{{el.value='{escaped}';}}\
           el.dispatchEvent(new Event('input',{{bubbles:true}}));\
         }})()",
        sid = sid,
        escaped = escaped,
    );
    spawn(async move {
        let _ = dioxus::document::eval(&script).await;
    });
}

/// Poll the JS-side drag globals written by the document-capture listeners
/// installed in `start_drag`. Returns `(x, y, done)`, or `None` if the globals
/// aren't set yet (listener install still in flight).
async fn poll_chat_drag() -> Option<(f64, f64, bool)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var pos = window.__rusterm_chat_drag_pos || '';\n\
            if (!pos) return '';\n\
            var done = window.__rusterm_chat_drag_done ? '1' : '0';\n\
            return pos + '|' + done;\n\
        })()",
    )
    .await
    .ok()?;
    let raw = result.as_str()?;
    if raw.is_empty() {
        return None;
    }
    let (pos, done) = raw.split_once('|')?;
    let (x_str, y_str) = pos.split_once(',')?;
    let x: f64 = x_str.parse().ok()?;
    let y: f64 = y_str.parse().ok()?;
    let done = done == "1";
    Some((x, y, done))
}

fn provider_label(p: ChatAgentProvider) -> &'static str {
    match p {
        ChatAgentProvider::OpenAI => "OpenAI",
        ChatAgentProvider::Anthropic => "Anthropic",
        ChatAgentProvider::Local => "Local",
    }
}

fn source_label(s: ChatCommandSource) -> &'static str {
    match s {
        ChatCommandSource::History => "history",
        ChatCommandSource::AppCommand => "app",
    }
}

fn agent_button_style() -> &'static str {
    "background:none;border:none;color:var(--skin-text-muted);cursor:pointer;font-size:14px;padding:0 2px;line-height:1;"
}

fn label_style() -> &'static str {
    "display:flex;flex-direction:column;gap:2px;font-size:10px;color:var(--skin-text-muted);"
}

fn input_style() -> &'static str {
    "background:var(--skin-bg);color:var(--skin-text);border:1px solid var(--skin-border);border-radius:3px;padding:3px 6px;font-size:11px;font-family:inherit;box-sizing:border-box;"
}
