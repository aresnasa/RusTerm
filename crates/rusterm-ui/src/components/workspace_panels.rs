use std::fs;
use std::path::{Path, PathBuf};

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use rusterm_core::config::{
    BottomPanelTab, MAX_BOTTOM_PANEL_HEIGHT_PX, MAX_RIGHT_PANEL_WIDTH_PX, MAX_SIDEBAR_WIDTH_PX,
    MIN_BOTTOM_PANEL_HEIGHT_PX, MIN_RIGHT_PANEL_WIDTH_PX, MIN_SIDEBAR_WIDTH_PX, RightPanelTab,
};

use crate::components::TransfersPanel;
use crate::state::SessionTab;
use crate::transfers::TransferJob;

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalFileEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
}

fn initial_local_directory() -> PathBuf {
    dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn read_local_directory(path: &Path) -> Result<Vec<LocalFileEntry>, String> {
    let entries = fs::read_dir(path).map_err(|error| error.to_string())?;
    let mut result = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            Some(LocalFileEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
            })
        })
        .collect::<Vec<_>>();
    result.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(result)
}

fn file_size_label(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let size = size as f64;
    if size >= GB {
        format!("{:.1} GB", size / GB)
    } else if size >= MB {
        format!("{:.1} MB", size / MB)
    } else if size >= KB {
        format!("{:.1} KB", size / KB)
    } else {
        format!("{} B", size as u64)
    }
}

