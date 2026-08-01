use std::path::PathBuf;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use rusterm_core::config::{MAX_RIGHT_PANEL_WIDTH_PX, MIN_RIGHT_PANEL_WIDTH_PX};
use rusterm_core::session::SessionType;
use rusterm_ssh::{RemoteDirEntry, RemoteFileType, SftpClient};

use crate::state::{AppState, focused_pane_session};
use crate::transfers::{FileEndpoint, TransferRequest};

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshSessionOption {
    id: String,
    label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingDialog {
    CreateDirectory,
    Rename(RemoteDirEntry),
    Delete(RemoteDirEntry),
}

fn available_ssh_sessions(state: &AppState) -> Vec<SshSessionOption> {
    state
        .sessions
        .iter()
        .filter(|session| {
            session.kind == SessionType::Ssh && state.ssh_sessions.contains_key(&session.id)
        })
        .map(|session| SshSessionOption {
            id: session.id.clone(),
            label: session
                .hostname
                .as_ref()
                .filter(|hostname| !hostname.is_empty() && *hostname != &session.name)
                .map(|hostname| format!("{} · {hostname}", session.name))
                .unwrap_or_else(|| session.name.clone()),
        })
        .collect()
}

fn preferred_ssh_session(state: &AppState) -> Option<String> {
    let available = available_ssh_sessions(state);
    let focused = focused_pane_session(state);

    focused
        .as_ref()
        .filter(|id| {
            available
                .iter()
                .any(|session| session.id.as_str() == id.as_str())
        })
        .cloned()
        .or_else(|| {
            state
                .active_session
                .as_ref()
                .filter(|id| {
                    available
                        .iter()
                        .any(|session| session.id.as_str() == id.as_str())
                })
                .cloned()
        })
        .or_else(|| available.first().map(|session| session.id.clone()))
}

async fn sftp_client(mut state: Signal<AppState>, session_id: &str) -> Result<SftpClient, String> {
    if let Some(client) = state.read().sftp_clients.get(session_id).cloned() {
        return Ok(client);
    }

    let ssh_session = state
        .read()
        .ssh_sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| "SSH session is no longer connected".to_string())?;
    let opened = ssh_session
        .open_sftp()
        .await
        .map_err(|error| format!("Unable to open SFTP: {error}"))?;

    let mut app = state.write();
    if let Some(existing) = app.sftp_clients.get(session_id).cloned() {
        return Ok(existing);
    }
    if !app.ssh_sessions.contains_key(session_id) {
        return Err("SSH session disconnected while SFTP was opening".to_string());
    }
    app.sftp_clients
        .insert(session_id.to_string(), opened.clone());
    Ok(opened)
}

async fn list_remote_directory(
    state: Signal<AppState>,
    session_id: String,
    path: String,
) -> Result<Vec<RemoteDirEntry>, String> {
    let client = sftp_client(state, &session_id).await?;
    let mut entries = client
        .list(path)
        .await
        .map_err(|error| format!("Unable to list remote directory: {error}"))?;
    entries.retain(|entry| !matches!(entry.name.as_str(), "." | ".."));
    entries.sort_by(|left, right| {
        let left_is_directory = left.metadata.file_type == RemoteFileType::Directory;
        let right_is_directory = right.metadata.file_type == RemoteFileType::Directory;
        right_is_directory
            .cmp(&left_is_directory)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

async fn create_remote_directory(
    state: Signal<AppState>,
    session_id: String,
    path: String,
) -> Result<(), String> {
    sftp_client(state, &session_id)
        .await?
        .mkdir(path)
        .await
        .map_err(|error| format!("Unable to create directory: {error}"))
}

async fn rename_remote_entry(
    state: Signal<AppState>,
    session_id: String,
    old_path: String,
    new_path: String,
) -> Result<(), String> {
    sftp_client(state, &session_id)
        .await?
        .rename(old_path, new_path)
        .await
        .map_err(|error| format!("Unable to rename remote entry: {error}"))
}

async fn delete_remote_entry(
    state: Signal<AppState>,
    session_id: String,
    entry: RemoteDirEntry,
) -> Result<(), String> {
    let client = sftp_client(state, &session_id).await?;
    match entry.metadata.file_type {
        RemoteFileType::File | RemoteFileType::Symlink => client
            .remove_file(entry.path)
            .await
            .map_err(|error| format!("Unable to delete remote entry: {error}")),
        RemoteFileType::Directory => client
            .remove_empty_dir(entry.path)
            .await
            .map_err(|error| format!("Unable to delete empty directory: {error}")),
        RemoteFileType::Other => Err("This remote entry type cannot be deleted here".to_string()),
    }
}

fn validate_entry_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        Err("Name cannot be empty")
    } else if matches!(name, "." | "..") {
        Err("Name cannot be . or ..")
    } else if name.contains('/') {
        Err("Name cannot contain /")
    } else {
        Ok(name)
    }
}

fn normalize_posix_path(path: &str) -> Result<String, &'static str> {
    let path = path.trim();
    if !path.starts_with('/') {
        return Err("Remote path must be absolute and start with /");
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }

    if components.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn posix_join(parent: &str, child: &str) -> String {
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        format!("/{child}")
    } else if child.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

fn posix_parent(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        return None;
    }

    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(path[..index].to_string()),
        None => None,
    }
}

