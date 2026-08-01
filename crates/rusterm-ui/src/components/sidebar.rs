use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use rusterm_core::config::{
    ConnectionConfig, ConnectionGroup, ConnectionKind, MAX_SIDEBAR_WIDTH_PX, MIN_SIDEBAR_WIDTH_PX,
    SidebarPreferences,
};

use super::icon::{Icon, IconName};

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
        ConnectionKind::Ssh(_) => "var(--skin-accent)",
        ConnectionKind::Serial(_) => "var(--skin-warning)",
        ConnectionKind::Telnet(_) => "#ff9e64",
        ConnectionKind::Shell(_) => "var(--skin-success)",
        ConnectionKind::Tcp(_) => "#7dcfff",
    }
}

fn kind_icon(kind: &ConnectionKind) -> IconName {
    match kind {
        ConnectionKind::Ssh(_) => IconName::Ssh,
        ConnectionKind::Serial(_) => IconName::Serial,
        ConnectionKind::Telnet(_) => IconName::Telnet,
        ConnectionKind::Shell(_) => IconName::Shell,
        ConnectionKind::Tcp(_) => IconName::Tcp,
    }
}

pub(crate) fn connection_is_visible(
    preferences: &SidebarPreferences,
    connection_id: &str,
    show_hidden: bool,
) -> bool {
    show_hidden
        || !preferences
            .hidden_connection_ids
            .iter()
            .any(|id| id == connection_id)
}

fn connection_matches_search(connection: &ConnectionConfig, search: &str) -> bool {
    search.is_empty()
        || connection.name.to_lowercase().contains(search)
        || kind_label(&connection.kind).to_lowercase().contains(search)
}

pub(crate) fn create_group(
    preferences: &SidebarPreferences,
    name: &str,
) -> Option<SidebarPreferences> {
    let name = name.trim();
    if name.is_empty()
        || preferences
            .groups
            .iter()
            .any(|group| group.name.eq_ignore_ascii_case(name))
    {
        return None;
    }

    let mut updated = preferences.clone();
    updated.groups.push(ConnectionGroup {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        collapsed: false,
    });
    Some(updated)
}

