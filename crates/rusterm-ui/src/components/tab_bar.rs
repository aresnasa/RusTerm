use dioxus::prelude::*;

use crate::state::SessionTab;
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

#[component]
pub fn TabBar(
    tabs: Vec<SessionTab>,
    active: Option<String>,
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
    on_drag_start: EventHandler<(String, String, f64, f64)>,
) -> Element {
    let mut hover_tab = use_signal(|| None::<String>);

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
                    let is_active = active.as_ref() == Some(&tab.id);
                    let is_hover = hover_tab() == Some(tab.id.clone());
                    let color = session_type_color(&tab.kind);
                    let _label = session_type_label(&tab.kind);
                    let bg = if is_active { "#24283b" } else if is_hover { "#1f2335" } else { "transparent" };
                    let border_bottom = if is_active { format!("2px solid {color}") } else { "2px solid transparent".to_string() };
                    let tab_id = tab.id.clone();
                    let tab_id2 = tab.id.clone();

                    // Clone the tab id + name for the mousedown handler.
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
                    let tab_name_for_drag = tab.name.clone();

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
                                white-space: nowrap;
                                gap: 6px;
                                position: relative;
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
                                    let c = e.client_coordinates();
                                    on_drag_start.call((
                                        tab_id_for_drag.clone(),
                                        tab_name_for_drag.clone(),
                                        c.x,
                                        c.y,
                                    ));
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
                                "{tab.name}"
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