#[component]
pub fn LocalFilePanel(
    width_px: u16,
    on_width_change: EventHandler<u16>,
    on_show_connections: EventHandler<()>,
) -> Element {
    let mut current_dir = use_signal(initial_local_directory);
    let mut refresh_epoch = use_signal(|| 0_u64);
    let mut live_width = use_signal(|| width_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    let _ = refresh_epoch();
    let current_path = current_dir();
    let path_label = current_path.to_string_lossy().into_owned();
    let entries = read_local_directory(&current_path);

    rsx! {
        style { "
            .workspace-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .workspace-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .workspace-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .workspace-file-row{{display:flex;align-items:center;gap:7px;padding:5px 8px;border-radius:3px;font-size:12px;cursor:default;min-width:0;}}
            .workspace-file-row:hover{{background:var(--skin-surface-hover);}}
            .workspace-resize-handle:hover,.workspace-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
        " }
        div {
            style: "position:relative;width:min({live_width}px,45vw);min-width:min({live_width}px,45vw);max-width:min({live_width}px,45vw);flex:0 0 min({live_width}px,45vw);height:100%;display:flex;flex-direction:column;background:var(--skin-bg);border-right:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;",
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                button {
                    class: "workspace-tab",
                    onclick: move |_| on_show_connections.call(()),
                    "Connections"
                }
                button { class: "workspace-tab active", "Local files" }
                button {
                    style: "margin-left:auto;margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;font-size:14px;padding:4px 6px;",
                    title: "Refresh local directory",
                    onclick: move |_| refresh_epoch.set(refresh_epoch().wrapping_add(1)),
                    "↻"
                }
            }
            div {
                style: "display:flex;align-items:center;gap:5px;padding:7px;border-bottom:1px solid var(--skin-border);",
                button {
                    style: "border:1px solid var(--skin-border);background:var(--skin-surface);color:var(--skin-text);border-radius:3px;cursor:pointer;padding:3px 7px;",
                    title: "Parent directory",
                    disabled: current_path.parent().is_none(),
                    onclick: move |_| {
                        if let Some(parent) = current_dir().parent() {
                            current_dir.set(parent.to_path_buf());
                        }
                    },
                    "↑"
                }
                div {
                    style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font:11px ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--skin-text-muted);",
                    title: "{path_label}",
                    "{path_label}"
                }
            }
            div {
                style: "padding:5px 8px;border-bottom:1px solid var(--skin-border);font-size:10px;color:var(--skin-warning);",
                "Local filesystem only · remote SFTP is not available yet"
            }
            div {
                style: "flex:1;overflow:auto;padding:4px;",
                match entries {
                    Ok(entries) if entries.is_empty() => rsx! {
                        div { style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;", "This directory is empty" }
                    },
                    Ok(entries) => rsx! {
                        for entry in entries {
                            {let entry_path = entry.path.clone();
                            let entry_is_dir = entry.is_dir;
                            let entry_key = entry.path.to_string_lossy().into_owned();
                            let size_label = file_size_label(entry.size);
                            rsx! {
                                div {
                                    key: "{entry_key}",
                                    class: "workspace-file-row",
                                    title: if entry.is_dir { "Open directory" } else { "Local file" },
                                    ondoubleclick: move |_| {
                                        if entry_is_dir {
                                            current_dir.set(entry_path.clone());
                                        }
                                    },
                                    span { style: "flex:0 0 auto;color:var(--skin-accent);font-size:13px;", if entry.is_dir { "▸" } else { "·" } }
                                    span { style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{entry.name}" }
                                    if !entry.is_dir {
                                        span { style: "flex:0 0 auto;color:var(--skin-text-muted);font-size:10px;", "{size_label}" }
                                    }
                                }
                            }}
                        }
                    },
                    Err(error) => rsx! {
                        div { style: "padding:20px;color:var(--skin-danger);font-size:12px;overflow-wrap:anywhere;", "Unable to read directory: {error}" }
                    },
                }
            }

            if resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:col-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_x, start_width)) = resize_drag() else { return; };
                        let delta = event.client_coordinates().x - start_x;
                        live_width.set((f64::from(start_width) + delta).round().clamp(f64::from(MIN_SIDEBAR_WIDTH_PX), f64::from(MAX_SIDEBAR_WIDTH_PX)) as u16);
                    },
                    onmouseup: move |_| {
                        resize_drag.set(None);
                        on_width_change.call(live_width());
                    },
                }
            }
            div {
                class: if resize_drag().is_some() { "workspace-resize-handle active" } else { "workspace-resize-handle" },
                style: "position:absolute;right:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;",
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

#[component]
pub fn RightToolPanel(
    width_px: u16,
    active_tab: RightPanelTab,
    sessions: Vec<SessionTab>,
    active_session: Option<String>,
    on_width_change: EventHandler<u16>,
    on_tab_change: EventHandler<RightPanelTab>,
    on_select_session: EventHandler<String>,
    on_run_history: EventHandler<String>,
    on_close: EventHandler<()>,
) -> Element {
    let mut search = use_signal(String::new);
    let mut live_width = use_signal(|| width_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    let active_name = active_session.as_ref().and_then(|session_id| {
        sessions
            .iter()
            .find(|session| &session.id == session_id)
            .map(|session| session.name.clone())
    });
    let search_lower = search().trim().to_lowercase();
    let mut history = active_session
        .as_ref()
        .and_then(|session_id| sessions.iter().find(|session| &session.id == session_id))
        .map(|session| session.command_history.clone())
        .unwrap_or_default();
    history.reverse();
    history.retain(|command| {
        search_lower.is_empty() || command.to_lowercase().contains(&search_lower)
    });

    rsx! {
        style { "
            .workspace-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .workspace-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .workspace-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .workspace-session-row,.workspace-history-row{{display:flex;align-items:center;gap:7px;padding:6px 8px;border-radius:3px;font-size:12px;min-width:0;cursor:pointer;}}
            .workspace-session-row:hover,.workspace-history-row:hover{{background:var(--skin-surface-hover);}}
            .workspace-session-row.active{{background:color-mix(in srgb,var(--skin-accent) 16%,transparent);color:var(--skin-text);}}
            .workspace-resize-handle:hover,.workspace-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
        " }
        div {
            style: "position:relative;width:min({live_width}px,45vw);min-width:min({live_width}px,45vw);max-width:min({live_width}px,45vw);flex:0 0 min({live_width}px,45vw);height:100%;display:flex;flex-direction:column;background:var(--skin-bg);border-left:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;",
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                button {
                    class: if active_tab == RightPanelTab::Sessions { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(RightPanelTab::Sessions),
                    "Sessions"
                }
                button {
                    class: if active_tab == RightPanelTab::History { "workspace-tab active" } else { "workspace-tab" },
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

            if active_tab == RightPanelTab::Sessions {
                div { style: "padding:7px 9px;font-size:10px;color:var(--skin-text-muted);border-bottom:1px solid var(--skin-border);", "OPEN SESSIONS · {sessions.len()}" }
                div {
                    style: "flex:1;overflow:auto;padding:4px;",
                    if sessions.is_empty() {
                        div { style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;", "No open sessions" }
                    }
                    for session in sessions {
                        {let session_id = session.id.clone();
                        let session_kind = format!("{:?}", session.kind).to_uppercase();
                        rsx! {
                            div {
                                key: "{session.id}",
                                class: if active_session.as_deref() == Some(session.id.as_str()) { "workspace-session-row active" } else { "workspace-session-row" },
                                onclick: move |_| on_select_session.call(session_id.clone()),
                                span { style: "color:var(--skin-success);font-size:10px;", "●" }
                                span { style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{session.name}" }
                                span { style: "color:var(--skin-text-muted);font-size:9px;", "{session_kind}" }
                            }
                        }}
                    }
                }
            } else {
                div {
                    style: "padding:7px;border-bottom:1px solid var(--skin-border);",
                    input {
                        style: "width:100%;box-sizing:border-box;background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:4px;padding:6px 8px;color:var(--skin-text);font-size:11px;outline:none;",
                        placeholder: "Filter command history...",
                        value: "{search}",
                        oninput: move |event| search.set(event.value()),
                    }
                }
                div {
                    style: "padding:6px 9px;font-size:10px;color:var(--skin-text-muted);border-bottom:1px solid var(--skin-border);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                    if let Some(name) = active_name { "{name} · double-click to run" } else { "Select an active session" }
                }
                div {
                    style: "flex:1;overflow:auto;padding:4px;",
                    if history.is_empty() {
                        div { style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;", "No matching command history" }
                    }
                    for (index, command) in history.into_iter().enumerate() {
                        {let command_to_run = command.clone();
                        rsx! {
                            div {
                                key: "history-{index}-{command}",
                                class: "workspace-history-row",
                                title: "Double-click to run",
                                ondoubleclick: move |_| on_run_history.call(command_to_run.clone()),
                                span { style: "flex:0 0 auto;color:var(--skin-text-muted);font-size:10px;", "{index + 1}" }
                                code { style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--skin-text);font-size:11px;", "{command}" }
                            }
                        }}
                    }
                }
            }

            if resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:col-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_x, start_width)) = resize_drag() else { return; };
                        let delta = start_x - event.client_coordinates().x;
                        live_width.set((f64::from(start_width) + delta).round().clamp(f64::from(MIN_RIGHT_PANEL_WIDTH_PX), f64::from(MAX_RIGHT_PANEL_WIDTH_PX)) as u16);
                    },
                    onmouseup: move |_| {
                        resize_drag.set(None);
                        on_width_change.call(live_width());
                    },
                }
            }
            div {
                class: if resize_drag().is_some() { "workspace-resize-handle active" } else { "workspace-resize-handle" },
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

#[component]
pub fn BottomToolPanel(
    height_px: u16,
    embedded: bool,
    active_tab: BottomPanelTab,
    target_label: String,
    shell_content: Option<Element>,
    transfer_jobs: Vec<TransferJob>,
    on_height_change: EventHandler<u16>,
    on_tab_change: EventHandler<BottomPanelTab>,
    on_send: EventHandler<String>,
    on_open_shell: EventHandler<()>,
    on_terminate_shell: EventHandler<()>,
    on_cancel_transfer: EventHandler<String>,
    on_retry_transfer: EventHandler<String>,
    on_clear_transfers: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    let mut command = use_signal(String::new);
    let mut live_height = use_signal(|| height_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    rsx! {
        style { "
            .workspace-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .workspace-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .workspace-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .workspace-primary-button{{border:1px solid var(--skin-accent);background:var(--skin-accent);color:var(--skin-bg);border-radius:4px;padding:6px 12px;font-size:11px;font-weight:600;cursor:pointer;}}
            .workspace-primary-button:disabled{{opacity:.45;cursor:default;}}
            .workspace-resize-handle:hover,.workspace-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
        " }
        div {
            style: if embedded {
                "position:relative;width:100%;min-width:0;height:100%;min-height:0;display:flex;flex-direction:column;background:var(--skin-bg);box-sizing:border-box;overflow:hidden;".to_string()
            } else {
                format!("position:relative;height:min({live_height}px,55vh);min-height:min({live_height}px,55vh);max-height:min({live_height}px,55vh);flex:0 0 min({live_height}px,55vh);display:flex;flex-direction:column;background:var(--skin-bg);border-top:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;")
            },
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                if !embedded {
                button {
                    class: if active_tab == BottomPanelTab::Send { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Send),
                    "Send"
                }
                button {
                    class: if active_tab == BottomPanelTab::Shell { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Shell),
                    "Shell"
                }
                button {
                    class: if active_tab == BottomPanelTab::Transfers { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Transfers),
                    "Transfers"
                }
                }
                span { style: "margin-left:auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:var(--skin-text-muted);font-size:10px;padding:0 8px;", "Target: {target_label}" }
                if active_tab == BottomPanelTab::Shell && shell_content.is_some() {
                    button {
                        style: "border:0;background:transparent;color:var(--skin-danger);cursor:pointer;padding:4px 7px;font-size:11px;",
                        title: "Terminate embedded shell",
                        onclick: move |_| on_terminate_shell.call(()),
                        "Terminate"
                    }
                }
                if !embedded {
                button {
                    style: "margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;padding:4px 7px;font-size:14px;",
                    title: "Hide bottom panel",
                    onclick: move |_| on_close.call(()),
                    "×"
                }
                }
            }

            match active_tab {
                BottomPanelTab::Send => rsx! {
                    div {
                        style: "flex:1;display:flex;gap:8px;padding:9px;min-height:0;",
                        textarea {
                            style: "min-width:0;flex:1;resize:none;background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:4px;padding:8px 9px;color:var(--skin-text);font:12px ui-monospace,SFMono-Regular,Menlo,monospace;outline:none;",
                            placeholder: "Command to send (Ctrl/Cmd+Enter to run)...",
                            value: "{command}",
                            oninput: move |event| command.set(event.value()),
                            onkeydown: move |event: KeyboardEvent| {
                                if matches!(event.key(), Key::Enter)
                                    && (event.modifiers().ctrl() || event.modifiers().meta())
                                {
                                    event.prevent_default();
                                    let value = command().trim().to_string();
                                    if !value.is_empty() {
                                        on_send.call(value);
                                        command.set(String::new());
                                    }
                                }
                            },
                        }
                        div {
                            style: "display:flex;flex-direction:column;justify-content:flex-end;gap:6px;",
                            button {
                                class: "workspace-primary-button",
                                disabled: command().trim().is_empty(),
                                onclick: move |_| {
                                    let value = command().trim().to_string();
                                    if !value.is_empty() {
                                        on_send.call(value);
                                        command.set(String::new());
                                    }
                                },
                                "Send ↵"
                            }
                        }
                    }
                },
                BottomPanelTab::Shell => rsx! {
                    if let Some(content) = shell_content {
                        div {
                            style: "flex:1;min-height:0;overflow:hidden;position:relative;",
                            {content}
                        }
                    } else {
                        div {
                            style: "flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:9px;color:var(--skin-text-muted);font-size:12px;",
                            div { style: "font-size:24px;color:var(--skin-accent);", ">_" }
                            div { "Start a local shell embedded in this bottom panel." }
                            button { class: "workspace-primary-button", onclick: move |_| on_open_shell.call(()), "Start local shell" }
                        }
                    }
                },
                BottomPanelTab::Transfers => rsx! {
                    TransfersPanel {
                        jobs: transfer_jobs,
                        on_cancel: move |job_id| on_cancel_transfer.call(job_id),
                        on_retry: move |job_id| on_retry_transfer.call(job_id),
                        on_clear_finished: move |_| on_clear_transfers.call(()),
                    }
                },
            }

            if !embedded && resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:row-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_y, start_height)) = resize_drag() else { return; };
                        let delta = start_y - event.client_coordinates().y;
                        live_height.set((f64::from(start_height) + delta).round().clamp(f64::from(MIN_BOTTOM_PANEL_HEIGHT_PX), f64::from(MAX_BOTTOM_PANEL_HEIGHT_PX)) as u16);
                    },
                    onmouseup: move |_| {
                        resize_drag.set(None);
                        on_height_change.call(live_height());
                    },
                }
            }
            if !embedded {
            div {
                class: if resize_drag().is_some() { "workspace-resize-handle active" } else { "workspace-resize-handle" },
                style: "position:absolute;left:0;top:-3px;width:100%;height:6px;z-index:80;cursor:row-resize;background:transparent;",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        resize_drag.set(Some((event.client_coordinates().y, live_height())));
                    }
                },
            }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_directory_entries_sort_directories_first() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a-file"), b"abc").unwrap();
        fs::create_dir(temp.path().join("z-dir")).unwrap();

        let entries = read_local_directory(temp.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert_eq!(entries[0].name, "z-dir");
        assert_eq!(entries[1].name, "a-file");
    }

    #[test]
    fn file_size_labels_are_compact() {
        assert_eq!(file_size_label(12), "12 B");
        assert_eq!(file_size_label(1024), "1.0 KB");
        assert_eq!(file_size_label(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn missing_local_directory_returns_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        assert!(read_local_directory(&missing).is_err());
    }
}
