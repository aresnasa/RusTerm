use dioxus::prelude::*;

use rusterm_core::config::{ConnectionConfig, ConnectionKind};

fn kind_label(kind: &ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Ssh(_) => "SSH",
        ConnectionKind::Serial(_) => "Serial",
        ConnectionKind::Telnet(_) => "Telnet",
        ConnectionKind::Shell(_) => "Shell",
        ConnectionKind::Tcp(_) => "TCP",
    }
}

fn kind_color(kind: &ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::Ssh(_) => "#7aa2f7",
        ConnectionKind::Serial(_) => "#e0af68",
        ConnectionKind::Telnet(_) => "#ff9e64",
        ConnectionKind::Shell(_) => "#9ece6a",
        ConnectionKind::Tcp(_) => "#7dcfff",
    }
}

#[component]
pub fn Sidebar(
    connections: Vec<ConnectionConfig>,
    on_connect: EventHandler<String>,
    on_new: EventHandler<()>,
    on_copy: EventHandler<String>,
    on_onekey: EventHandler<()>,
    /// Open the connection dialog in edit mode for the connection with this id.
    on_edit: EventHandler<String>,
    /// Request deletion of the connection with this id (the App component
    /// owns the confirm dialog — the sidebar only triggers it).
    on_delete: EventHandler<String>,
) -> Element {
    let mut search = use_signal(String::new);
    let mut expanded_ssh = use_signal(|| true);
    let mut expanded_shell = use_signal(|| true);
    let mut expanded_other = use_signal(|| false);
    let mut context_menu = use_signal(|| Option::<(String, f64, f64)>::None);

    let search_lower = search.read().to_lowercase();
    let filtered: Vec<ConnectionConfig> = connections
        .into_iter()
        .filter(|c| {
            if search_lower.is_empty() {
                true
            } else {
                c.name.to_lowercase().contains(&search_lower)
                    || kind_label(&c.kind).to_lowercase().contains(&search_lower)
            }
        })
        .collect();

    let ssh_conns: Vec<&ConnectionConfig> = filtered
        .iter()
        .filter(|c| matches!(c.kind, ConnectionKind::Ssh(_)))
        .collect();
    let shell_conns: Vec<&ConnectionConfig> = filtered
        .iter()
        .filter(|c| matches!(c.kind, ConnectionKind::Shell(_)))
        .collect();
    let other_conns: Vec<&ConnectionConfig> = filtered
        .iter()
        .filter(|c| !matches!(c.kind, ConnectionKind::Ssh(_) | ConnectionKind::Shell(_)))
        .collect();

    rsx! {
        // Scoped CSS for the hover-revealed row action icons. Rendered inline
        // (not in main.rs's head) so the sidebar is self-contained; class names
        // are namespaced with `conn-` to avoid collisions.
        style { "
            .conn-icons{{opacity:0;transition:opacity 0.12s;display:flex;gap:2px;align-items:center;}}
            .conn-item:hover .conn-icons{{opacity:1;}}
            .conn-edit{{color:#9ece6a;cursor:pointer;font-size:13px;padding:0 4px;line-height:1;user-select:none;}}
            .conn-edit:hover{{color:#7aa2f7;}}
            .conn-del{{color:#f7768e;cursor:pointer;font-size:13px;padding:0 4px;line-height:1;user-select:none;}}
            .conn-del:hover{{color:#ff5e8f;}}
            .ctx-item{{padding:6px 12px;font-size:12px;cursor:pointer;color:#c0caf5;}}
            .ctx-item:hover{{background:#2a2b3d;}}
            .ctx-danger:hover{{background:#2a2b3d;color:#f7768e;}}
        " }

        div {
            style: "
                width: 260px;
                background: #1a1b26;
                border-right: 1px solid #2a2b3d;
                display: flex;
                flex-direction: column;
                height: 100%;
                color: #c0caf5;
                user-select: none;
            ",

            // Header
            div {
                style: "padding: 12px; display: flex; justify-content: space-between; align-items: center;",
                span { style: "font-weight: 600; font-size: 14px; letter-spacing: 0.3px;", "Connections" }
                div {
                    style: "display: flex; gap: 6px;",
                    button {
                        class: "conn-btn",
                        style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; border-radius: 4px; padding: 4px 10px; cursor: pointer; font-size: 12px;",
                        title: "Configure OneKeys (Expect/Send auto-fill)",
                        onclick: move |_| on_onekey.call(()),
                        "OneKeys"
                    }
                    button {
                        class: "conn-btn",
                        style: "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; padding: 4px 10px; cursor: pointer; font-size: 12px; font-weight: 600;",
                        onclick: move |_| on_new.call(()),
                        "+ New"
                    }
                }
            }

            // Search
            div {
                style: "padding: 0 12px 8px;",
                input {
                    style: "width: 100%; background: #24283b; border: 1px solid #2a2b3d; border-radius: 4px; padding: 6px 8px; color: #c0caf5; font-size: 12px; box-sizing: border-box; outline: none;",
                    r#type: "text",
                    placeholder: "Search connections...",
                    value: "{search}",
                    oninput: move |e| search.set(e.value()),
                }
            }

            // Connection groups
            div {
                style: "flex: 1; overflow-y: auto; padding: 0 4px;",

                // SSH group
                if !ssh_conns.is_empty() {
                    {rsx! {
                        div {
                            style: "padding: 4px 8px; font-size: 11px; color: #565f89; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; cursor: pointer; display: flex; align-items: center; gap: 4px;",
                            onclick: move |_| expanded_ssh.set(!expanded_ssh()),
                            span { style: "font-size: 8px; color: #565f89;", if expanded_ssh() { "▼" } else { "▶" } }
                            span { style: "color: #7aa2f7;", "●" }
                            "SSH ({ssh_conns.len()})"
                        }
                        if expanded_ssh() {
                            for conn in ssh_conns {
                                {rsx! {
                                    ConnItem {
                                        key: "{conn.id}",
                                        conn: conn.clone(),
                                        on_connect: on_connect,
                                        on_copy: on_copy,
                                        on_edit: on_edit,
                                        on_delete: on_delete,
                                        context_menu: context_menu,
                                    }
                                }}
                            }
                        }
                    }}
                }

                // Shell group
                if !shell_conns.is_empty() {
                    {rsx! {
                        div {
                            style: "padding: 4px 8px; margin-top: 4px; font-size: 11px; color: #565f89; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; cursor: pointer; display: flex; align-items: center; gap: 4px;",
                            onclick: move |_| expanded_shell.set(!expanded_shell()),
                            span { style: "font-size: 8px; color: #565f89;", if expanded_shell() { "▼" } else { "▶" } }
                            span { style: "color: #9ece6a;", "●" }
                            "Shell ({shell_conns.len()})"
                        }
                        if expanded_shell() {
                            for conn in shell_conns {
                                {rsx! {
                                    ConnItem {
                                        key: "{conn.id}",
                                        conn: conn.clone(),
                                        on_connect: on_connect,
                                        on_copy: on_copy,
                                        on_edit: on_edit,
                                        on_delete: on_delete,
                                        context_menu: context_menu,
                                    }
                                }}
                            }
                        }
                    }}
                }

                // Other group (Serial, Telnet, TCP)
                if !other_conns.is_empty() {
                    {rsx! {
                        div {
                            style: "padding: 4px 8px; margin-top: 4px; font-size: 11px; color: #565f89; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; cursor: pointer; display: flex; align-items: center; gap: 4px;",
                            onclick: move |_| expanded_other.set(!expanded_other()),
                            span { style: "font-size: 8px; color: #565f89;", if expanded_other() { "▼" } else { "▶" } }
                            span { style: "color: #ff9e64;", "●" }
                            "Other ({other_conns.len()})"
                        }
                        if expanded_other() {
                            for conn in other_conns {
                                {rsx! {
                                    ConnItem {
                                        key: "{conn.id}",
                                        conn: conn.clone(),
                                        on_connect: on_connect,
                                        on_copy: on_copy,
                                        on_edit: on_edit,
                                        on_delete: on_delete,
                                        context_menu: context_menu,
                                    }
                                }}
                            }
                        }
                    }}
                }

                if filtered.is_empty() {
                    div {
                        style: "padding: 24px 12px; text-align: center; color: #565f89; font-size: 12px;",
                        if search_lower.is_empty() {
                            "No connections yet.\nClick + New to add one."
                        } else {
                            "No matching connections."
                        }
                    }
                }
            }
        }

        // Context menu overlay
        if let Some((ref _menu_id, x, y)) = context_menu() {
            div {
                style: "position: fixed; top: 0; left: 0; right: 0; bottom: 0; z-index: 2999;",
                onclick: move |_| context_menu.set(None),
            }
            div {
                style: "position: fixed; top: {y}px; left: {x}px; z-index: 3000; background: #24283b; border: 1px solid #2a2b3d; border-radius: 4px; padding: 4px 0; min-width: 140px; box-shadow: 0 4px 12px rgba(0,0,0,0.4);",

                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() {
                            on_connect.call(id);
                        }
                        context_menu.set(None);
                    },
                    "Connect"
                }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() {
                            on_copy.call(id);
                        }
                        context_menu.set(None);
                    },
                    "Copy Session"
                }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() {
                            on_edit.call(id);
                        }
                        context_menu.set(None);
                    },
                    "Edit…"
                }
                div {
                    class: "ctx-item ctx-danger",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() {
                            on_delete.call(id);
                        }
                        context_menu.set(None);
                    },
                    "Delete"
                }
            }
        }
    }
}

/// Proper Dioxus component for a connection item — has its own hook context
/// so that use_signal doesn't break the parent's hook ordering.
#[component]
fn ConnItem(
    conn: ConnectionConfig,
    on_connect: EventHandler<String>,
    on_copy: EventHandler<String>,
    on_edit: EventHandler<String>,
    on_delete: EventHandler<String>,
    mut context_menu: Signal<Option<(String, f64, f64)>>,
) -> Element {
    let color = kind_color(&conn.kind);
    let id = conn.id.clone();
    let id_for_ctx = conn.id.clone();
    let id_for_edit = conn.id.clone();
    let id_for_del = conn.id.clone();
    let mut hovered = use_signal(|| false);
    let bg = if hovered() { "#24283b" } else { "transparent" };

    rsx! {
        div {
            class: "conn-item",
            style: "
                padding: 6px 10px;
                margin: 1px 4px;
                border-radius: 4px;
                cursor: pointer;
                font-size: 12px;
                display: flex;
                align-items: center;
                gap: 6px;
                background: {bg};
                transition: background 0.1s;
            ",
            onclick: move |_| {
                on_connect.call(id.clone());
            },
            onmouseenter: move |_| hovered.set(true),
            onmouseleave: move |_| hovered.set(false),
            oncontextmenu: move |e: MouseEvent| {
                e.prevent_default();
                context_menu.set(Some((id_for_ctx.clone(), e.client_coordinates().x, e.client_coordinates().y)));
            },

            span {
                style: "width: 6px; height: 6px; border-radius: 50%; background: {color}; flex-shrink: 0;",
            }
            span {
                style: "flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                "{conn.name}"
            }
            if conn.onekey {
                span {
                    style: "font-size: 9px; background: #9ece6a; color: #1a1b26; padding: 1px 5px; border-radius: 3px; font-weight: 600; flex-shrink: 0;",
                    "1-KEY"
                }
            }

            // Hover-revealed inline action icons. `stop_propagation` on their
            // clicks prevents the row's `onclick` (Connect) from also firing.
            // The icons are always in the DOM (so CSS :hover on the row can
            // drive their opacity) but invisible until the row is hovered.
            span {
                class: "conn-icons",
                span {
                    class: "conn-edit",
                    title: "Edit connection",
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        on_edit.call(id_for_edit.clone());
                    },
                    "✎"
                }
                span {
                    class: "conn-del",
                    title: "Delete connection",
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        on_delete.call(id_for_del.clone());
                    },
                    "✕"
                }
            }
        }
    }
}
