use dioxus::prelude::*;

use rusterm_core::session::SessionType;
use crate::state::SessionTab;

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

                    // Clone the tab id for the dragstart handler. When
                    // the user drags an open session tab, we stash its
                    // session id in the DataTransfer under a custom
                    // MIME type. The pane drop handler reads this to
                    // know which open session to move/swap into the
                    // target pane. We use a different MIME type than
                    // the sidebar's "connection-id" so the drop
                    // handler can distinguish "drag from tab bar"
                    // (move existing session) from "drag from sidebar"
                            // (open a new connection).
                            let tab_id_for_drag = tab.id.clone();

                            rsx! {
                        div {
                            key: "{tab.id}",
                            draggable: true,
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
                            onmouseenter: move |_| hover_tab.set(Some(tab_id2.clone())),
                            onmouseleave: move |_| hover_tab.set(None),
                            ondragstart: move |e: DragEvent| {
                                let dt = e.data_transfer();
                                let _ = dt.set_data(
                                    "application/x-rusterm-session-id",
                                    &tab_id_for_drag,
                                );
                                // "move" indicates that dropping will
                                // move the session from one pane to
                                // another (rather than copying it).
                                dt.set_drop_effect("move");
                                dt.set_effect_allowed("move");
                                tracing::debug!(
                                    "[DRAG] tab drag started: session={:?}",
                                    &tab_id_for_drag[..tab_id_for_drag.len().min(8)]
                                );
                            },

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