#[component]
pub fn Sidebar(
    connections: Vec<ConnectionConfig>,
    preferences: SidebarPreferences,
    drag_over_group: Option<String>,
    on_preferences_change: EventHandler<SidebarPreferences>,
    on_group_change: EventHandler<(String, Option<String>)>,
    on_group_delete: EventHandler<String>,
    on_connect: EventHandler<String>,
    on_new: EventHandler<()>,
    on_copy: EventHandler<String>,
    on_onekey: EventHandler<()>,
    on_edit: EventHandler<String>,
    on_delete: EventHandler<String>,
    on_drag_start: EventHandler<(ConnectionConfig, String, f64, f64)>,
) -> Element {
    let mut search = use_signal(String::new);
    let mut show_hidden = use_signal(|| false);
    let mut creating_group = use_signal(|| false);
    let mut new_group_name = use_signal(String::new);
    let mut context_menu = use_signal(|| Option::<(String, f64, f64)>::None);
    let mut live_width = use_signal(|| preferences.clone().normalized().width_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    let normalized_preferences = preferences.normalized();
    let search_lower = search.read().trim().to_lowercase();
    let hidden_count = normalized_preferences.hidden_connection_ids.len();
    let visible_connections: Vec<ConnectionConfig> = connections
        .into_iter()
        .filter(|connection| {
            connection_is_visible(&normalized_preferences, &connection.id, show_hidden())
                && connection_matches_search(connection, &search_lower)
        })
        .collect();

    let grouped: Vec<(ConnectionGroup, Vec<ConnectionConfig>)> = normalized_preferences
        .groups
        .iter()
        .cloned()
        .map(|group| {
            let members = visible_connections
                .iter()
                .filter(|connection| connection.group.as_deref() == Some(group.id.as_str()))
                .cloned()
                .collect();
            (group, members)
        })
        .collect();
    let ungrouped: Vec<ConnectionConfig> = visible_connections
        .iter()
        .filter(|connection| {
            connection.group.as_ref().is_none_or(|group_id| {
                !normalized_preferences
                    .groups
                    .iter()
                    .any(|group| group.id == *group_id)
            })
        })
        .cloned()
        .collect();

    let sidebar_style = format!(
        "position:relative;width:min({}px,45vw);min-width:min({}px,45vw);max-width:min({}px,45vw);flex:0 0 min({}px,45vw);background:var(--skin-bg);border-right:1px solid var(--skin-border);display:flex;flex-direction:column;height:100%;color:var(--skin-text);user-select:none;box-sizing:border-box;",
        live_width(),
        MIN_SIDEBAR_WIDTH_PX,
        MAX_SIDEBAR_WIDTH_PX,
        live_width()
    );
    let hidden_button_title = if show_hidden() {
        "Hide hidden connections again"
    } else {
        "Show hidden connections"
    };
    let hidden_button_color = if show_hidden() {
        "var(--skin-accent)"
    } else {
        "var(--skin-text-muted)"
    };

    let prefs_for_create_key = normalized_preferences.clone();
    let prefs_for_create_click = normalized_preferences.clone();
    let prefs_for_hidden = normalized_preferences.clone();
    let prefs_for_resize = normalized_preferences.clone();
    let prefs_for_resize_overlay = normalized_preferences.clone();
    let groups_for_menu = normalized_preferences.groups.clone();
    let hidden_ids_for_menu = normalized_preferences.hidden_connection_ids.clone();

    rsx! {
        style { "
            .sidebar-icon-button{{display:inline-flex;align-items:center;justify-content:center;width:26px;height:26px;padding:0;border:1px solid var(--skin-border);border-radius:4px;background:transparent;color:var(--skin-text);cursor:pointer;}}
            .sidebar-icon-button:hover{{background:var(--skin-surface);color:var(--skin-accent);border-color:var(--skin-border-strong);}}
            .conn-icons{{opacity:0;transition:opacity .12s;display:flex;gap:1px;align-items:center;}}
            .conn-item:hover .conn-icons{{opacity:1;}}
            .conn-row-action{{display:inline-flex;align-items:center;justify-content:center;color:var(--skin-text-muted);cursor:pointer;padding:2px;}}
            .conn-row-action:hover{{color:var(--skin-accent);}}
            .ctx-item{{padding:6px 12px;font-size:12px;cursor:pointer;color:var(--skin-text);display:flex;align-items:center;gap:8px;white-space:nowrap;}}
            .ctx-item:hover{{background:var(--skin-border);}}
            .ctx-label{{padding:7px 12px 3px;font-size:10px;color:var(--skin-text-muted);text-transform:uppercase;letter-spacing:.5px;}}
            .ctx-danger:hover{{color:var(--skin-danger);}}
            .sidebar-resize-handle:hover,.sidebar-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.55);}}
            .connection-group-header{{border:1px solid transparent;border-radius:4px;transition:background .1s,border-color .1s,color .1s;}}
            .connection-group-header.connection-group-drop-target{{background:rgba(122,162,247,.16);border-color:var(--skin-accent);color:var(--skin-text);}}
        " }

        div {
            style: "{sidebar_style}",

            div {
                style: "padding:10px 10px 8px;display:flex;justify-content:space-between;align-items:center;gap:8px;",
                span { style: "font-weight:600;font-size:14px;letter-spacing:.3px;", "Connections" }
                div {
                    style: "display:flex;gap:5px;",
                    button {
                        class: "sidebar-icon-button",
                        style: "color:{hidden_button_color};",
                        title: "{hidden_button_title}",
                        onclick: move |_| show_hidden.set(!show_hidden()),
                        Icon { name: if show_hidden() { IconName::Eye } else { IconName::EyeOff }, size: 15 }
                        if hidden_count > 0 {
                            span { style: "font-size:9px;margin-left:1px;", "{hidden_count}" }
                        }
                    }
                    button {
                        class: "sidebar-icon-button",
                        title: "Create connection group",
                        onclick: move |_| creating_group.set(!creating_group()),
                        Icon { name: IconName::Folder, size: 15 }
                        Icon { name: IconName::Plus, size: 10 }
                    }
                    button {
                        class: "sidebar-icon-button",
                        title: "Configure OneKeys",
                        onclick: move |_| on_onekey.call(()),
                        Icon { name: IconName::Key, size: 15 }
                    }
                    button {
                        class: "sidebar-icon-button",
                        style: "background:var(--skin-accent);color:var(--skin-bg);border-color:var(--skin-accent);",
                        title: "Create connection",
                        onclick: move |_| on_new.call(()),
                        Icon { name: IconName::Plus, size: 16 }
                    }
                }
            }

            if creating_group() {
                div {
                    style: "padding:0 10px 8px;display:flex;gap:5px;",
                    input {
                        style: "min-width:0;flex:1;background:var(--skin-surface);border:1px solid var(--skin-border-strong);border-radius:4px;padding:6px 8px;color:var(--skin-text);font-size:12px;outline:none;",
                        r#type: "text",
                        placeholder: "Group name",
                        value: "{new_group_name}",
                        autofocus: true,
                        oninput: move |event| new_group_name.set(event.value()),
                        onkeydown: move |event: KeyboardEvent| {
                            if matches!(event.key(), Key::Enter) {
                                event.prevent_default();
                                if let Some(updated) = create_group(&prefs_for_create_key, &new_group_name()) {
                                    on_preferences_change.call(updated);
                                    new_group_name.set(String::new());
                                    creating_group.set(false);
                                }
                            } else if matches!(event.key(), Key::Escape) {
                                event.prevent_default();
                                new_group_name.set(String::new());
                                creating_group.set(false);
                            }
                        },
                    }
                    button {
                        class: "sidebar-icon-button",
                        title: "Add group",
                        onclick: move |_| {
                            if let Some(updated) = create_group(&prefs_for_create_click, &new_group_name()) {
                                on_preferences_change.call(updated);
                                new_group_name.set(String::new());
                                creating_group.set(false);
                            }
                        },
                        Icon { name: IconName::Plus, size: 15 }
                    }
                }
            }

            div {
                style: "padding:0 10px 8px;position:relative;",
                span {
                    style: "position:absolute;left:18px;top:7px;color:var(--skin-text-muted);display:inline-flex;pointer-events:none;",
                    Icon { name: IconName::Search, size: 14 }
                }
                input {
                    style: "width:100%;background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:4px;padding:6px 8px 6px 28px;color:var(--skin-text);font-size:12px;box-sizing:border-box;outline:none;",
                    r#type: "text",
                    placeholder: "Search connections...",
                    value: "{search}",
                    oninput: move |event| search.set(event.value()),
                }
            }

            div {
                style: "flex:1;overflow-y:auto;padding:0 4px 8px;",
                for (group, members) in grouped {
                    ConnectionGroupSection {
                        key: "{group.id}",
                        group,
                        connections: members,
                        drag_over_group: drag_over_group.clone(),
                        hidden_ids: normalized_preferences.hidden_connection_ids.clone(),
                        preferences: normalized_preferences.clone(),
                        on_preferences_change,
                        on_group_delete,
                        on_connect,
                        on_copy,
                        on_edit,
                        on_delete,
                        on_drag_start,
                        context_menu,
                    }
                }

                if !ungrouped.is_empty() {
                    div {
                        style: "padding:5px 8px 3px;font-size:11px;color:var(--skin-text-muted);font-weight:600;text-transform:uppercase;letter-spacing:.5px;display:flex;align-items:center;gap:6px;",
                        Icon { name: IconName::FolderOpen, size: 14 }
                        "Ungrouped ({ungrouped.len()})"
                    }
                    for connection in ungrouped {
                        ConnItem {
                            key: "{connection.id}",
                            hidden: normalized_preferences.hidden_connection_ids.iter().any(|id| id == &connection.id),
                            conn: connection,
                            on_connect,
                            on_edit,
                            on_delete,
                            on_drag_start,
                            context_menu,
                        }
                    }
                }

                if visible_connections.is_empty() {
                    div {
                        style: "padding:24px 12px;text-align:center;color:var(--skin-text-muted);font-size:12px;white-space:pre-line;",
                        if !search_lower.is_empty() {
                            "No matching connections."
                        } else if hidden_count > 0 && !show_hidden() {
                            "All connections are hidden.\nUse the eye button to restore them."
                        } else {
                            "No connections yet.\nUse + to create one."
                        }
                    }
                }
            }

            if resize_drag().is_some() {
                // Fallback for WebViews that do not keep sending mouse events
                // to the narrow handle after the pointer leaves it. On
                // platforms with implicit capture the handle remains primary;
                // otherwise this fixed layer completes and persists the drag.
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:col-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_x, start_width)) = resize_drag() else { return; };
                        event.prevent_default();
                        let delta = event.client_coordinates().x - start_x;
                        let width = (f64::from(start_width) + delta)
                            .round()
                            .clamp(f64::from(MIN_SIDEBAR_WIDTH_PX), f64::from(MAX_SIDEBAR_WIDTH_PX)) as u16;
                        live_width.set(width);
                    },
                    onmouseup: move |event: MouseEvent| {
                        event.prevent_default();
                        resize_drag.set(None);
                        let mut updated = prefs_for_resize_overlay.clone();
                        updated.width_px = live_width();
                        on_preferences_change.call(updated);
                    },
                }
            }

            div {
                class: if resize_drag().is_some() { "sidebar-resize-handle active" } else { "sidebar-resize-handle" },
                style: "position:absolute;right:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;transition:background .1s;",
                title: "Drag to resize connection sidebar",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        event.stop_propagation();
                        resize_drag.set(Some((event.client_coordinates().x, live_width())));
                    }
                },
                onmousemove: move |event: MouseEvent| {
                    let Some((start_x, start_width)) = resize_drag() else { return; };
                    event.prevent_default();
                    let delta = event.client_coordinates().x - start_x;
                    let width = (f64::from(start_width) + delta)
                        .round()
                        .clamp(f64::from(MIN_SIDEBAR_WIDTH_PX), f64::from(MAX_SIDEBAR_WIDTH_PX)) as u16;
                    live_width.set(width);
                },
                onmouseup: move |event: MouseEvent| {
                    if resize_drag().is_none() { return; }
                    event.prevent_default();
                    event.stop_propagation();
                    resize_drag.set(None);
                    let mut updated = prefs_for_resize.clone();
                    updated.width_px = live_width();
                    on_preferences_change.call(updated);
                },
            }
        }

        if let Some((ref _menu_id, x, y)) = context_menu() {
            div {
                style: "position:fixed;inset:0;z-index:2999;",
                onclick: move |_| context_menu.set(None),
            }
            div {
                style: "position:fixed;top:{y}px;left:{x}px;z-index:3000;background:var(--skin-surface);border:1px solid var(--skin-border-strong);border-radius:5px;padding:4px 0;min-width:180px;max-height:70vh;overflow-y:auto;box-shadow:0 6px 18px rgba(0,0,0,.5);",
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() { on_connect.call(id); }
                        context_menu.set(None);
                    },
                    Icon { name: IconName::Connect, size: 14 }
                    "Connect"
                }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() { on_copy.call(id); }
                        context_menu.set(None);
                    },
                    Icon { name: IconName::Plus, size: 14 }
                    "Copy connection"
                }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() { on_edit.call(id); }
                        context_menu.set(None);
                    },
                    Icon { name: IconName::Edit, size: 14 }
                    "Edit…"
                }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() {
                            let mut updated = prefs_for_hidden.clone();
                            if let Some(index) = updated.hidden_connection_ids.iter().position(|hidden| hidden == &id) {
                                updated.hidden_connection_ids.remove(index);
                            } else {
                                updated.hidden_connection_ids.push(id);
                            }
                            on_preferences_change.call(updated);
                        }
                        context_menu.set(None);
                    },
                    Icon {
                        name: if context_menu().as_ref().is_some_and(|(id, _, _)| hidden_ids_for_menu.iter().any(|hidden| hidden == id)) {
                            IconName::Eye
                        } else {
                            IconName::EyeOff
                        },
                        size: 14,
                    }
                    if context_menu().as_ref().is_some_and(|(id, _, _)| hidden_ids_for_menu.iter().any(|hidden| hidden == id)) {
                        "Show in sidebar"
                    } else {
                        "Hide from sidebar"
                    }
                }

                div { style: "height:1px;background:var(--skin-border-strong);margin:4px 0;" }
                div { class: "ctx-label", "Move to group" }
                div {
                    class: "ctx-item",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() { on_group_change.call((id, None)); }
                        context_menu.set(None);
                    },
                    Icon { name: IconName::FolderOpen, size: 14 }
                    "Ungrouped"
                }
                for group in groups_for_menu {
                    div {
                        class: "ctx-item",
                        onclick: move |_| {
                            if let Some((id, _, _)) = context_menu() {
                                on_group_change.call((id, Some(group.id.clone())));
                            }
                            context_menu.set(None);
                        },
                        Icon { name: IconName::Folder, size: 14 }
                        "{group.name}"
                    }
                }

                div { style: "height:1px;background:var(--skin-border-strong);margin:4px 0;" }
                div {
                    class: "ctx-item ctx-danger",
                    onclick: move |_| {
                        if let Some((id, _, _)) = context_menu() { on_delete.call(id); }
                        context_menu.set(None);
                    },
                    Icon { name: IconName::Delete, size: 14 }
                    "Delete"
                }
            }
        }
    }
}

