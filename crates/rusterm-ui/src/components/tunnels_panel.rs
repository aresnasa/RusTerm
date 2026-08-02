//! Tunnel manager modal (feature #63): lists every configured tunnel with
//! a live status dot, start/stop/remove buttons, and an add/edit form with
//! port conflict detection + suggestions.

use std::net::IpAddr;
use std::sync::Arc;

use dioxus::prelude::*;
use rusterm_core::config::ConnectionKind;
use rusterm_tunnel::{TunnelConfig, TunnelKind, TunnelManager, TunnelSnapshot, TunnelState};

use crate::components::{Icon, IconName};

/// Form state for the add/edit tunnel editor. Strings everywhere — parsing
/// happens on save so we can surface validation messages.
#[derive(Debug, Clone, Default, PartialEq)]
struct TunnelForm {
    id: String,
    name: String,
    connection_id: String,
    kind: String, // "local" | "socks"
    listen_addr: String,
    listen_port: String,
    remote_host: String,
    remote_port: String,
    auto_start: bool,
    auto_reconnect: bool,
}

impl TunnelForm {
    fn from_config(config: &TunnelConfig) -> Self {
        let (kind, remote_host, remote_port) = match &config.kind {
            TunnelKind::LocalForward {
                remote_host,
                remote_port,
            } => (
                "local".to_string(),
                remote_host.clone(),
                remote_port.to_string(),
            ),
            TunnelKind::DynamicSocks => ("socks".to_string(), String::new(), String::new()),
            TunnelKind::Remote { .. } => ("local".to_string(), String::new(), String::new()),
        };
        Self {
            id: config.id.clone(),
            name: config.name.clone(),
            connection_id: config.connection_id.clone(),
            kind,
            listen_addr: config.listen_addr.to_string(),
            listen_port: config.listen_port.to_string(),
            remote_host,
            remote_port,
            auto_start: config.auto_start,
            auto_reconnect: config.auto_reconnect,
        }
    }
}

