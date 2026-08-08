use dioxus::prelude::*;

use crate::components::CommandStatusBadge;
use crate::state::{CommandStatus, SessionConnectionState, SessionTab, WorkspaceTab};
use rusterm_core::FocusedTabAppearance;
use rusterm_core::session::SessionType;
use std::collections::HashMap;

// `MouseButton` lives in `dioxus::html::input_data` (not re-exported by
// `dioxus::prelude::*`). Used by the tab's `onmousedown` handler to
// filter for primary-button (left-click) drags only.
use dioxus::html::input_data::MouseButton;

fn session_type_color(kind: &SessionType) -> &'static str {
    match kind {
        SessionType::Ssh => "#7aa2f7",
        SessionType::Serial => "#e0af68",
        SessionType::Telnet => "#ff9e64",
        SessionType::Shell => "#9ece6a",
        SessionType::Tcp => "#7dcfff",
    }
}

fn session_type_label(kind: &SessionType) -> &'static str {
    match kind {
        SessionType::Ssh => "SSH",
        SessionType::Serial => "SER",
        SessionType::Telnet => "TEL",
        SessionType::Shell => "SH",
        SessionType::Tcp => "TCP",
    }
}

/// Resolve the indicator-dot colour for a tab from its session's connection
/// state. The colours come from the user-configurable skin palette CSS
/// variables, so they follow the active theme (and can be customised via the
/// Custom skin settings):
///
/// - `Connected`    → `--skin-success` (green)
/// - `Connecting`   → `--skin-accent`  (blue)
/// - `Reconnecting` → `--skin-warning` (amber)
/// - `Failed`       → `--skin-danger`  (red)
/// - `Disconnected` → `--skin-text-muted` (neutral grey)
///
/// `None` (no state entry yet) falls back to `--skin-accent` so a freshly
/// created tab shows the in-progress blue until its driver reports back.
fn connection_state_dot_color(state: Option<SessionConnectionState>) -> &'static str {
    match state {
        Some(SessionConnectionState::Connected) => "var(--skin-success)",
        Some(SessionConnectionState::Connecting) => "var(--skin-accent)",
        Some(SessionConnectionState::Reconnecting) => "var(--skin-warning)",
        Some(SessionConnectionState::Failed) => "var(--skin-danger)",
        Some(SessionConnectionState::Disconnected) => "var(--skin-text-muted)",
        None => "var(--skin-accent)",
    }
}

fn focused_tab_chrome(is_focused: bool, appearance: &FocusedTabAppearance) -> (String, String) {
    if is_focused {
        (
            format!(
                "inset 0 0 0 {}px {}",
                appearance.border_width, appearance.border_color
            ),
            format!("{}px", appearance.border_radius),
        )
    } else {
        ("none".to_string(), "0".to_string())
    }
}

/// Resolve a `WorkspaceTab` to the displayable (id, name, kind) triple of its
/// anchor session. Falls back to the tab's group id and a placeholder kind
/// when the anchor session can't be found (e.g. during teardown).
fn resolve_tab_display<'a>(
    tab: &'a WorkspaceTab,
    sessions: &'a [SessionTab],
) -> (String, &'a str, SessionType) {
    // Default to SSH so the indicator dot has a sensible colour even before
    // the anchor session is located. The real kind overrides it below.
    let mut kind = SessionType::Ssh;
    let mut name: &str = "—";
    let mut display_id = tab.id.clone();

    if let Some(anchor) = &tab.anchor_session_id {
        if let Some(session) = sessions.iter().find(|s| &s.id == anchor) {
            kind = session.kind;
            name = &session.name;
            display_id = session.id.clone();
        } else {
            // Anchor is set but the session is gone — show the anchor id
            // stub so the user can still tell tabs apart during teardown.
            name = "";
            display_id = anchor.clone();
        }
    }

    (display_id, name, kind)
}