#[component]
fn ConnectionGroupSection(
    group: ConnectionGroup,
    connections: Vec<ConnectionConfig>,
    drag_over_group: Option<String>,
    hidden_ids: Vec<String>,
    preferences: SidebarPreferences,
    on_preferences_change: EventHandler<SidebarPreferences>,
    on_group_delete: EventHandler<String>,
    on_connect: EventHandler<String>,
    on_copy: EventHandler<String>,
    on_edit: EventHandler<String>,
    on_delete: EventHandler<String>,
    on_drag_start: EventHandler<(ConnectionConfig, String, f64, f64)>,
    mut context_menu: Signal<Option<(String, f64, f64)>>,
) -> Element {
    let group_id = group.id.clone();
    let group_id_for_delete = group.id.clone();
    let preferences_for_toggle = preferences.clone();
    let collapsed = group.collapsed;
    let is_drop_target = drag_over_group.as_deref() == Some(group.id.as_str());
    rsx! {
        div {
            class: if is_drop_target { "connection-group-header connection-group-drop-target" } else { "connection-group-header" },
            "data-rusterm-group-id": "{group.id}",
            style: "padding:5px 8px 3px;font-size:11px;color:var(--skin-text-muted);font-weight:600;text-transform:uppercase;letter-spacing:.5px;cursor:pointer;display:flex;align-items:center;gap:6px;",
            onclick: move |_| {
                let mut updated = preferences_for_toggle.clone();
                if let Some(group) = updated.groups.iter_mut().find(|group| group.id == group_id) {
                    group.collapsed = !group.collapsed;
                }
                on_preferences_change.call(updated);
            },
            Icon { name: if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }, size: 12 }
            Icon { name: if collapsed { IconName::Folder } else { IconName::FolderOpen }, size: 14 }
            span { style: "flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{group.name} ({connections.len()})" }
            span {
                class: "conn-row-action",
                title: "Delete group and move its connections to Ungrouped",
                onclick: move |event: MouseEvent| {
                    event.stop_propagation();
                    on_group_delete.call(group_id_for_delete.clone());
                },
                Icon { name: IconName::Delete, size: 12 }
            }
        }
        if !collapsed {
            for connection in connections {
                ConnItem {
                    key: "{connection.id}",
                    hidden: hidden_ids.iter().any(|id| id == &connection.id),
                    conn: connection,
                    on_connect,
                    on_edit,
                    on_delete,
                    on_drag_start,
                    context_menu,
                }
            }
        }
    }
}

