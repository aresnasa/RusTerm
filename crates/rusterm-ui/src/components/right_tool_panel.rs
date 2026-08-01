use std::path::PathBuf;

use chrono::{DateTime, Local};
use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use rusterm_core::config::{MAX_RIGHT_PANEL_WIDTH_PX, MIN_RIGHT_PANEL_WIDTH_PX, RightPanelTab};
use rusterm_db::Database;
use rusterm_db::history::{HistoryCursor, HistoryEntry, HistoryPage};

use crate::state::{AppState, SessionConnectionState, build_session_tree, focused_pane_session};

const HISTORY_PAGE_SIZE: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
struct HistoryQuery {
    contains: String,
    current_session_only: bool,
    session_id: Option<String>,
}

fn database_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_default()
        .join("rusterm")
        .join("rusterm.db")
}

fn start_history_request(
    query: HistoryQuery,
    before: Option<HistoryCursor>,
    append: bool,
    mut entries: Signal<Vec<HistoryEntry>>,
    mut next_cursor: Signal<Option<HistoryCursor>>,
    mut loading: Signal<bool>,
    mut error: Signal<Option<String>>,
    mut request_epoch: Signal<u64>,
) {
    let epoch = (*request_epoch.peek()).wrapping_add(1);
    request_epoch.set(epoch);
    loading.set(true);
    error.set(None);

    if !append {
        entries.set(Vec::new());
        next_cursor.set(None);
    }

    if query.current_session_only && query.session_id.is_none() {
        loading.set(false);
        error.set(Some(
            "No focused or active session is available for this filter.".to_string(),
        ));
        return;
    }

    spawn(async move {
        let db_path = database_path();
        let result = match Database::open(Some(db_path)).await {
            Ok(database) => {
                let contains = (!query.contains.is_empty()).then_some(query.contains.as_str());
                database
                    .list_history_page(
                        contains,
                        query.session_id.as_deref(),
                        before.as_ref(),
                        HISTORY_PAGE_SIZE,
                    )
                    .await
            }
            Err(open_error) => Err(open_error),
        };

        if *request_epoch.peek() != epoch {
            return;
        }

        match result {
            Ok(HistoryPage {
                entries: page_entries,
                next_cursor: page_cursor,
            }) => {
                if append {
                    entries.write().extend(page_entries);
                } else {
                    entries.set(page_entries);
                }
                next_cursor.set(page_cursor);
                error.set(None);
            }
            Err(query_error) => {
                error.set(Some(format!(
                    "Unable to load command history: {query_error}"
                )));
            }
        }
        loading.set(false);
    });
}

fn connection_status(state: SessionConnectionState) -> (&'static str, &'static str, &'static str) {
    match state {
        SessionConnectionState::Connected => ("●", "Connected", "var(--skin-success)"),
        SessionConnectionState::Disconnected => ("○", "Disconnected", "var(--skin-danger)"),
        SessionConnectionState::Reconnecting => ("◌", "Reconnecting", "var(--skin-warning)"),
    }
}

fn format_history_time(created_at: &str) -> String {
    DateTime::parse_from_rfc3339(created_at)
        .map(|timestamp| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| created_at.to_string())
}