/// The top TabBar. Renders one entry per `WorkspaceTab` (Plan B). Pane-only
/// sessions (sidebar drops, pane clones) do NOT appear here — they're shown
/// only inside their host tab's layout.
///
/// `sessions` is passed alongside `tabs` so the bar can resolve each tab's
/// anchor session for its display name + type indicator dot. The bar never
/// shows a session that isn't a tab anchor.
///
/// Right-clicking a tab opens a context menu with Disconnect / Reconnect /
/// Copy Session actions (Task: 会话支持断开和重连 + 复制会话). The actions
/// operate on the tab's ANCHOR session. `connection_states` is consulted to
/// enable/disable Disconnect (only when Connected) and Reconnect (only when
/// Disconnected); Copy Session is always available as long as the session has
/// a stored login config (the App-side handler logs a warning if not).
#[component]
pub fn TabBar(
    tabs: Vec<WorkspaceTab>,
    sessions: Vec<SessionTab>,
    active: Option<String>,
    focused_session: Option<String>,
    focused_appearance: FocusedTabAppearance,
    /// Per-session connection state, used to enable/disable the Disconnect
    /// and Reconnect context-menu items.
    connection_states: HashMap<String, SessionConnectionState>,
    /// Per-session hostname of the jumpserver-internal node the session has
    /// landed on (captured from the target shell's OSC 7 report). Shown as a
    /// muted suffix on the tab title when it differs from the session's own
    /// connection host — so a jumpserver copy reads "ops@jump 副本 1 · web-01"
    /// instead of just the bastion name.
    session_nodes: HashMap<String, String>,
    on_select: EventHandler<String>,
    on_close: EventHandler<String>,
    /// Manual mouse-based tab drag (Task 22). Fired on `mousedown` with
    /// the primary button. The handler receives
    /// `(session_id, session_name, client_x, client_y)` — the parent
    /// (`App`) calls `start_tab_drag` to install the document-level JS
    /// listeners + set the `tab_drag` signal. The polling `use_future`
    /// in `App` takes over from there.
    ///
    /// Replaces the prior HTML5 `draggable: true` / `ondragstart` wiring
    /// (Tasks 17/19), which was unreliable in dioxus 0.7's desktop
    /// webview. Plain click-to-select still works: `onmousedown` sets
    /// `tab_drag` with `dragging: false`; the polling loop only executes
    /// a drop if the cursor crossed the threshold (i.e. it became a
    /// real drag); `onclick` fires normally for non-drag clicks.
    ///
    /// The session id handed to the drag handler is the tab's ANCHOR
    /// session id (the session occupying pane 0). Dragging a tab onto a
    /// pane is therefore semantically "drag the anchor session" —
    /// which matches the legacy behaviour the drop handlers expect.
    on_drag_start: EventHandler<(String, String, f64, f64)>,
    /// Context-menu: disconnect the tab's anchor session. Fired with the
    /// anchor session id. The App-side handler is a no-op if the session
    /// is not currently Connected/Reconnecting.
    on_disconnect: EventHandler<String>,
    /// Context-menu: reconnect the tab's anchor session. Fired with the
    /// anchor session id. The App-side handler is a no-op if the session
    /// is not Disconnected.
    on_reconnect: EventHandler<String>,
    /// Context-menu: copy the tab's anchor session. Fired with the anchor
    /// session id. The App-side handler clones the stored login config and
    /// opens a new independent session via `open_connection`.
    on_copy_session: EventHandler<String>,
) -> Element {
    let mut hover_tab = use_signal(|| None::<String>);
    // `(session_id, client_x, client_y)` for the tab whose context menu is
    // open. `None` when the menu is closed. Mirrors the Sidebar's context-menu
    // signal pattern.
    let mut context_menu = use_signal(|| None::<(String, f64, f64)>);
    let focused_appearance = focused_appearance.normalized();

    // Pre-compute the context-menu enable flags by reading the signals once.
    // dioxus rsx! `{}` formatted segments don't accept `matches!` macro calls,
    // so we derive plain `bool`s here and reference them inside the rsx!.
    let menu_snapshot = context_menu();
    let menu_state = menu_snapshot
        .as_ref()
        .and_then(|(sid, _, _)| connection_states.get(sid).copied())
        .unwrap_or(SessionConnectionState::Connected);
    let can_disconnect = matches!(
        menu_state,
        SessionConnectionState::Connected | SessionConnectionState::Reconnecting
    );
    let can_reconnect = matches!(
        menu_state,
        SessionConnectionState::Disconnected | SessionConnectionState::Failed
    );
    let disconnect_style = if can_disconnect {
        "cursor:pointer;".to_string()
    } else {
        "opacity:0.4;cursor:default;".to_string()
    };
    let reconnect_style = if can_reconnect {
        "cursor:pointer;".to_string()
    } else {
        "opacity:0.4;cursor:default;".to_string()
    };
    // Pre-clone the menu session id for each onclick closure. dioxus `move`
    // closures take ownership of captured variables, so a single `menu_sid`
    // can't be shared across three closures — each gets its own clone.
    let menu_sid_disconnect = menu_snapshot.as_ref().map(|(sid, _, _)| sid.clone());
    let menu_sid_reconnect = menu_snapshot.as_ref().map(|(sid, _, _)| sid.clone());
    let menu_sid_copy = menu_snapshot.as_ref().map(|(sid, _, _)| sid.clone());

    rsx! {
        div {
            // v0.22: the bar no longer has a fixed 36px height — it grows
            // taller when a tab title wraps onto two lines (long jumpserver
            // node hostnames such as "x-prod-k8s-master-0001.host.example"
            // used to make one tab overdren long / get ellipsis-clipped).
            // `min-height` keeps the compact 36px look when nothing wraps.
            style: "
                display: flex;
                background: var(--skin-bg);
                border-bottom: 1px solid var(--skin-border);
                min-height: 36px;
                align-items: stretch;
                overflow-x: auto;
            ",

            for (tab_index, tab) in tabs.into_iter().enumerate() {
                {
                    let (session_id, session_name, kind) = resolve_tab_display(&tab, &sessions);
                    let is_active = active.as_ref() == Some(&tab.id);
                    let is_pane_focused = focused_session.as_ref() == Some(&session_id);
                    let is_hover = hover_tab() == Some(tab.id.clone());
                    // v0.21 session header: when this session has landed on a
                    // jumpserver-internal node (OSC 7 host) that's distinct from
                    // its own connection host, surface the node as a muted suffix
                    // so a jumpserver copy reads "ops@jump 副本 1 · web-01". We
                    // suppress the suffix when the node matches (or is a
                    // substring of) the connection host, so plain SSH / local
                    // shells don't show a redundant label.
                    let node_label: Option<String> = {
                        let node = session_nodes.get(&session_id);
                        let host = sessions
                            .iter()
                            .find(|s| s.id == session_id)
                            .and_then(|s| s.hostname.as_deref());
                        node.and_then(|n| {
                            let n = n.trim();
                            if n.is_empty() {
                                return None;
                            }
                            let same_as_host = match host {
                                Some(h) => {
                                    let h = h.trim();
                                    h == n
                                        || h.contains(n)
                                        || n.contains(h)
                                        || n.eq_ignore_ascii_case(h)
                                }
                                None => false,
                            };
                            if same_as_host { None } else { Some(n.to_string()) }
                        })
                    };
                    let color = session_type_color(&kind);
                    let _label = session_type_label(&kind);
                    // The indicator dot reflects the session's *connection*
                    // state (blue→green/red), not its type. Type colour is
                    // still used for the active-tab underline below.
                    let conn_state = connection_states.get(&session_id).copied();
                    let dot_color = connection_state_dot_color(conn_state);
                    let command_status = sessions
                        .iter()
                        .find(|session| session.id == session_id)
                        .map(|session| session.last_command_status.clone())
                        .unwrap_or(CommandStatus::Idle);
                    let bg = if is_active { "var(--skin-surface)" } else if is_hover { "var(--skin-surface-hover)" } else { "transparent" };
                    let border_bottom = if is_active { format!("2px solid {color}") } else { "2px solid transparent".to_string() };
                    let (pane_focus_shadow, pane_focus_radius) =
                        focused_tab_chrome(is_pane_focused, &focused_appearance);
                    let tab_id = tab.id.clone();
                    let tab_id2 = tab.id.clone();
                    // 1-based positional tab number. Reflects the current
                    // order in `state.tabs` (so it updates after a reorder).
                    // Shown in a muted colour so it reads as a quiet index,
                    // not a primary label — the session name is still the
                    // main identifier. The number makes it easier to refer
                    // to tabs ("tab 3 disconnected") and disambiguates
                    // same-named sessions.
                    let tab_number = tab_index + 1;

                    // Clone the session id + name for the mousedown handler.
                    // When the user presses the primary mouse button on
                    // a tab, we hand off (session_id, session_name, x, y)
                    // to the parent (`App`), which calls `start_tab_drag`
                    // to set the `tab_drag` signal and install the
                    // document-level JS listeners. The polling
                    // `use_future` in `App` takes over from there.
                    //
                    // We do NOT set `draggable: true` — that would start
                    // a native HTML5 drag alongside the manual system,
                    // producing two ghosts and double-executing drops.
                    let tab_id_for_drag = tab.id.clone();
                    let session_id_for_drag = session_id.clone();
                    let session_name_for_drag = session_name.to_string();
                    // Clone for the right-click context menu handler.
                    let session_id_for_ctx = session_id.clone();

                    rsx! {
                        div {
                            key: "{tab.id}",
                            // `data-rusterm-tab-id` lets the tab-drag JS
                            // hit-test (`document.elementFromPoint` →
                            // `closest('[data-rusterm-tab-id]')`) detect when
                            // the cursor is over a tab in the top bar. This
                            // drives the drag-to-reorder gesture: dropping a
                            // session-drag onto another tab reorders the top
                            // tab bar instead of splitting a pane.
                            "data-rusterm-tab-id": "{tab.id}",
                            style: "
                                display: flex;
                                align-items: center;
                                padding: 0 12px;
                                cursor: pointer;
                                font-size: 12px;
                                color: var(--skin-text);
                                background: {bg};
                                border-bottom: {border_bottom};
                                box-shadow: {pane_focus_shadow};
                                border-radius: {pane_focus_radius};
                                white-space: nowrap;
                                gap: 6px;
                                position: relative;
                                user-select: none;
                                -webkit-user-select: none;
                            ",
                            onclick: move |_| {
                                on_select.call(tab.id.clone());
                            },
                            onmousedown: move |e: MouseEvent| {
                                // Only start a drag on primary button
                                // (left click). Middle/right clicks have
                                // other semantics (middle-click close,
                                // right-click context menu) and shouldn't
                                // initiate a drag.
                                if e.trigger_button() == Some(MouseButton::Primary) {
                                    // Prevent the browser from starting a
                                    // native text-selection drag on this
                                    // mousedown (the root cause of "page
                                    // text gets blue-highlighted while
                                    // dragging a tab"). preventDefault on
                                    // mousedown does NOT cancel the
                                    // subsequent click event, so
                                    // click-to-select still works.
                                    e.prevent_default();
                                    let c = e.client_coordinates();
                                    on_drag_start.call((
                                        session_id_for_drag.clone(),
                                        session_name_for_drag.clone(),
                                        c.x,
                                        c.y,
                                    ));
                                    // Suppress the unused-variable warning
                                    // for `tab_id_for_drag` — kept for
                                    // future "drag the tab itself (not
                                    // the session)" features.
                                    let _ = &tab_id_for_drag;
                                }
                            },
                            onmouseenter: move |_| hover_tab.set(Some(tab_id2.clone())),
                            onmouseleave: move |_| hover_tab.set(None),
                            // Right-click opens the Disconnect / Reconnect /
                            // Copy Session context menu. `prevent_default`
                            // suppresses the native browser context menu so
                            // only ours shows.
                            oncontextmenu: move |e: MouseEvent| {
                                e.prevent_default();
                                let c = e.client_coordinates();
                                context_menu.set(Some((
                                    session_id_for_ctx.clone(),
                                    c.x,
                                    c.y,
                                )));
                            },

                            // Positional tab number (1-based). Shown muted
                            // so the session name remains the primary label.
                            // Reflects current tab order — updates after a
                            // reorder.
                            span {
                                style: "color: var(--skin-text-muted); font-variant-numeric: tabular-nums; flex-shrink: 0; min-width: 12px; text-align: right;",
                                "{tab_number}"
                            }

                            // Connection-state indicator dot. Colour follows
                            // the connection lifecycle (blue while connecting,
                            // green once connected, red on failure) and is
                            // driven by the user-configurable skin palette.
                            span {
                                style: "width: 6px; height: 6px; border-radius: 50%; background: {dot_color}; flex-shrink: 0;",
                            }

                            // v0.25: the session name and the node hostname
                            // no longer sit side-by-side — the hostname is
                            // stacked UNDER the name (its own second row),
                            // so the tab is taller but much narrower than the
                            // v0.22/v0.24 side-by-side wrapping layout. Each
                            // row ellipsizes on a single line; the column
                            // caps at 220px total (220+400px side-by-side
                            // needed ~740px for the same content).
                            div {
                                style: "
                                    display: flex;
                                    flex-direction: column;
                                    justify-content: center;
                                    gap: 1px;
                                    max-width: 220px;
                                    min-width: 0;
                                ",
                                span {
                                    style: "
                                        overflow: hidden;
                                        text-overflow: ellipsis;
                                        white-space: nowrap;
                                        line-height: 1.2;
                                    ",
                                    "{session_name}"
                                }
                                // v0.21: jumpserver-internal node suffix (e.g.
                                // "web-01"), shown only when the node differs
                                // from the connection host. Lets the user tell
                                // which internal machine each jumpserver copy
                                // landed on.
                                // v0.25: rendered on its own row beneath the
                                // name; single-line ellipsis, narrower tab.
                                if let Some(node) = &node_label {
                                    span {
                                        style: "
                                            color: var(--skin-text-muted);
                                            font-size: 11px;
                                            overflow: hidden;
                                            text-overflow: ellipsis;
                                            white-space: nowrap;
                                            line-height: 1.2;
                                        ",
                                        "{node}"
                                    }
                                }
                            }

                            // Task #65: status belongs to the session's real top
                            // bar, not to the terminal output surface. A live
                            // Connected session with no command result yet still
                            // shows a green "已连接" badge (see CommandStatusBadge).
                            CommandStatusBadge {
                                status: command_status,
                                connected: conn_state == Some(SessionConnectionState::Connected),
                            }

                            // Close button (show on hover or active)
                            if is_hover || is_active {
                                button {
                                    style: "
                                        background: none;
                                        border: none;
                                        color: var(--skin-text-muted);
                                        cursor: pointer;
                                        font-size: 12px;
                                        padding: 2px 4px;
                                        border-radius: 3px;
                                        line-height: 1;
                                        flex-shrink: 0;
                                    ",
                                    // Stop propagation on mousedown so the
                                    // tab's `onmousedown` (which starts a
                                    // drag) doesn't fire when the user is
                                    // trying to close the tab.
                                    onmousedown: move |e: MouseEvent| {
                                        e.stop_propagation();
                                    },
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        on_close.call(tab_id.clone());
                                    },
                                    "x"
                                }
                            }
                        }
                    }
                }
            }

            // ── Right-click context menu (Disconnect / Reconnect / Copy Session) ──
            // Mirrors the Sidebar's context-menu pattern: a full-viewport
            // click-away backdrop + a fixed-position menu. The menu reads the
            // anchor session id from the `context_menu` signal, so it doesn't
            // need per-tab clones.
            if let Some((_menu_sid, menu_x, menu_y)) = &menu_snapshot {
                // Click-away backdrop.
                div {
                    style: "position:fixed;inset:0;z-index:2999;",
                    onclick: move |_| context_menu.set(None),
                }
                div {
                    style: "position:fixed;top:{menu_y}px;left:{menu_x}px;z-index:3000;background:var(--skin-surface);border:1px solid var(--skin-border-strong);border-radius:5px;padding:4px 0;min-width:170px;box-shadow:0 6px 18px rgba(0,0,0,.5);",
                    // Disconnect (enabled only when Connected/Reconnecting).
                    div {
                        style: "padding:6px 12px;font-size:12px;display:flex;align-items:center;gap:8px;white-space:nowrap;color:var(--skin-text);{disconnect_style}",
                        onclick: move |_| {
                            if let Some(sid) = &menu_sid_disconnect {
                                if can_disconnect {
                                    on_disconnect.call(sid.clone());
                                }
                            }
                            context_menu.set(None);
                        },
                        "⏏  {crate::i18n::t(\"session.disconnect\")}"
                    }
                    // Reconnect (enabled only when Disconnected).
                    div {
                        style: "padding:6px 12px;font-size:12px;display:flex;align-items:center;gap:8px;white-space:nowrap;color:var(--skin-text);{reconnect_style}",
                        onclick: move |_| {
                            if let Some(sid) = &menu_sid_reconnect {
                                if can_reconnect {
                                    on_reconnect.call(sid.clone());
                                }
                            }
                            context_menu.set(None);
                        },
                        "⟳  {crate::i18n::t(\"session.reconnect\")}"
                    }
                    // Separator.
                    div { style: "height:1px;background:var(--skin-border-strong);margin:4px 0;" }
                    // Copy Session (always available; App-side handler warns
                    // if there's no stored config).
                    div {
                        style: "padding:6px 12px;font-size:12px;cursor:pointer;color:var(--skin-text);display:flex;align-items:center;gap:8px;white-space:nowrap;",
                        onclick: move |_| {
                            if let Some(sid) = &menu_sid_copy {
                                on_copy_session.call(sid.clone());
                            }
                            context_menu.set(None);
                        },
                        "⧉  {crate::i18n::t(\"session.copy_session\")}"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CommandStatus;

    #[test]
    fn focused_tab_uses_complete_inset_outline() {
        let appearance = FocusedTabAppearance {
            border_color: "#d5c8ff".to_string(),
            border_width: 2,
            border_radius: 6,
        };

        let (shadow, radius) = focused_tab_chrome(true, &appearance);

        assert_eq!(shadow, "inset 0 0 0 2px #d5c8ff");
        assert_eq!(radius, "6px");
    }

    #[test]
    fn unfocused_tab_has_no_outline() {
        let (shadow, radius) = focused_tab_chrome(false, &FocusedTabAppearance::default());

        assert_eq!(shadow, "none");
        assert_eq!(radius, "0");
    }

    fn session(id: &str, name: &str, kind: SessionType) -> SessionTab {
        SessionTab {
            id: id.to_string(),
            name: name.to_string(),
            kind,
            render_output: Default::default(),
            version: 0,
            suggestion: None,
            suggestions: Vec::new(),
            suggestion_corrections: std::collections::HashSet::new(),
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: None,
            cwd: None,
            last_command_status: CommandStatus::default(),
        }
    }

    #[test]
    fn resolve_tab_display_uses_anchor_session_name_and_kind() {
        let sessions = vec![
            session("sess-1", "alpha-host", SessionType::Ssh),
            session("sess-2", "local", SessionType::Shell),
        ];
        let tab = WorkspaceTab {
            id: "tab-1".to_string(),
            anchor_session_id: Some("sess-1".to_string()),
        };

        let (id, name, kind) = resolve_tab_display(&tab, &sessions);

        assert_eq!(id, "sess-1");
        assert_eq!(name, "alpha-host");
        assert_eq!(kind, SessionType::Ssh);
    }

    #[test]
    fn resolve_tab_display_falls_back_when_anchor_missing() {
        let sessions: Vec<SessionTab> = Vec::new();
        let tab = WorkspaceTab {
            id: "tab-1".to_string(),
            anchor_session_id: Some("sess-gone".to_string()),
        };

        let (id, _name, kind) = resolve_tab_display(&tab, &sessions);

        // Falls back to the anchor id stub and default SSH kind.
        assert_eq!(id, "sess-gone");
        assert_eq!(kind, SessionType::Ssh);
    }

    /// The indicator dot must reflect the connection lifecycle:
    /// blue while connecting, green once connected, red on failure. These
    /// pin the mapping so a future refactor can't silently swap colours.
    #[test]
    fn connection_state_dot_color_maps_lifecycle_colours() {
        use crate::state::SessionConnectionState;

        assert_eq!(
            connection_state_dot_color(Some(SessionConnectionState::Connected)),
            "var(--skin-success)"
        );
        assert_eq!(
            connection_state_dot_color(Some(SessionConnectionState::Connecting)),
            "var(--skin-accent)"
        );
        assert_eq!(
            connection_state_dot_color(Some(SessionConnectionState::Reconnecting)),
            "var(--skin-warning)"
        );
        assert_eq!(
            connection_state_dot_color(Some(SessionConnectionState::Failed)),
            "var(--skin-danger)"
        );
        assert_eq!(
            connection_state_dot_color(Some(SessionConnectionState::Disconnected)),
            "var(--skin-text-muted)"
        );
        // No state entry yet (brand-new tab) shows the in-progress blue.
        assert_eq!(connection_state_dot_color(None), "var(--skin-accent)");
    }
}