#[component]
pub fn TunnelsPanel(state: Signal<crate::state::AppState>, on_close: EventHandler<()>) -> Element {
    let manager: Option<Arc<TunnelManager>> = state.read().tunnel_manager.clone();
    let mut snapshots = use_signal(Vec::<TunnelSnapshot>::new);
    let mut form = use_signal(TunnelForm::default);
    let mut editing = use_signal(|| false);
    let mut form_error = use_signal(String::new);
    let mut port_hints = use_signal(Vec::<u16>::new);
    let mut port_free = use_signal(|| true);

    // Live refresh: rebuild the snapshot list every second. Cheap — a
    // handful of rows.
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let mgr = state.read().tunnel_manager.clone();
            if let Some(mgr) = mgr {
                snapshots.set(mgr.list());
            }
        }
    });

    // Seed once at mount too.
    let manager_for_effect = manager.clone();
    use_effect(move || {
        if let Some(mgr) = &manager_for_effect {
            snapshots.set(mgr.list());
        }
    });

    let Some(manager) = manager else {
        return rsx! {
            div { class: "tunnels-overlay",
                div { class: "tunnels-card",
                    p { "Tunnel manager is not initialized (unlock the app first)." }
                    button { onclick: move |_| on_close.call(()), "Close" }
                }
            }
        };
    };

    // Connection options for the dropdown (SSH connections only).
    let connections = state.read().connections.clone();
    let ssh_connections: Vec<(String, String)> = connections
        .iter()
        .filter(|c| matches!(c.kind, ConnectionKind::Ssh(_)))
        .map(|c| {
            let label = match &c.kind {
                ConnectionKind::Ssh(ssh) => {
                    format!("{} ({}@{}:{})", c.name, ssh.username, ssh.host, ssh.port)
                }
                _ => c.name.clone(),
            };
            (c.id.clone(), label)
        })
        .collect();

    let manager_for_form = manager.clone();
    let manager_for_list = manager;

    rsx! {
        style { r#"
            .tunnels-overlay{{position:fixed;inset:0;background:rgba(0,0,0,0.6);display:flex;align-items:center;justify-content:center;z-index:1000;}}
            .tunnels-card{{width:min(860px,92vw);max-height:86vh;background:#24283b;color:#c0caf5;border:1px solid #2a2b3d;border-radius:8px;display:flex;flex-direction:column;overflow:hidden;box-shadow:0 12px 40px rgba(0,0,0,.35);}}
            .tunnels-head{{display:flex;align-items:center;justify-content:space-between;padding:12px 16px;border-bottom:1px solid #2a2b3d;}}
            .tunnels-body{{display:flex;min-height:0;flex:1;}}
            .tunnels-list{{flex:1;min-width:0;overflow-y:auto;padding:8px 12px;border-right:1px solid #2a2b3d;}}
            .tunnels-editor{{width:300px;flex:0 0 auto;overflow-y:auto;padding:12px;background:#1a1b26;}}
            .tunnel-row{{display:flex;align-items:center;gap:8px;padding:8px 10px;border:1px solid #2a2b3d;border-radius:6px;margin-bottom:6px;background:#1a1b26;}}
            .tunnel-dot{{width:9px;height:9px;border-radius:50%;flex:0 0 auto;}}
            .tunnel-dot.green{{background:#4caf50;box-shadow:0 0 6px #4caf50;}}
            .tunnel-dot.yellow{{background:#ffb300;box-shadow:0 0 6px #ffb300;}}
            .tunnel-dot.red{{background:#e53935;}}
            .tunnel-name{{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;}}
            .tunnel-sub{{font-size:10px;color:#9aa5ce;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}}
            .tunnel-btn{{border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:11px;padding:3px 8px;cursor:pointer;}}
            .tunnel-btn:hover{{border-color:#7aa2f7;color:#7aa2f7;}}
            .tunnel-btn.danger:hover{{border-color:#f7768e;color:#f7768e;}}
            .tunnel-btn.primary{{background:#7aa2f7;color:#1a1b26;border:1px solid #7aa2f7;font-weight:600;}}
            .tunnel-field{{display:flex;flex-direction:column;gap:3px;margin-bottom:9px;font-size:11px;}}
            .tunnel-field input,.tunnel-field select{{padding:5px 7px;border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:12px;}}
            .tunnel-hints{{display:flex;gap:4px;flex-wrap:wrap;margin-top:3px;}}
            .tunnel-hint-chip{{border:1px solid #7aa2f7;color:#7aa2f7;border-radius:10px;padding:1px 8px;font-size:10px;cursor:pointer;background:transparent;}}
            .tunnel-error{{color:#f7768e;font-size:11px;margin-top:4px;}}
            .tunnel-check{{display:flex;align-items:center;gap:6px;font-size:11px;margin-bottom:9px;}}
        "# }

        div { class: "tunnels-overlay", onclick: move |_| on_close.call(()),
            div { class: "tunnels-card", onclick: move |e| e.stop_propagation(),
                div { class: "tunnels-head",
                    span { style: "font-size:16px;font-weight:600;", "SSH Tunnels" }
                    button {
                        class: "tunnel-btn",
                        onclick: move |_| {
                            editing.set(true);
                            form_error.set(String::new());
                            port_hints.set(Vec::new());
                            port_free.set(true);
                            let mut f = TunnelForm::default();
                            f.kind = "local".into();
                            f.listen_addr = "127.0.0.1".into();
                            f.listen_port = "1080".into();
                            if let Some((id, _)) = ssh_connections.first() {
                                f.connection_id = id.clone();
                            }
                            form.set(f);
                        },
                        "+ New tunnel"
                    }
                }

                div { class: "tunnels-body",
                    // ── list ────────────────────────────────────────────
                    div { class: "tunnels-list",
                        if snapshots().is_empty() {
                            div { style: "padding:32px;text-align:center;color:#9aa5ce;font-size:12px;",
                                "No tunnels yet. Create one to forward a local port through SSH."
                            }
                        } else {
                            for snap in snapshots().iter() {
                                {
                                    let (dot_class, status_text) = describe_state(&snap.state);
                                    let running = matches!(
                                        snap.state,
                                        TunnelState::Active { .. }
                                            | TunnelState::Connecting { .. }
                                            | TunnelState::Reconnecting { .. }
                                    );
                                    let id_start = snap.config.id.clone();
                                    let id_stop = snap.config.id.clone();
                                    let id_remove = snap.config.id.clone();
                                    let cfg_edit = snap.config.clone();
                                    let mgr_start = manager_for_list.clone();
                                    let mgr_stop = manager_for_list.clone();
                                    let mgr_remove = manager_for_list.clone();
                                    rsx! {
                                        div { class: "tunnel-row", key: "{snap.config.id}",
                                            div { class: "tunnel-dot {dot_class}" }
                                            div { style: "flex:1;min-width:0;",
                                                div { class: "tunnel-name", "{snap.config.name}" }
                                                div { class: "tunnel-sub",
                                                    "{describe_kind(&snap.config.kind)} · :{snap.config.listen_port} · {status_text}"
                                                }
                                            }
                                            if running {
                                                button {
                                                    class: "tunnel-btn",
                                                    onclick: move |_| { mgr_stop.stop(&id_stop).ok(); },
                                                    "Stop"
                                                }
                                            } else {
                                                button {
                                                    class: "tunnel-btn",
                                                    onclick: move |_| { mgr_start.start(&id_start).ok(); },
                                                    "Start"
                                                }
                                            }
                                            button {
                                                class: "tunnel-btn",
                                                onclick: move |_| {
                                                    form.set(TunnelForm::from_config(&cfg_edit));
                                                    editing.set(true);
                                                    form_error.set(String::new());
                                                },
                                                "Edit"
                                            }
                                            button {
                                                class: "tunnel-btn danger",
                                                onclick: move |_| { mgr_remove.remove(&id_remove).ok(); },
                                                Icon { name: IconName::Delete, size: 12 }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // ── editor ─────────────────────────────────────────
                    if editing() {
                        div { class: "tunnels-editor",
                            div { style: "font-size:12px;font-weight:600;margin-bottom:10px;",
                                if form().id.is_empty() { "New tunnel" } else { "Edit tunnel" }
                            }

                            div { class: "tunnel-field",
                                span { "Name" }
                                input {
                                    value: "{form().name}",
                                    oninput: move |e| form.write().name = e.value(),
                                }
                            }
                            div { class: "tunnel-field",
                                span { "SSH connection" }
                                select {
                                    value: "{form().connection_id}",
                                    onchange: move |e| form.write().connection_id = e.value(),
                                    for (id, label) in ssh_connections.iter() {
                                        option { value: "{id}", "{label}" }
                                    }
                                }
                            }
                            div { class: "tunnel-field",
                                span { "Type" }
                                select {
                                    value: "{form().kind}",
                                    onchange: move |e| form.write().kind = e.value(),
                                    option { value: "local", "Local forward (ssh -L)" }
                                    option { value: "socks", "Dynamic SOCKS5 proxy (ssh -D)" }
                                }
                            }
                            if form().kind == "local" {
                                div { class: "tunnel-field",
                                    span { "Remote host : port" }
                                    div { style: "display:flex;gap:6px;",
                                        input {
                                            style: "flex:1;",
                                            placeholder: "127.0.0.1",
                                            value: "{form().remote_host}",
                                            oninput: move |e| form.write().remote_host = e.value(),
                                        }
                                        input {
                                            style: "width:70px;",
                                            placeholder: "5432",
                                            value: "{form().remote_port}",
                                            oninput: move |e| form.write().remote_port = e.value(),
                                        }
                                    }
                                }
                            }
                            div { class: "tunnel-field",
                                span { "Listen : port" }
                                div { style: "display:flex;gap:6px;",
                                    input {
                                        style: "flex:1;",
                                        placeholder: "127.0.0.1",
                                        value: "{form().listen_addr}",
                                        oninput: move |e| form.write().listen_addr = e.value(),
                                    }
                                    input {
                                        style: "width:70px;",
                                        placeholder: "1080",
                                        value: "{form().listen_port}",
                                        oninput: move |e| form.write().listen_port = e.value(),
                                    }
                                }
                                button {
                                    class: "tunnel-btn",
                                    style: "margin-top:4px;align-self:flex-start;",
                                    onclick: move |_| {
                                        let f = form.read();
                                        let addr: IpAddr = f.listen_addr.parse().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
                                        let wanted: u16 = f.listen_port.parse().unwrap_or(0);
                                        let free = rusterm_tunnel::check_port_available(addr, wanted);
                                        port_free.set(free);
                                        if !free {
                                            let suggestions = rusterm_tunnel::suggest_listen_ports(addr, wanted, 5);
                                            port_hints.set(suggestions);
                                        } else {
                                            port_hints.set(Vec::new());
                                        }
                                    },
                                    if port_free() { "Check port" } else { "Port busy — suggest free ports" }
                                }
                                if !port_free() {
                                    div { class: "tunnel-error", "Port is in use." }
                                    if !port_hints().is_empty() {
                                        div { class: "tunnel-hints",
                                            for port in port_hints().clone() {
                                                button {
                                                    class: "tunnel-hint-chip",
                                                    onclick: move |_| {
                                                        form.write().listen_port = port.to_string();
                                                        port_free.set(true);
                                                        port_hints.set(Vec::new());
                                                    },
                                                    "{port}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            label { class: "tunnel-check",
                                input {
                                    r#type: "checkbox",
                                    checked: form().auto_start,
                                    onchange: move |e| form.write().auto_start = e.checked(),
                                }
                                "Auto-start on app launch"
                            }
                            label { class: "tunnel-check",
                                input {
                                    r#type: "checkbox",
                                    checked: form().auto_reconnect,
                                    onchange: move |e| form.write().auto_reconnect = e.checked(),
                                }
                                "Auto-reconnect with backoff"
                            }

                            if !form_error().is_empty() {
                                div { class: "tunnel-error", "{form_error()}" }
                            }

                            div { style: "display:flex;gap:8px;margin-top:6px;",
                                button {
                                    class: "tunnel-btn primary",
                                    onclick: move |_| {
                                        match build_config(&form()) {
                                            Ok(config) => {
                                                let id = config.id.clone();
                                                manager_for_form.upsert(config);
                                                snapshots.set(manager_for_form.list());
                                                let _ = manager_for_form.start(&id);
                                                editing.set(false);
                                                form.set(TunnelForm::default());
                                            }
                                            Err(e) => form_error.set(e),
                                        }
                                    },
                                    "Save & start"
                                }
                                button {
                                    class: "tunnel-btn",
                                    onclick: move |_| {
                                        editing.set(false);
                                        form_error.set(String::new());
                                        form.set(TunnelForm::default());
                                    },
                                    "Cancel"
                                }
                            }
                        }
                    }
                }

                div { style: "display:flex;justify-content:flex-end;padding:8px 16px;border-top:1px solid #2a2b3d;",
                    button { class: "tunnel-btn", onclick: move |_| on_close.call(()), "Close" }
                }
            }
        }
    }
}

fn describe_state(state: &TunnelState) -> (&'static str, String) {
    match state {
        TunnelState::Stopped => ("red", "Stopped".to_string()),
        TunnelState::Connecting { attempt } => {
            ("yellow", format!("Connecting (attempt {attempt})"))
        }
        TunnelState::Active { since_epoch_secs } => {
            let up_secs = (chrono::Utc::now().timestamp() - since_epoch_secs).max(0);
            (
                "green",
                format!("Active {}m {}s", up_secs / 60, up_secs % 60),
            )
        }
        TunnelState::Reconnecting {
            attempt,
            next_retry_ms,
            last_error,
        } => (
            "yellow",
            format!(
                "Reconnecting attempt {attempt} in {}ms ({})",
                next_retry_ms, last_error
            ),
        ),
        TunnelState::Failed(msg) => ("red", format!("Failed: {msg}")),
    }
}

fn describe_kind(kind: &TunnelKind) -> &'static str {
    match kind {
        TunnelKind::LocalForward { .. } => "-L",
        TunnelKind::DynamicSocks => "-D",
        TunnelKind::Remote { .. } => "-R",
    }
}

/// Validate + assemble a `TunnelConfig` from the form strings.
fn build_config(form: &TunnelForm) -> Result<TunnelConfig, String> {
    if form.name.trim().is_empty() {
        return Err("Name is required".into());
    }
    if form.connection_id.is_empty() {
        return Err("Pick an SSH connection".into());
    }
    let listen_addr: IpAddr = form
        .listen_addr
        .parse()
        .map_err(|_| "Listen address must be a valid IP".to_string())?;
    let listen_port: u16 = form
        .listen_port
        .parse()
        .map_err(|_| "Listen port must be 1-65535".to_string())?;
    if listen_port == 0 {
        return Err("Listen port cannot be 0".into());
    }
    let kind = match form.kind.as_str() {
        "local" => {
            let remote_host = form.remote_host.trim();
            if remote_host.is_empty() {
                return Err("Remote host is required for local forward".into());
            }
            let remote_port: u16 = form
                .remote_port
                .parse()
                .map_err(|_| "Remote port must be 1-65535".to_string())?;
            TunnelKind::LocalForward {
                remote_host: remote_host.to_string(),
                remote_port,
            }
        }
        "socks" => TunnelKind::DynamicSocks,
        other => return Err(format!("unknown kind {other}")),
    };

    let id = if form.id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        form.id.clone()
    };

    Ok(TunnelConfig {
        id,
        name: form.name.trim().to_string(),
        connection_id: form.connection_id.clone(),
        listen_addr,
        listen_port,
        kind,
        auto_start: form.auto_start,
        auto_reconnect: form.auto_reconnect,
    })
}