#[component]
pub fn RightToolPanel(
    state: Signal<AppState>,
    width_px: u16,
    embedded: bool,
    active_tab: RightPanelTab,
    on_width_change: EventHandler<u16>,
    on_tab_change: EventHandler<RightPanelTab>,
    on_select_session: EventHandler<String>,
    on_run_history: EventHandler<String>,
    on_close: EventHandler<()>,
) -> Element {
    let mut search = use_signal(String::new);
    let mut current_session_only = use_signal(|| false);
    let history_entries = use_signal(Vec::<HistoryEntry>::new);
    let next_cursor = use_signal(|| Option::<HistoryCursor>::None);
    let loading = use_signal(|| false);
    let error = use_signal(|| Option::<String>::None);
    let request_epoch = use_signal(|| 0_u64);
    let mut live_width = use_signal(|| width_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    let history_query = use_memo(move || {
        let filter_current = current_session_only();
        let session_id = filter_current.then(|| {
            let app = state.read();
            focused_pane_session(&app).or_else(|| app.active_session.clone())
        });

        HistoryQuery {
            contains: search().trim().to_string(),
            current_session_only: filter_current,
            session_id: session_id.flatten(),
        }
    });

    use_effect(move || {
        start_history_request(
            history_query(),
            None,
            false,
            history_entries,
            next_cursor,
            loading,
            error,
            request_epoch,
        );
    });

    let session_tree = build_session_tree(&state.read());
    let session_count = session_tree
        .iter()
        .flat_map(|workspace| workspace.panes.iter())
        .filter(|pane| pane.session.is_some())
        .count();
    let entries_snapshot = history_entries();
    let cursor_snapshot = next_cursor();
    let loading_snapshot = loading();
    let error_snapshot = error();
    let query_snapshot = history_query();
    let current_session_name = query_snapshot.session_id.as_deref().and_then(|session_id| {
        state
            .read()
            .sessions
            .iter()
            .find(|session| session.id == session_id)
            .map(|session| session.name.clone())
    });

    rsx! {
        style { "
            .right-tool-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .right-tool-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .right-tool-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .right-tool-workspace{{margin:4px;border:1px solid var(--skin-border);border-radius:5px;overflow:hidden;}}
            .right-tool-workspace-header{{display:flex;align-items:center;gap:6px;padding:6px 8px;background:var(--skin-surface);font-size:11px;min-width:0;}}
            .right-tool-pane{{border-top:1px solid var(--skin-border);}}
            .right-tool-pane-header{{display:flex;align-items:center;gap:6px;padding:5px 8px 5px 17px;color:var(--skin-text-muted);font-size:10px;}}
            .right-tool-session-row{{display:flex;align-items:center;gap:7px;padding:6px 8px 6px 29px;font-size:12px;min-width:0;cursor:pointer;}}
            .right-tool-session-row:hover,.right-tool-history-row:hover{{background:var(--skin-surface-hover);}}
            .right-tool-session-row.active{{background:color-mix(in srgb,var(--skin-accent) 16%,transparent);color:var(--skin-text);}}
            .right-tool-badge{{flex:0 0 auto;padding:1px 4px;border:1px solid var(--skin-border);border-radius:8px;color:var(--skin-text-muted);font-size:8px;line-height:1.3;}}
            .right-tool-badge.active{{border-color:var(--skin-accent);color:var(--skin-accent);}}
            .right-tool-badge.focused{{border-color:var(--skin-warning);color:var(--skin-warning);}}
            .right-tool-history-row{{display:flex;flex-direction:column;gap:4px;padding:7px 8px;border-radius:3px;min-width:0;cursor:pointer;}}
            .right-tool-history-meta{{display:flex;align-items:center;gap:7px;min-width:0;color:var(--skin-text-muted);font-size:9px;}}
            .right-tool-history-meta-item{{min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}}
            .right-tool-resize-handle:hover,.right-tool-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
        " }
        div {
            style: if embedded {
                "position:relative;width:100%;min-width:0;height:100%;display:flex;flex-direction:column;background:var(--skin-bg);box-sizing:border-box;overflow:hidden;".to_string()
            } else {
                format!("position:relative;width:min({live_width}px,45vw);min-width:min({live_width}px,45vw);max-width:min({live_width}px,45vw);flex:0 0 min({live_width}px,45vw);height:100%;display:flex;flex-direction:column;background:var(--skin-bg);border-left:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;")
            },
            if !embedded {
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                button {
                    class: if active_tab == RightPanelTab::Sessions { "right-tool-tab active" } else { "right-tool-tab" },
                    onclick: move |_| on_tab_change.call(RightPanelTab::Sessions),
                    "Sessions"
                }
                button {
                    class: if active_tab == RightPanelTab::History { "right-tool-tab active" } else { "right-tool-tab" },
                    onclick: move |_| on_tab_change.call(RightPanelTab::History),
                    "History"
                }
                button {
                    style: "margin-left:auto;margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;padding:4px 7px;font-size:14px;",
                    title: "Hide right panel",
                    onclick: move |_| on_close.call(()),
                    "×"
                }
            }
            }

            if active_tab == RightPanelTab::Sessions {
                div {
                    style: "padding:7px 9px;font-size:10px;color:var(--skin-text-muted);border-bottom:1px solid var(--skin-border);",
                    "OPEN SESSIONS · {session_count}"
                }
                div {
                    style: "flex:1;overflow:auto;padding:1px 0 4px;",
                    if session_tree.is_empty() {
                        div {
                            style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;",
                            "No open workspaces"
                        }
                    }
                    for (workspace_index, workspace) in session_tree.into_iter().enumerate() {
                        {let workspace_label = workspace
                            .anchor_session_id
                            .as_deref()
                            .and_then(|anchor_id| {
                                workspace
                                    .panes
                                    .iter()
                                    .filter_map(|pane| pane.session.as_ref())
                                    .find(|session| session.id == anchor_id)
                                    .map(|session| session.name.clone())
                            })
                            .unwrap_or_else(|| workspace.tab_id.clone());
                        rsx! {
                            div {
                                key: "{workspace.tab_id}",
                                class: "right-tool-workspace",
                                div {
                                    class: "right-tool-workspace-header",
                                    span { style: "color:var(--skin-accent);font-size:10px;", "▾" }
                                    span {
                                        style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                        title: "{workspace.tab_id}",
                                        "Workspace {workspace_index + 1} · {workspace_label}"
                                    }
                                    if workspace.is_active {
                                        span { class: "right-tool-badge active", "ACTIVE" }
                                    }
                                }
                                for pane in workspace.panes {
                                    div {
                                        key: "{workspace.tab_id}-pane-{pane.index}",
                                        class: "right-tool-pane",
                                        div {
                                            class: "right-tool-pane-header",
                                            span { "Pane {pane.index + 1}" }
                                            if pane.is_focused {
                                                span { class: "right-tool-badge focused", "FOCUSED" }
                                            }
                                        }
                                        if let Some(session) = pane.session {
                                            {let session_id = session.id.clone();
                                            let session_kind = format!("{:?}", session.kind).to_uppercase();
                                            let (connection_icon, connection_label, connection_color) = connection_status(session.connection_state);
                                            rsx! {
                                                div {
                                                    class: if session.is_active { "right-tool-session-row active" } else { "right-tool-session-row" },
                                                    title: "Select {session.name}",
                                                    onclick: move |_| on_select_session.call(session_id.clone()),
                                                    span {
                                                        style: "flex:0 0 auto;color:{connection_color};font-size:10px;",
                                                        title: "{connection_label}",
                                                        "{connection_icon}"
                                                    }
                                                    span {
                                                        style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                                                        "{session.name}"
                                                    }
                                                    if session.is_active {
                                                        span { class: "right-tool-badge active", "ACTIVE" }
                                                    }
                                                    span { style: "flex:0 0 auto;color:var(--skin-text-muted);font-size:9px;", "{session_kind}" }
                                                    span {
                                                        style: "flex:0 0 auto;color:{connection_color};font-size:9px;",
                                                        "{connection_label}"
                                                    }
                                                }
                                            }}
                                        } else {
                                            div {
                                                style: "padding:5px 8px 7px 29px;color:var(--skin-text-muted);font-size:11px;font-style:italic;",
                                                "Empty pane"
                                            }
                                        }
                                    }
                                }
                            }
                        }}
                    }
                }
            } else {
                div {
                    style: "padding:7px;border-bottom:1px solid var(--skin-border);display:flex;flex-direction:column;gap:7px;",
                    input {
                        style: "width:100%;box-sizing:border-box;background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:4px;padding:6px 8px;color:var(--skin-text);font-size:11px;outline:none;",
                        placeholder: "Search command history (contains)...",
                        value: "{search}",
                        oninput: move |event| search.set(event.value()),
                    }
                    label {
                        style: "display:flex;align-items:center;gap:6px;color:var(--skin-text-muted);font-size:10px;cursor:pointer;min-width:0;",
                        input {
                            r#type: "checkbox",
                            checked: current_session_only(),
                            style: "accent-color:var(--skin-accent);cursor:pointer;",
                            onchange: move |event| current_session_only.set(event.checked()),
                        }
                        span {
                            style: "min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                            if let Some(name) = current_session_name {
                                "Current focused/active session only · {name}"
                            } else {
                                "Current focused/active session only"
                            }
                        }
                    }
                }
                div {
                    style: "padding:6px 9px;font-size:10px;color:var(--skin-text-muted);border-bottom:1px solid var(--skin-border);",
                    "Persistent history · newest first · double-click to run"
                }
                div {
                    style: "flex:1;overflow:auto;padding:4px;",
                    if let Some(message) = error_snapshot.as_deref() {
                        div {
                            style: "margin:4px;padding:8px;border:1px solid var(--skin-danger);border-radius:4px;color:var(--skin-danger);font-size:11px;overflow-wrap:anywhere;",
                            "{message}"
                        }
                    }
                    if entries_snapshot.is_empty() && loading_snapshot {
                        div {
                            style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;",
                            "Loading history…"
                        }
                    } else if entries_snapshot.is_empty() && error_snapshot.is_none() {
                        div {
                            style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;",
                            "No matching persistent history"
                        }
                    }
                    for entry in entries_snapshot {
                        {let command_to_run = entry.command.clone();
                        let hostname = entry.hostname.as_deref().unwrap_or("—").to_string();
                        let cwd = entry.cwd.as_deref().unwrap_or("—").to_string();
                        let time = format_history_time(&entry.created_at);
                        rsx! {
                            div {
                                key: "{entry.id}",
                                class: "right-tool-history-row",
                                title: "Double-click to run",
                                ondoubleclick: move |_| on_run_history.call(command_to_run.clone()),
                                code {
                                    style: "width:100%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--skin-text);font-size:11px;",
                                    title: "{entry.command}",
                                    "{entry.command}"
                                }
                                div {
                                    class: "right-tool-history-meta",
                                    span { class: "right-tool-history-meta-item", style: "flex:0 1 auto;", title: "Host: {hostname}", "host {hostname}" }
                                    span { "·" }
                                    span { class: "right-tool-history-meta-item", style: "flex:1 1 auto;", title: "Cwd: {cwd}", "cwd {cwd}" }
                                    span { "·" }
                                    span { class: "right-tool-history-meta-item", style: "flex:0 0 auto;", title: "Time: {entry.created_at}", "{time}" }
                                }
                            }
                        }}
                    }
                    if loading_snapshot && !history_entries.read().is_empty() {
                        div {
                            style: "padding:9px;text-align:center;color:var(--skin-text-muted);font-size:11px;",
                            "Loading more…"
                        }
                    } else if cursor_snapshot.is_some() {
                        button {
                            style: "display:block;width:calc(100% - 8px);margin:6px 4px;padding:6px;border:1px solid var(--skin-border);border-radius:4px;background:var(--skin-surface);color:var(--skin-text-muted);font-size:11px;cursor:pointer;",
                            onclick: move |_| {
                                let before = next_cursor();
                                if loading() || before.is_none() {
                                    return;
                                }
                                start_history_request(
                                    history_query(),
                                    before,
                                    true,
                                    history_entries,
                                    next_cursor,
                                    loading,
                                    error,
                                    request_epoch,
                                );
                            },
                            "Load more"
                        }
                    }
                }
            }

            if !embedded && resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:col-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_x, start_width)) = resize_drag() else {
                            return;
                        };
                        let delta = start_x - event.client_coordinates().x;
                        live_width.set(
                            (f64::from(start_width) + delta)
                                .round()
                                .clamp(
                                    f64::from(MIN_RIGHT_PANEL_WIDTH_PX),
                                    f64::from(MAX_RIGHT_PANEL_WIDTH_PX),
                                ) as u16,
                        );
                    },
                    onmouseup: move |_| {
                        resize_drag.set(None);
                        on_width_change.call(live_width());
                    },
                }
            }
            if !embedded {
            div {
                class: if resize_drag().is_some() { "right-tool-resize-handle active" } else { "right-tool-resize-handle" },
                style: "position:absolute;left:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        resize_drag.set(Some((event.client_coordinates().x, live_width())));
                    }
                },
            }
            }
        }
    }
}