fn file_type_label(file_type: RemoteFileType) -> &'static str {
    match file_type {
        RemoteFileType::File => "file",
        RemoteFileType::Directory => "directory",
        RemoteFileType::Symlink => "symlink",
        RemoteFileType::Other => "other",
    }
}

fn file_type_icon(file_type: RemoteFileType) -> &'static str {
    match file_type {
        RemoteFileType::Directory => "▸",
        RemoteFileType::Symlink => "↗",
        RemoteFileType::File => "·",
        RemoteFileType::Other => "?",
    }
}

fn file_size_label(size: Option<u64>) -> String {
    let Some(size) = size else {
        return "—".to_string();
    };
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let size_float = size as f64;
    if size_float >= GB {
        format!("{:.1} GB", size_float / GB)
    } else if size_float >= MB {
        format!("{:.1} MB", size_float / MB)
    } else if size_float >= KB {
        format!("{:.1} KB", size_float / KB)
    } else {
        format!("{size} B")
    }
}

#[component]
pub fn RemoteFilesPanel(
    state: Signal<AppState>,
    width_px: u16,
    embedded: bool,
    on_width_change: EventHandler<u16>,
    on_show_connections: EventHandler<()>,
    on_transfer: EventHandler<TransferRequest>,
) -> Element {
    let initial_session = preferred_ssh_session(&state.read());
    let mut selected_session = use_signal(move || initial_session);
    let mut current_path = use_signal(|| "/".to_string());
    let mut path_draft = use_signal(|| "/".to_string());
    let entries = use_signal(Vec::<RemoteDirEntry>::new);
    let mut selected_entry = use_signal(|| Option::<RemoteDirEntry>::None);
    let mut refresh_epoch = use_signal(|| 0_u64);
    let load_sequence = use_signal(|| 0_u64);
    let loading = use_signal(|| false);
    let load_error = use_signal(|| Option::<String>::None);
    let mut action_busy = use_signal(|| false);
    let mut status = use_signal(|| Option::<String>::None);
    let mut action_error = use_signal(|| Option::<String>::None);
    let mut pending_dialog = use_signal(|| Option::<PendingDialog>::None);
    let mut dialog_input = use_signal(String::new);
    let mut live_width = use_signal(|| width_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);

    let state_for_session_sync = state;
    let mut selected_for_session_sync = selected_session;
    let mut path_for_session_sync = current_path;
    let mut draft_for_session_sync = path_draft;
    use_effect(move || {
        let snapshot = state_for_session_sync.read();
        let available = available_ssh_sessions(&snapshot);
        let current = selected_for_session_sync.peek().clone();
        let current_is_available = current
            .as_ref()
            .is_some_and(|id| available.iter().any(|session| &session.id == id));
        if !current_is_available {
            let replacement = preferred_ssh_session(&snapshot);
            drop(snapshot);
            selected_for_session_sync.set(replacement);
            path_for_session_sync.set("/".to_string());
            draft_for_session_sync.set("/".to_string());
        }
    });

    let state_for_load = state;
    let selected_for_load = selected_session;
    let path_for_load = current_path;
    let refresh_for_load = refresh_epoch;
    let mut entries_for_load = entries;
    let mut selection_for_load = selected_entry;
    let mut sequence_for_load = load_sequence;
    let mut loading_for_load = loading;
    let mut error_for_load = load_error;
    use_effect(move || {
        let session_id = selected_for_load();
        let path = path_for_load();
        let _ = refresh_for_load();
        let request_id = (*sequence_for_load.peek()).wrapping_add(1);
        sequence_for_load.set(request_id);
        selection_for_load.set(None);
        error_for_load.set(None);

        let Some(session_id) = session_id else {
            entries_for_load.set(Vec::new());
            loading_for_load.set(false);
            return;
        };

        loading_for_load.set(true);
        spawn(async move {
            let result = list_remote_directory(state_for_load, session_id, path).await;
            if *sequence_for_load.peek() != request_id {
                return;
            }
            loading_for_load.set(false);
            match result {
                Ok(listed) => entries_for_load.set(listed),
                Err(error) => {
                    entries_for_load.set(Vec::new());
                    error_for_load.set(Some(error));
                }
            }
        });
    });

    let session_options = available_ssh_sessions(&state.read());
    let session_options_empty = session_options.is_empty();
    let selected_session_value = selected_session().unwrap_or_default();
    let selected = selected_entry();
    let can_rename = selected.is_some() && !action_busy();
    let can_delete = selected.as_ref().is_some_and(|entry| {
        matches!(
            entry.metadata.file_type,
            RemoteFileType::File | RemoteFileType::Directory | RemoteFileType::Symlink
        )
    }) && !action_busy();
    let can_download = selected
        .as_ref()
        .is_some_and(|entry| entry.metadata.file_type == RemoteFileType::File)
        && !action_busy();
    let has_session = !selected_session_value.is_empty();

    rsx! {
        style { "
            .remote-files-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .remote-files-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .remote-files-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .remote-files-button{{border:1px solid var(--skin-border);background:var(--skin-surface);color:var(--skin-text);border-radius:3px;cursor:pointer;padding:4px 7px;font-size:11px;white-space:nowrap;}}
            .remote-files-button:hover:not(:disabled){{border-color:var(--skin-accent);color:var(--skin-accent);}}
            .remote-files-button:disabled{{opacity:.4;cursor:default;}}
            .remote-files-row{{display:flex;align-items:center;gap:7px;padding:5px 8px;border-radius:3px;font-size:12px;cursor:default;min-width:0;border:1px solid transparent;user-select:none;}}
            .remote-files-row:hover{{background:var(--skin-surface-hover);}}
            .remote-files-row.selected{{background:color-mix(in srgb,var(--skin-accent) 16%,transparent);border-color:color-mix(in srgb,var(--skin-accent) 35%,transparent);}}
            .remote-files-resize-handle:hover,.remote-files-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
            .remote-files-input{{min-width:0;border:1px solid var(--skin-border);background:var(--skin-bg);color:var(--skin-text);border-radius:3px;padding:4px 6px;font:11px ui-monospace,SFMono-Regular,Menlo,monospace;outline:none;}}
            .remote-files-input:focus{{border-color:var(--skin-accent);}}
        " }
        div {
            style: if embedded {
                "position:relative;width:100%;min-width:0;height:100%;display:flex;flex-direction:column;background:var(--skin-bg);box-sizing:border-box;overflow:hidden;".to_string()
            } else {
                format!("position:relative;width:min({live_width}px,45vw);min-width:min({live_width}px,45vw);max-width:min({live_width}px,45vw);flex:0 0 min({live_width}px,45vw);height:100%;display:flex;flex-direction:column;background:var(--skin-bg);border-left:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;")
            },
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                if !embedded {
                    button {
                        class: "remote-files-tab",
                        onclick: move |_| on_show_connections.call(()),
                        "Connections"
                    }
                    button { class: "remote-files-tab active", "Remote files" }
                }
                button {
                    class: "remote-files-button",
                    style: "margin-left:auto;margin-right:5px;border:0;background:transparent;font-size:14px;",
                    title: "Refresh remote directory",
                    disabled: !has_session || loading(),
                    onclick: move |_| refresh_epoch.set(refresh_epoch().wrapping_add(1)),
                    "↻"
                }
            }

            div {
                style: "display:flex;align-items:center;gap:6px;padding:7px;border-bottom:1px solid var(--skin-border);",
                select {
                    style: "min-width:0;flex:1;border:1px solid var(--skin-border);background:var(--skin-surface);color:var(--skin-text);border-radius:3px;padding:5px;font-size:11px;outline:none;",
                    value: "{selected_session_value}",
                    disabled: session_options_empty || action_busy(),
                    onchange: move |event| {
                        let value = event.value();
                        selected_session.set((!value.is_empty()).then_some(value));
                        current_path.set("/".to_string());
                        path_draft.set("/".to_string());
                        status.set(None);
                        action_error.set(None);
                    },
                    if session_options_empty {
                        option { value: "", "No connected SSH sessions" }
                    }
                    for session in session_options {
                        option { key: "{session.id}", value: "{session.id}", "{session.label}" }
                    }
                }
            }

            div {
                style: "display:flex;align-items:center;gap:5px;padding:7px;border-bottom:1px solid var(--skin-border);",
                button {
                    class: "remote-files-button",
                    title: "Parent directory",
                    disabled: posix_parent(&current_path()).is_none() || loading() || action_busy(),
                    onclick: move |_| {
                        if let Some(parent) = posix_parent(&current_path()) {
                            current_path.set(parent.clone());
                            path_draft.set(parent);
                            status.set(None);
                            action_error.set(None);
                        }
                    },
                    "↑"
                }
                input {
                    class: "remote-files-input",
                    style: "flex:1;",
                    value: "{path_draft}",
                    disabled: !has_session || action_busy(),
                    oninput: move |event| path_draft.set(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter {
                            match normalize_posix_path(&path_draft()) {
                                Ok(path) => {
                                    current_path.set(path.clone());
                                    path_draft.set(path);
                                    action_error.set(None);
                                }
                                Err(error) => action_error.set(Some(error.to_string())),
                            }
                        }
                    },
                }
                button {
                    class: "remote-files-button",
                    disabled: !has_session || action_busy(),
                    onclick: move |_| {
                        match normalize_posix_path(&path_draft()) {
                            Ok(path) => {
                                current_path.set(path.clone());
                                path_draft.set(path);
                                action_error.set(None);
                            }
                            Err(error) => action_error.set(Some(error.to_string())),
                        }
                    },
                    "Go"
                }
            }

            div {
                style: "display:flex;align-items:center;gap:5px;padding:6px 7px;border-bottom:1px solid var(--skin-border);overflow-x:auto;",
                button {
                    class: "remote-files-button",
                    disabled: !has_session || action_busy(),
                    onclick: move |_| {
                        dialog_input.set(String::new());
                        action_error.set(None);
                        pending_dialog.set(Some(PendingDialog::CreateDirectory));
                    },
                    "New folder"
                }
                button {
                    class: "remote-files-button",
                    disabled: !can_rename,
                    onclick: move |_| {
                        if let Some(entry) = selected_entry() {
                            dialog_input.set(entry.name.clone());
                            action_error.set(None);
                            pending_dialog.set(Some(PendingDialog::Rename(entry)));
                        }
                    },
                    "Rename"
                }
                button {
                    class: "remote-files-button",
                    disabled: !can_delete,
                    onclick: move |_| {
                        if let Some(entry) = selected_entry() {
                            pending_dialog.set(Some(PendingDialog::Delete(entry)));
                        }
                    },
                    "Delete"
                }
                button {
                    class: "remote-files-button",
                    disabled: !has_session || action_busy(),
                    onclick: move |_| {
                        let Some(session_id) = selected_session.peek().clone() else { return; };
                        let remote_directory = current_path.peek().clone();
                        action_busy.set(true);
                        action_error.set(None);
                        status.set(Some("Choosing a local file…".to_string()));
                        spawn(async move {
                            let Some(file) = rfd::AsyncFileDialog::new().pick_file().await else {
                                action_busy.set(false);
                                status.set(Some("Upload cancelled".to_string()));
                                return;
                            };
                            let local_path = file.path().to_path_buf();
                            let Some(file_name) = local_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .map(str::to_owned)
                            else {
                                action_busy.set(false);
                                action_error.set(Some("Selected file has no valid UTF-8 name".to_string()));
                                status.set(None);
                                return;
                            };
                            let metadata = match tokio::fs::metadata(&local_path).await {
                                Ok(metadata) if metadata.is_file() => metadata,
                                Ok(_) => {
                                    action_busy.set(false);
                                    action_error.set(Some("Please select a regular local file".to_string()));
                                    status.set(None);
                                    return;
                                }
                                Err(error) => {
                                    action_busy.set(false);
                                    action_error.set(Some(format!("Unable to read local file metadata: {error}")));
                                    status.set(None);
                                    return;
                                }
                            };
                            on_transfer.call(TransferRequest {
                                session: session_id,
                                source: FileEndpoint::Local(local_path),
                                destination: FileEndpoint::Remote(posix_join(&remote_directory, &file_name)),
                                total: metadata.len(),
                            });
                            action_busy.set(false);
                            status.set(Some(format!("Upload queued: {file_name}")));
                            refresh_epoch.set(refresh_epoch().wrapping_add(1));
                        });
                    },
                    "Upload"
                }
                button {
                    class: "remote-files-button",
                    disabled: !can_download,
                    onclick: move |_| {
                        let Some(session_id) = selected_session.peek().clone() else { return; };
                        let Some(entry) = selected_entry.peek().clone() else { return; };
                        if entry.metadata.file_type != RemoteFileType::File {
                            return;
                        }
                        action_busy.set(true);
                        action_error.set(None);
                        status.set(Some("Choosing download destination…".to_string()));
                        spawn(async move {
                            let Some(destination) = rfd::AsyncFileDialog::new()
                                .set_file_name(&entry.name)
                                .save_file()
                                .await
                            else {
                                action_busy.set(false);
                                status.set(Some("Download cancelled".to_string()));
                                return;
                            };
                            let local_path: PathBuf = destination.path().to_path_buf();
                            on_transfer.call(TransferRequest {
                                session: session_id,
                                source: FileEndpoint::Remote(entry.path.clone()),
                                destination: FileEndpoint::Local(local_path),
                                total: entry.metadata.size.unwrap_or(0),
                            });
                            action_busy.set(false);
                            status.set(Some(format!("Download queued: {}", entry.name)));
                            refresh_epoch.set(refresh_epoch().wrapping_add(1));
                        });
                    },
                    "Download"
                }
            }

            if loading() {
                div { style: "padding:6px 8px;border-bottom:1px solid var(--skin-border);font-size:10px;color:var(--skin-accent);", "Loading remote directory…" }
            }
            if let Some(error) = load_error() {
                div { style: "padding:6px 8px;border-bottom:1px solid var(--skin-border);font-size:10px;color:var(--skin-danger);overflow-wrap:anywhere;", "{error}" }
            }
            if let Some(error) = action_error() {
                div { style: "padding:6px 8px;border-bottom:1px solid var(--skin-border);font-size:10px;color:var(--skin-danger);overflow-wrap:anywhere;", "{error}" }
            } else if let Some(message) = status() {
                div { style: "padding:6px 8px;border-bottom:1px solid var(--skin-border);font-size:10px;color:var(--skin-text-muted);overflow-wrap:anywhere;", "{message}" }
            }

            div {
                style: "flex:1;overflow:auto;padding:4px;",
                if !has_session {
                    div {
                        style: "padding:22px 14px;text-align:center;color:var(--skin-text-muted);font-size:12px;line-height:1.5;",
                        "Connect an SSH session to browse remote files."
                    }
                } else if !loading() && load_error().is_none() && entries().is_empty() {
                    div { style: "padding:20px;text-align:center;color:var(--skin-text-muted);font-size:12px;", "This directory is empty" }
                } else {
                    for entry in entries() {
                        {let entry_key = entry.path.clone();
                        let entry_for_click = entry.clone();
                        let entry_for_double_click = entry.clone();
                        let entry_path = entry.path.clone();
                        let is_directory = entry.metadata.file_type == RemoteFileType::Directory;
                        let is_selected = selected_entry()
                            .as_ref()
                            .is_some_and(|selected| selected.path == entry.path);
                        let icon = file_type_icon(entry.metadata.file_type);
                        let kind = file_type_label(entry.metadata.file_type);
                        let size = file_size_label(entry.metadata.size);
                        rsx! {
                            div {
                                key: "{entry_key}",
                                class: if is_selected { "remote-files-row selected" } else { "remote-files-row" },
                                title: if is_directory { "Double-click to open directory" } else { "{kind}" },
                                onclick: move |_| selected_entry.set(Some(entry_for_click.clone())),
                                ondoubleclick: move |_| {
                                    if is_directory {
                                        current_path.set(entry_path.clone());
                                        path_draft.set(entry_path.clone());
                                        status.set(None);
                                        action_error.set(None);
                                    } else {
                                        selected_entry.set(Some(entry_for_double_click.clone()));
                                    }
                                },
                                span { style: "flex:0 0 auto;color:var(--skin-accent);font-size:13px;", "{icon}" }
                                span { style: "min-width:0;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{entry.name}" }
                                span { style: "flex:0 0 auto;color:var(--skin-text-muted);font-size:10px;", "{size}" }
                            }
                        }}
                    }
                }
            }

            if action_busy() {
                div {
                    style: "position:absolute;inset:0;z-index:70;background:color-mix(in srgb,var(--skin-bg) 35%,transparent);cursor:progress;",
                }
            }

            if let Some(dialog) = pending_dialog() {
                {let is_delete = matches!(dialog, PendingDialog::Delete(_));
                let dialog_title = match &dialog {
                    PendingDialog::CreateDirectory => "Create remote directory",
                    PendingDialog::Rename(_) => "Rename remote entry",
                    PendingDialog::Delete(_) => "Confirm remote deletion",
                };
                let delete_message = match &dialog {
                    PendingDialog::Delete(entry) if entry.metadata.file_type == RemoteFileType::Directory => {
                        format!("Delete empty directory ‘{}’? Non-empty directories are never deleted recursively.", entry.name)
                    }
                    PendingDialog::Delete(entry) if entry.metadata.file_type == RemoteFileType::Symlink => {
                        format!("Delete symbolic link ‘{}’? Its target will not be followed or deleted.", entry.name)
                    }
                    PendingDialog::Delete(entry) => format!("Delete remote file ‘{}’?", entry.name),
                    _ => String::new(),
                };
                let dialog_for_submit = dialog.clone();
                rsx! {
                    div {
                        style: "position:absolute;inset:0;z-index:90;background:rgba(0,0,0,.55);display:flex;align-items:center;justify-content:center;padding:16px;",
                        div {
                            style: "width:min(360px,100%);background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:6px;box-shadow:0 14px 36px rgba(0,0,0,.45);padding:14px;box-sizing:border-box;",
                            h3 { style: "margin:0 0 10px;color:var(--skin-text);font-size:14px;", "{dialog_title}" }
                            if is_delete {
                                p { style: "margin:0 0 14px;color:var(--skin-text-muted);font-size:12px;line-height:1.5;overflow-wrap:anywhere;", "{delete_message}" }
                            } else {
                                input {
                                    class: "remote-files-input",
                                    style: "width:100%;box-sizing:border-box;margin-bottom:14px;",
                                    autofocus: true,
                                    value: "{dialog_input}",
                                    oninput: move |event| dialog_input.set(event.value()),
                                }
                            }
                            div {
                                style: "display:flex;justify-content:flex-end;gap:8px;",
                                button {
                                    class: "remote-files-button",
                                    onclick: move |_| pending_dialog.set(None),
                                    "Cancel"
                                }
                                button {
                                    class: "remote-files-button",
                                    style: if is_delete { "border-color:var(--skin-danger);color:var(--skin-danger);" } else { "border-color:var(--skin-accent);color:var(--skin-accent);" },
                                    onclick: move |_| {
                                        let Some(session_id) = selected_session.peek().clone() else { return; };
                                        let operation = dialog_for_submit.clone();
                                        let input = dialog_input.peek().clone();
                                        let directory = current_path.peek().clone();
                                        let (future, success_message): (
                                            std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>>,
                                            String,
                                        ) = match operation {
                                            PendingDialog::CreateDirectory => {
                                                let name = match validate_entry_name(&input) {
                                                    Ok(name) => name.to_string(),
                                                    Err(error) => {
                                                        action_error.set(Some(error.to_string()));
                                                        return;
                                                    }
                                                };
                                                let path = posix_join(&directory, &name);
                                                (
                                                    Box::pin(create_remote_directory(state, session_id, path)),
                                                    format!("Created directory: {name}"),
                                                )
                                            }
                                            PendingDialog::Rename(entry) => {
                                                let name = match validate_entry_name(&input) {
                                                    Ok(name) => name.to_string(),
                                                    Err(error) => {
                                                        action_error.set(Some(error.to_string()));
                                                        return;
                                                    }
                                                };
                                                if name == entry.name {
                                                    pending_dialog.set(None);
                                                    return;
                                                }
                                                let new_path = posix_join(&directory, &name);
                                                (
                                                    Box::pin(rename_remote_entry(state, session_id, entry.path, new_path)),
                                                    format!("Renamed to: {name}"),
                                                )
                                            }
                                            PendingDialog::Delete(entry) => {
                                                let name = entry.name.clone();
                                                (
                                                    Box::pin(delete_remote_entry(state, session_id, entry)),
                                                    format!("Deleted: {name}"),
                                                )
                                            }
                                        };
                                        pending_dialog.set(None);
                                        action_busy.set(true);
                                        action_error.set(None);
                                        status.set(Some("Applying remote operation…".to_string()));
                                        spawn(async move {
                                            match future.await {
                                                Ok(()) => {
                                                    status.set(Some(success_message));
                                                    selected_entry.set(None);
                                                    refresh_epoch.set(refresh_epoch().wrapping_add(1));
                                                }
                                                Err(error) => {
                                                    status.set(None);
                                                    action_error.set(Some(error));
                                                }
                                            }
                                            action_busy.set(false);
                                        });
                                    },
                                    if is_delete { "Delete" } else { "Apply" }
                                }
                            }
                        }
                    }
                }}
            }

            if !embedded && resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:col-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_x, start_width)) = resize_drag() else { return; };
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
                class: if resize_drag().is_some() { "remote-files-resize-handle active" } else { "remote-files-resize-handle" },
                style: "position:absolute;left:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;transition:background .1s;",
                title: "Drag to resize remote files panel",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        event.stop_propagation();
                        resize_drag.set(Some((event.client_coordinates().x, live_width())));
                    }
                },
            }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{posix_join, posix_parent};

    #[test]
    fn joins_posix_paths_without_platform_separators() {
        assert_eq!(posix_join("/", "etc"), "/etc");
        assert_eq!(
            posix_join("/home/user/", "notes.txt"),
            "/home/user/notes.txt"
        );
        assert_eq!(posix_join("/home/user", ""), "/home/user");
    }

    #[test]
    fn finds_posix_parent_and_stops_at_root() {
        assert_eq!(posix_parent("/home/user/"), Some("/home".to_string()));
        assert_eq!(posix_parent("/home"), Some("/".to_string()));
        assert_eq!(posix_parent("/"), None);
        assert_eq!(posix_parent(""), None);
    }
}
