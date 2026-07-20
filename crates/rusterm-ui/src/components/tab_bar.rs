use dioxus::prelude::*;

use crate::state::{SessionTab, WorkspaceTab};
use rusterm_core::FocusedTabAppearance;
use rusterm_core::session::SessionType;

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
#[component]
pub fn TabBar(
    tabs: Vec<WorkspaceTab>,
    sessions: Vec<SessionTab>,
    active: Option<String>,
    focused_session: Option<String>,
    focused_appearance: FocusedTabAppearance,
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
) -> Element {
    let mut hover_tab = use_signal(|| None::<String>);
    let focused_appearance = focused_appearance.normalized();

    rsx! {
        div {
            style: "
                display: flex;
                background: #1a1b26;
                border-bottom: 1px solid #2a2b3d;
                height: 36px;
                align-items: stretch;
                overflow-x: auto;
            ",

            for tab in tabs {
                {
                    let (session_id, session_name, kind) = resolve_tab_display(&tab, &sessions);
                    let is_active = active.as_ref() == Some(&tab.id);
                    let is_pane_focused = focused_session.as_ref() == Some(&session_id);
                    let is_hover = hover_tab() == Some(tab.id.clone());
                    let color = session_type_color(&kind);
                    let _label = session_type_label(&kind);
                    let bg = if is_active { "#24283b" } else if is_hover { "#1f2335" } else { "transparent" };
                    let border_bottom = if is_active { format!("2px solid {color}") } else { "2px solid transparent".to_string() };
                    let (pane_focus_shadow, pane_focus_radius) =
                        focused_tab_chrome(is_pane_focused, &focused_appearance);
                    let tab_id = tab.id.clone();
                    let tab_id2 = tab.id.clone();

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

                    rsx! {
                        div {
                            key: "{tab.id}",
                            style: "
                                display: flex;
                                align-items: center;
                                padding: 0 12px;
                                cursor: pointer;
                                font-size: 12px;
                                color: #c0caf5;
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

                            // Type indicator dot
                            span {
                                style: "width: 6px; height: 6px; border-radius: 50%; background: {color}; flex-shrink: 0;",
                            }

                            span {
                                style: "overflow: hidden; text-overflow: ellipsis; max-width: 120px;",
                                "{session_name}"
                            }

                            // Close button (show on hover or active)
                            if is_hover || is_active {
                                button {
                                    style: "
                                        background: none;
                                        border: none;
                                        color: #565f89;
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            suggestion_selected: 0,
            suggestion_visible: false,
            command_history: Vec::new(),
            hostname: None,
            cwd: None,
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
}