#[component]
fn ConnItem(
    conn: ConnectionConfig,
    hidden: bool,
    on_connect: EventHandler<String>,
    on_edit: EventHandler<String>,
    on_delete: EventHandler<String>,
    on_drag_start: EventHandler<(ConnectionConfig, String, f64, f64)>,
    mut context_menu: Signal<Option<(String, f64, f64)>>,
) -> Element {
    let color = kind_color(&conn.kind);
    let icon = kind_icon(&conn.kind);
    let id = conn.id.clone();
    let id_for_ctx = conn.id.clone();
    let id_for_edit = conn.id.clone();
    let id_for_delete = conn.id.clone();
    let conn_for_drag = conn.clone();
    let name_for_drag = conn.name.clone();
    let row_opacity = if hidden { "0.5" } else { "1" };

    rsx! {
        div {
            class: "conn-item",
            style: "padding:6px 9px;margin:1px 4px;border-radius:4px;cursor:pointer;font-size:12px;display:flex;align-items:center;gap:7px;background:transparent;opacity:{row_opacity};transition:background .1s,opacity .1s;",
            title: "{kind_label(&conn.kind)} · {conn.name}",
            onclick: move |_| on_connect.call(id.clone()),
            onmousedown: move |event: MouseEvent| {
                if event.trigger_button() == Some(MouseButton::Primary) {
                    event.prevent_default();
                    let point = event.client_coordinates();
                    on_drag_start.call((conn_for_drag.clone(), name_for_drag.clone(), point.x, point.y));
                }
            },
            oncontextmenu: move |event: MouseEvent| {
                event.prevent_default();
                context_menu.set(Some((id_for_ctx.clone(), event.client_coordinates().x, event.client_coordinates().y)));
            },
            span { style: "display:inline-flex;color:{color};flex-shrink:0;", Icon { name: icon, size: 15 } }
            span { style: "flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{conn.name}" }
            if hidden {
                span { style: "display:inline-flex;color:var(--skin-text-muted);", Icon { name: IconName::EyeOff, size: 12 } }
            }
            if conn.onekey {
                span { style: "display:inline-flex;color:var(--skin-success);", title: "OneKey enabled", Icon { name: IconName::Key, size: 12 } }
            }
            span {
                class: "conn-icons",
                span {
                    class: "conn-row-action",
                    title: "Edit connection",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        on_edit.call(id_for_edit.clone());
                    },
                    Icon { name: IconName::Edit, size: 13 }
                }
                span {
                    class: "conn-row-action",
                    title: "Delete connection",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        on_delete.call(id_for_delete.clone());
                    },
                    Icon { name: IconName::Delete, size: 13 }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_connections_are_filtered_until_reveal_is_enabled() {
        let preferences = SidebarPreferences {
            hidden_connection_ids: vec!["hidden".to_string()],
            ..SidebarPreferences::default()
        };
        assert!(!connection_is_visible(&preferences, "hidden", false));
        assert!(connection_is_visible(&preferences, "hidden", true));
        assert!(connection_is_visible(&preferences, "visible", false));
    }

    #[test]
    fn create_group_trims_name_and_rejects_empty_or_duplicate_names() {
        let preferences = SidebarPreferences::default();
        let updated = create_group(&preferences, "  Production  ").expect("valid group");
        assert_eq!(updated.groups.len(), 1);
        assert_eq!(updated.groups[0].name, "Production");
        assert!(!updated.groups[0].id.is_empty());

        assert!(create_group(&updated, "   ").is_none());
        assert!(create_group(&updated, "production").is_none());
    }
}
