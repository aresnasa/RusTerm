//! Bottom-dock REST API relay panel.
//!
//! A compact, always-available counterpart to the full [`crate::components::RelayPanel`]
//! modal. Lives in the bottom dock (the "API" tab) so the user can:
//!   1. Toggle / configure the relay (bind addr, port, accounts) without
//!      opening a modal.
//!   2. Auto-generate ready-to-paste `curl` commands that execute a chosen
//!      command **or multi-line script** on one or more connected SSH sessions
//!      through the relay, using HTTP BasicAuth through exported runtime
//!      environment variables.
//!
//! Each generated curl targets `POST /api/v1/exec` with a JSON body carrying
//! exactly one of `{ "command": ... }`, `{ "script": ... }`, or
//! `{ "script_base64": ... }` (mutually exclusive). Scripts pass through the
//! relay's hard-floor validator (`validate_script`), the dcg destructive-command
//! guard when installed, and a static sandbox pre-flight (`sh -n` + dcg) before
//! reaching the SSH executor.

use dioxus::prelude::*;
use rusterm_relay::{RelayAccount, hash_password};

use crate::state::AppState;

/// Which payload the curl builder should emit. Mirrors the three mutually
/// exclusive fields of the relay's `ExecRequest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CurlMode {
    /// `{"command": "..."}` — single-line, backward compatible.
    Command,
    /// `{"script": "..."}` — multi-line, validated by `validate_script`.
    Script,
    /// `{"script_base64": "..."}` — base64-encoded script.
    ScriptBase64,
}

/// The payload to embed in the generated curl's JSON body. The UI produces
/// one of these from the active [`CurlMode`]; the curl generator emits the
/// corresponding JSON field.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CurlPayload {
    Command(String),
    Script(String),
    ScriptBase64(String),
}

/// Build the base URL the relay is reachable at, given a running handle's
/// reported URL OR (when not yet started) the configured bind/port so the
/// curl preview can show the intended URL ahead of time.
fn base_url(running_url: &Option<String>, bind_addr: &str, port: u16) -> String {
    if let Some(url) = running_url {
        url.trim_end_matches('/').to_string()
    } else {
        // Normalize the bind addr for display: 0.0.0.0 → localhost-friendly
        // hint (the user still needs network reachability, but for a local
        // curl preview 127.0.0.1 is what works).
        let host = if bind_addr == "0.0.0.0" || bind_addr == "::" {
            "127.0.0.1"
        } else {
            bind_addr
        };
        format!("http://{host}:{port}")
    }
}

#[component]
pub fn ApiPanel(state: Signal<AppState>) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    // Local working copy of the relay config; persisted on Save / Start.
    let mut config = use_signal(|| state.read().relay_config.clone());
    let mut running = use_signal(|| state.read().relay_runtime.is_running());
    let mut started_url = use_signal(|| {
        state
            .read()
            .relay_runtime
            .0
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|h| h.url()))
    });
    let mut status_msg = use_signal(String::new);

    // New-account scratch form.
    let mut new_username = use_signal(String::new);
    let mut new_password = use_signal(String::new);
    let mut new_readonly = use_signal(|| false);
    let mut account_error = use_signal(String::new);

    // curl-builder scratch state.
    let mut curl_command = use_signal(|| "uname -a".to_string());
    let mut curl_script = use_signal(String::new);
    let mut curl_script_base64 = use_signal(String::new);
    let mut curl_mode = use_signal(|| CurlMode::Command);
    let mut curl_elevated = use_signal(|| true);
    let mut copied = use_signal(|| false);

    let (enabled, bind_addr_str, port_str) = {
        let cfg = config.read();
        (cfg.enabled, cfg.bind_addr.to_string(), cfg.port.to_string())
    };
    let runtime = state.read().relay_runtime.clone();
    let url = base_url(&started_url(), &bind_addr_str, config.read().port);
    // Clone for the copy-button move closure (which would otherwise move `url`
    // out of the render scope before the endpoint reference can read it).
    let url_for_copy = url.clone();

    // Connected SSH sessions → (saved connection id, label) pairs for the
    // curl builder. IDs are unambiguous and are also the key used by the
    // host-bound sudo credential lease; connection names may be duplicated.
    let sessions: Vec<(String, String)> = {
        let app = state.read();
        app.sessions
            .iter()
            .filter(|tab| tab.kind == rusterm_core::session::SessionType::Ssh)
            .map(|tab| {
                let label = if let Some(host) = &tab.hostname {
                    format!("{} ({})", tab.name, host)
                } else {
                    tab.name.clone()
                };
                let host_id = app
                    .session_configs
                    .get(&tab.id)
                    .map(|config| config.id.clone())
                    .unwrap_or_else(|| tab.name.clone());
                (host_id, label)
            })
            .collect()
    };
    // Default to one target while allowing any number of connected sessions.
    // Drop stale IDs when a selected session disconnects.
    let mut selected_sessions = use_signal(|| {
        sessions
            .first()
            .map(|(id, _)| vec![id.clone()])
            .unwrap_or_default()
    });
    let current_selection = selected_sessions();
    let valid_selection: Vec<String> = current_selection
        .iter()
        .filter(|selected| sessions.iter().any(|(id, _)| id == *selected))
        .cloned()
        .collect();
    if valid_selection != current_selection {
        selected_sessions.set(valid_selection);
    }
    let all_session_ids: Vec<String> = sessions.iter().map(|(id, _)| id.clone()).collect();

    let default_user = config
        .read()
        .accounts
        .first()
        .map(|a| a.username.clone())
        .unwrap_or_else(|| "USER".to_string());

    rsx! {
        style { r#"
            .api-panel{{display:flex;min-height:0;flex:1;font-size:12px;color:#c0caf5;background:#1a1b26;}}
            .api-col{{display:flex;flex-direction:column;min-height:0;overflow-y:auto;padding:10px 12px;}}
            .api-col.left{{flex:0 0 280px;border-right:1px solid #2a2b3d;background:#24283b;}}
            .api-col.right{{flex:1;min-width:0;}}
            .api-sect{{font-size:11px;font-weight:600;color:#9aa5ce;text-transform:uppercase;letter-spacing:.5px;margin:10px 0 6px;}}
            .api-sect:first-child{{margin-top:0;}}
            .api-field{{display:flex;flex-direction:column;gap:3px;margin-bottom:8px;}}
            .api-field > span{{font-size:11px;color:#9aa5ce;}}
            .api-input{{padding:5px 7px;border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:12px;outline:none;box-sizing:border-box;width:100%;}}
            .api-session-picker{{flex:1;min-width:0;border:1px solid #2a2b3d;border-radius:5px;background:#16161e;overflow:hidden;}}
            .api-session-toolbar{{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:5px 7px;border-bottom:1px solid #2a2b3d;color:#9aa5ce;font-size:11px;}}
            .api-session-actions{{display:flex;gap:4px;}}
            .api-session-list{{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));max-height:128px;overflow-y:auto;padding:4px;gap:2px;}}
            .api-session-option{{display:flex;align-items:center;gap:7px;min-width:0;padding:5px 6px;border-radius:4px;cursor:pointer;}}
            .api-session-option:hover{{background:#24283b;}}
            .api-session-option span{{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}}
            .api-command-field{{padding:8px;border:1px solid rgba(224,175,104,.55);border-radius:5px;background:rgba(224,175,104,.08);}}
            .api-command-field > span{{color:#e0af68;font-weight:600;}}
            .api-command-field .api-input{{border-color:#e0af68;box-shadow:0 0 0 1px rgba(224,175,104,.18);}}
            .api-command-edit-hint{{font-size:11px;color:#e0af68;line-height:1.4;}}
            .api-row{{display:flex;align-items:center;gap:8px;margin-bottom:8px;}}
            .api-btn{{border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:11px;padding:4px 10px;cursor:pointer;}}
            .api-btn:hover{{border-color:#7aa2f7;color:#7aa2f7;}}
            .api-btn.primary{{background:#7aa2f7;color:#1a1b26;border:1px solid #7aa2f7;font-weight:600;}}
            .api-btn.primary:hover{{color:#1a1b26;opacity:.9;}}
            .api-btn:disabled{{cursor:not-allowed;opacity:.45;}}
            .api-btn.danger:hover{{border-color:#f7768e;color:#f7768e;}}
            .api-status{{font-size:11px;padding:5px 9px;border-radius:4px;margin-bottom:8px;}}
            .api-status.ok{{background:rgba(76,175,80,.12);color:#9ece6a;}}
            .api-status.err{{background:rgba(247,118,142,.12);color:#f7768e;}}
            .api-status.idle{{background:#24283b;color:#9aa5ce;}}
            .api-account{{border:1px solid #2a2b3d;border-radius:4px;padding:6px 8px;margin-bottom:5px;background:#1a1b26;}}
            .api-code{{font-family:'JetBrains Mono','Fira Code',ui-monospace,monospace;font-size:11px;background:#16161e;border:1px solid #2a2b3d;border-radius:4px;padding:8px 10px;color:#9ece6a;white-space:pre-wrap;word-break:break-all;}}
            .api-command-highlight{{display:inline;padding:1px 3px;border-radius:3px;background:#e0af68;color:#16161e;font-weight:700;box-shadow:0 0 0 1px rgba(255,158,100,.75),0 0 8px rgba(224,175,104,.45);}}
            .api-hint{{font-size:11px;color:#9aa5ce;line-height:1.5;margin:4px 0 8px;}}
        "# }

        div { class: "api-panel",
            // ── Left column: server config + accounts ───────────────────
            div { class: "api-col left",
                div { class: "api-sect", { crate::i18n::t("api.title") } }

                if running() {
                    div { class: "api-status ok",
                        { crate::i18n::tf("api.status_running", &[("url", &url)]) }
                    }
                } else {
                    div { class: "api-status idle", { crate::i18n::t("api.status_stopped") } }
                }
                if !status_msg().is_empty() {
                    div { class: "api-status err", "{status_msg()}" }
                }

                div { class: "api-row",
                    label {
                        style: "display:flex;align-items:center;gap:6px;font-size:12px;cursor:pointer;",
                        input {
                            r#type: "checkbox",
                            checked: enabled,
                            onchange: move |e| config.write().enabled = e.checked(),
                        }
                        { crate::i18n::t("api.enable_on_startup") }
                    }
                }
                div { class: "api-row",
                    span { style: "width:64px;font-size:11px;color:#9aa5ce;", { crate::i18n::t("api.bind_addr") } }
                    input {
                        class: "api-input",
                        style: "width:120px;",
                        value: "{bind_addr_str}",
                        oninput: move |e| {
                            if let Ok(ip) = e.value().parse::<std::net::IpAddr>() {
                                config.write().bind_addr = ip;
                            }
                        },
                    }
                    span { style: "font-size:11px;color:#9aa5ce;", { crate::i18n::t("api.port") } }
                    input {
                        class: "api-input",
                        style: "width:64px;",
                        value: "{port_str}",
                        oninput: move |e| {
                            if let Ok(p) = e.value().parse::<u16>() {
                                config.write().port = p;
                                status_msg.set(String::new());
                            }
                        },
                    }
                }
                div { class: "api-row",
                    if running() {
                        button {
                            class: "api-btn",
                            onclick: move |_| {
                                let cfg = config();
                                if let Err(e) = cfg.save() {
                                    status_msg.set(e.to_string());
                                } else {
                                    state.write().relay_config = cfg.clone();
                                    crate::relay_tunnel::stop_relay(runtime.clone());
                                    running.set(false);
                                    started_url.set(None);
                                }
                            },
                            { crate::i18n::t("api.stop") }
                        }
                    } else {
                        button {
                            class: "api-btn primary",
                            onclick: move |_| {
                                let cfg = config();
                                if let Err(e) = cfg.save() {
                                    status_msg.set(e.to_string());
                                    return;
                                }
                                state.write().relay_config = cfg.clone();
                                match crate::relay_tunnel::start_relay(cfg, runtime.clone()) {
                                    Ok(()) => {
                                        running.set(true);
                                        started_url.set(
                                            state.read().relay_runtime.0.read().ok()
                                                .and_then(|g| g.as_ref().map(|h| h.url()))
                                        );
                                        status_msg.set(String::new());
                                    }
                                    Err(e) => status_msg.set(e),
                                }
                            },
                            { crate::i18n::t("api.start") }
                        }
                    }
                    button {
                        class: "api-btn",
                        onclick: move |_| {
                            let cfg = config();
                            match cfg.save() {
                                Ok(()) => {
                                    state.write().relay_config = cfg;
                                    status_msg.set(crate::i18n::t("api.saved"));
                                }
                                Err(e) => status_msg.set(e.to_string()),
                            }
                        },
                        { crate::i18n::t("common.save") }
                    }
                }

                div { class: "api-sect", { crate::i18n::t("api.accounts") } }
                if config.read().accounts.is_empty() {
                    div { class: "api-hint", { crate::i18n::t("api.no_account") } }
                }
                for account in config.read().accounts.iter().cloned() {
                    {
                        let username = account.username.clone();
                        let username_for_remove = username.clone();
                        rsx! {
                            div { class: "api-account", key: "{username}",
                                div { style: "display:flex;align-items:center;justify-content:space-between;gap:8px;",
                                    span { style: "font-weight:600;font-size:12px;", "{username}" }
                                    div { style: "display:flex;align-items:center;gap:6px;",
                                        if account.readonly {
                                            span { style: "font-size:10px;color:#7aa2f7;", { crate::i18n::t("api.readonly") } }
                                        }
                                        button {
                                            class: "api-btn danger",
                                            onclick: move |_| {
                                                config.write().accounts.retain(|a| a.username != username_for_remove);
                                            },
                                            { crate::i18n::t("api.remove") }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div { class: "api-field",
                    span { { crate::i18n::t("api.username") } }
                    input {
                        class: "api-input",
                        value: "{new_username()}",
                        oninput: move |e| new_username.set(e.value()),
                    }
                }
                div { class: "api-field",
                    span { { crate::i18n::t("api.password") } }
                    input {
                        r#type: "password",
                        class: "api-input",
                        value: "{new_password()}",
                        oninput: move |e| new_password.set(e.value()),
                    }
                }
                label {
                    style: "display:flex;align-items:center;gap:6px;font-size:11px;color:#9aa5ce;margin-bottom:8px;cursor:pointer;",
                    input {
                        r#type: "checkbox",
                        checked: new_readonly(),
                        onchange: move |e| new_readonly.set(e.checked()),
                    }
                    { crate::i18n::t("api.readonly") }
                }
                if !account_error().is_empty() {
                    div { class: "api-status err", "{account_error()}" }
                }
                button {
                    class: "api-btn primary",
                    onclick: move |_| {
                        let user = new_username().trim().to_string();
                        let pass = new_password();
                        if user.is_empty() || pass.is_empty() {
                            account_error.set(crate::i18n::t("api.fill_user_pass"));
                            return;
                        }
                        if config.read().accounts.iter().any(|a| a.username == user) {
                            account_error.set(crate::i18n::tf("api.account_exists", &[("name", &user)]));
                            return;
                        }
                        match hash_password(&pass) {
                            Ok(hash) => {
                                config.write().accounts.push(RelayAccount {
                                    username: user,
                                    password_hash: hash,
                                    allowed_hosts: vec![],
                                    allowed_commands: vec![],
                                    readonly: new_readonly(),
                                });
                                new_username.set(String::new());
                                new_password.set(String::new());
                                new_readonly.set(false);
                                account_error.set(String::new());
                            }
                            Err(e) => account_error.set(e.to_string()),
                        }
                    },
                    { crate::i18n::t("api.add_account") }
                }
            }

            // ── Right column: curl examples ─────────────────────────────
            div { class: "api-col right",
                div { class: "api-sect", { crate::i18n::t("api.curl_examples") } }
                div { class: "api-hint", { crate::i18n::t("api.curl_hint") } }

                if sessions.is_empty() {
                    div { class: "api-hint", style: "color:#e0af68;", { crate::i18n::t("api.no_sessions") } }
                } else {
                    div { class: "api-row", style: "align-items:flex-start;",
                        span { style: "width:64px;padding-top:7px;font-size:11px;color:#9aa5ce;", { crate::i18n::t("api.sessions") } }
                        div { class: "api-session-picker",
                            div { class: "api-session-toolbar",
                                span {
                                    { crate::i18n::tf(
                                        "api.selected_count",
                                        &[("count", &selected_sessions().len().to_string())],
                                    ) }
                                }
                                div { class: "api-session-actions",
                                    button {
                                        class: "api-btn",
                                        r#type: "button",
                                        onclick: move |_| selected_sessions.set(all_session_ids.clone()),
                                        { crate::i18n::t("api.select_all") }
                                    }
                                    button {
                                        class: "api-btn",
                                        r#type: "button",
                                        onclick: move |_| selected_sessions.set(Vec::new()),
                                        { crate::i18n::t("api.clear_selection") }
                                    }
                                }
                            }
                            div { class: "api-session-list",
                                for (id, label) in sessions.iter() {
                                    {
                                        let id_for_toggle = id.clone();
                                        let checked = selected_sessions().contains(id);
                                        rsx! {
                                            label { class: "api-session-option", key: "{id}", title: "{label}",
                                                input {
                                                    r#type: "checkbox",
                                                    checked,
                                                    onchange: move |e| {
                                                        let mut selected = selected_sessions.write();
                                                        if e.checked() {
                                                            if !selected.contains(&id_for_toggle) {
                                                                selected.push(id_for_toggle.clone());
                                                            }
                                                        } else {
                                                            selected.retain(|id| id != &id_for_toggle);
                                                        }
                                                    },
                                                }
                                                span { "{label}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if selected_sessions().is_empty() {
                        div { class: "api-hint", style: "color:#e0af68;", { crate::i18n::t("api.select_session_hint") } }
                    }
                    // Mode toggle: Command | Script | Script (base64). Drives
                    // which JSON field the generated curl sends and which
                    // input element is shown below.
                    div { class: "api-row", style: "gap:6px;margin-bottom:8px;",
                        button {
                            class: "api-btn",
                            style: if curl_mode() == CurlMode::Command { "font-weight:bold;" } else { "" },
                            onclick: move |_| curl_mode.set(CurlMode::Command),
                            { crate::i18n::t("api.mode_command") }
                        }
                        button {
                            class: "api-btn",
                            style: if curl_mode() == CurlMode::Script { "font-weight:bold;" } else { "" },
                            onclick: move |_| curl_mode.set(CurlMode::Script),
                            { crate::i18n::t("api.mode_script") }
                        }
                        button {
                            class: "api-btn",
                            style: if curl_mode() == CurlMode::ScriptBase64 { "font-weight:bold;" } else { "" },
                            onclick: move |_| curl_mode.set(CurlMode::ScriptBase64),
                            { crate::i18n::t("api.mode_script_base64") }
                        }
                    }
                    div { class: "api-field api-command-field",
                        span {
                            {
                                match curl_mode() {
                                    CurlMode::Command => crate::i18n::t("api.command"),
                                    CurlMode::Script => crate::i18n::t("api.script_label"),
                                    CurlMode::ScriptBase64 => crate::i18n::t("api.script_base64_label"),
                                }
                            }
                        }
                        div { class: "api-command-edit-hint",
                            {
                                match curl_mode() {
                                    CurlMode::Command => crate::i18n::t("api.command_edit_hint"),
                                    CurlMode::Script => crate::i18n::t("api.script_edit_hint"),
                                    CurlMode::ScriptBase64 => crate::i18n::t("api.script_base64_edit_hint"),
                                }
                            }
                        }
                        // Single-line input for Command mode; multi-line
                        // textarea for Script and ScriptBase64. The textarea
                        // is wide enough to read a typical script without
                        // horizontal scrolling, but the JSON body still
                        // serialises newlines as `\n` so the curl --data
                        // argument stays a single shell-quoted string.
                        match curl_mode() {
                            CurlMode::Command => rsx! {
                                input {
                                    class: "api-input",
                                    value: "{curl_command}",
                                    placeholder: "kubectl get pods",
                                    oninput: move |e| curl_command.set(e.value()),
                                }
                            },
                            CurlMode::Script => rsx! {
                                textarea {
                                    class: "api-input",
                                    style: "min-height:120px;font-family:monospace;resize:vertical;",
                                    value: "{curl_script}",
                                    placeholder: "#!/bin/sh\nset -e\necho hello\nuptime",
                                    oninput: move |e| curl_script.set(e.value()),
                                }
                            },
                            CurlMode::ScriptBase64 => rsx! {
                                textarea {
                                    class: "api-input",
                                    style: "min-height:80px;font-family:monospace;resize:vertical;",
                                    value: "{curl_script_base64}",
                                    placeholder: "IyEvYmluL3NoCmVjaG8gaGVsbG8K",
                                    oninput: move |e| curl_script_base64.set(e.value()),
                                }
                            },
                        }
                    }
                    label {
                        style: "display:flex;align-items:center;gap:6px;font-size:11px;color:#9aa5ce;margin-bottom:8px;cursor:pointer;",
                        input {
                            r#type: "checkbox",
                            checked: curl_elevated(),
                            onchange: move |e| curl_elevated.set(e.checked()),
                        }
                        { crate::i18n::t("api.elevated") }
                    }

                    {
                        // Build the payload from the active mode. Empty
                        // script/base64 falls back to an empty string so the
                        // curl preview still renders (the relay will reject
                        // it with `script_rejected`/`base64_invalid`).
                        let payload = match curl_mode() {
                            CurlMode::Command => CurlPayload::Command(curl_command()),
                            CurlMode::Script => CurlPayload::Script(curl_script()),
                            CurlMode::ScriptBase64 => CurlPayload::ScriptBase64(curl_script_base64()),
                        };
                        let preview = gen_curl_preview(
                            &url,
                            &default_user,
                            &selected_sessions(),
                            &payload,
                            curl_elevated(),
                        );
                        let CurlPreviewParts {
                            before_command,
                            command,
                            after_command,
                        } = preview;
                        rsx! {
                            div { class: "api-code",
                                {before_command}
                                span { class: "api-command-highlight", {command} }
                                {after_command}
                            }
                        }
                    }
                    div { class: "api-row", style: "margin-top:8px;",
                        button {
                            class: "api-btn primary",
                            disabled: selected_sessions().is_empty(),
                            // Clone into the move closure so `url` itself isn't
                            // moved out of the render scope.
                            onclick: move |_| {
                                let payload = match curl_mode() {
                                    CurlMode::Command => CurlPayload::Command(curl_command()),
                                    CurlMode::Script => CurlPayload::Script(curl_script()),
                                    CurlMode::ScriptBase64 => CurlPayload::ScriptBase64(curl_script_base64()),
                                };
                                let curl = gen_curl(
                                    &url_for_copy,
                                    &default_user,
                                    &selected_sessions(),
                                    &payload,
                                    curl_elevated(),
                                );
                                let _ = dioxus::document::eval(&format!(
                                    "navigator.clipboard.writeText({})",
                                    serde_json::to_string(&curl).unwrap_or_default()
                                ));
                                copied.set(true);
                                spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                                    copied.set(false);
                                });
                            },
                            if copied() { { crate::i18n::t("api.copied") } } else { { crate::i18n::t("api.copy") } }
                        }
                    }
                }

                // ── Endpoint reference ───────────────────────────────────
                div { class: "api-sect", { crate::i18n::t("api.endpoints") } }
                {
                    let endpoints = crate::i18n::tf(
                        "api.endpoint_reference",
                        &[("url", &url)],
                    );
                    rsx! {
                        div { class: "api-code", "{endpoints}" }
                    }
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CurlPreviewParts {
    before_command: String,
    command: String,
    after_command: String,
}

impl CurlPreviewParts {
    fn into_script(self) -> String {
        self.before_command + &self.command + &self.after_command
    }
}

/// Build the shell snippet as three text fragments so the editable payload
/// can be styled independently without injecting HTML or matching duplicate
/// text elsewhere in the script.
fn gen_curl_preview(
    url: &str,
    default_user: &str,
    sessions: &[String],
    payload: &CurlPayload,
    elevated: bool,
) -> CurlPreviewParts {
    gen_curl_preview_for_language(
        url,
        default_user,
        sessions,
        payload,
        elevated,
        crate::i18n::current_language(),
    )
}

fn gen_curl_preview_for_language(
    url: &str,
    default_user: &str,
    sessions: &[String],
    payload: &CurlPayload,
    elevated: bool,
    language: crate::i18n::Language,
) -> CurlPreviewParts {
    let hosts = if sessions.is_empty() {
        vec!["HOST".to_string()]
    } else {
        sessions.to_vec()
    };
    let shell_escape = |value: &str| value.replace('\'', "'\"'\"'");
    let shell_quote = |value: &str| format!("'{}'", shell_escape(value));
    let password_prompt = shell_escape(&crate::i18n::t_for("api.password_prompt", language));
    let password_not_tty = shell_escape(&crate::i18n::t_for("api.password_not_tty", language));
    let command_marker_title = crate::i18n::t_for("api.command_marker_title", language);
    let command_marker_help = crate::i18n::t_for("api.command_marker_help", language);
    let request_failed = shell_escape(&crate::i18n::t_for("api.request_failed", language));

    if let CurlPayload::Command(command) = payload {
        let function_usage = shell_escape(&crate::i18n::t_for("api.function_usage", language));
        let no_hosts = shell_escape(&crate::i18n::t_for("api.no_hosts", language));
        let missing_config = shell_escape(&crate::i18n::t_for("api.missing_config", language));
        let missing_user = shell_escape(&crate::i18n::t_for("api.missing_user", language));
        let elevated_query = if elevated { "?elevated=true" } else { "" };

        // Ordinary words can be pasted as `rusterm uname -a`. Shell syntax
        // such as `&&`, pipes, redirects, quotes, or repeated whitespace must
        // remain one argument so the local shell does not consume it first.
        let can_be_unquoted = !command.is_empty()
            && !command.starts_with(' ')
            && !command.ends_with(' ')
            && !command.contains("  ")
            && command.chars().all(|ch| {
                ch.is_alphanumeric()
                    || matches!(
                        ch,
                        ' ' | '-' | '_' | '.' | '/' | ':' | '=' | '@' | '%' | '+' | ','
                    )
            });
        let (invocation_before, rendered_command, invocation_after) = if command.is_empty() {
            (
                "# rusterm <command...>".to_string(),
                String::new(),
                "\n".to_string(),
            )
        } else if can_be_unquoted {
            ("rusterm ".to_string(), command.clone(), "\n".to_string())
        } else {
            (
                "rusterm '".to_string(),
                shell_escape(command),
                "'\n".to_string(),
            )
        };

        // Host ids are NOT baked into the copied function. At runtime the
        // function discovers the current hosts via `GET /r` (plain text, one
        // id per line) so the snippet keeps working after sessions reconnect
        // or the selected host changes. `RUSTERM_HOSTS` may be pre-exported to
        // pin a target; `--refresh` forces re-discovery.
        return CurlPreviewParts {
            before_command: format!(
                "export RUSTERM_API_URL={url}\n\
export RUSTERM_API_USER={user}\n\n\
rusterm() {{\n\
  if [ \"$#\" -eq 0 ]; then\n\
    printf '{function_usage}\\n' >&2\n\
    return 2\n\
  fi\n\
  if [ \"$1\" = \"--refresh\" ]; then\n\
    unset RUSTERM_HOSTS RUSTERM_API_PASSWORD\n\
    shift\n\
    [ \"$#\" -eq 0 ] && return 0\n\
  fi\n\
  RUSTERM_API_URL=$(printf '%s' \"$RUSTERM_API_URL\" | tr -d '\\r')\n\
  RUSTERM_API_USER=$(printf '%s' \"$RUSTERM_API_USER\" | tr -d '\\r')\n\
  if [ -z \"$RUSTERM_API_URL\" ]; then\n\
    printf '{missing_config}\\n' >&2\n\
    return 1\n\
  fi\n\
  if [ -z \"$RUSTERM_API_USER\" ]; then\n\
    printf '{missing_user}\\n' >&2\n\
    return 1\n\
  fi\n\
\n\
  if [ -z \"${{RUSTERM_API_PASSWORD+x}}\" ]; then\n\
    if [ ! -t 0 ]; then\n\
      printf '{password_not_tty}\\n' >&2\n\
      return 1\n\
    fi\n\
    printf '{password_prompt}' >&2\n\
    RUSTERM_STTY=$(stty -g)\n\
    trap 'stty \"$RUSTERM_STTY\"' 0 1 2 15\n\
    stty -echo\n\
    IFS= read -r RUSTERM_API_PASSWORD\n\
    stty \"$RUSTERM_STTY\"\n\
    trap - 0 1 2 15\n\
    printf '\\n' >&2\n\
    export RUSTERM_API_PASSWORD\n\
  fi\n\
\n\
  if [ -z \"${{RUSTERM_HOSTS+x}}\" ]; then\n\
    RUSTERM_HOSTS=$(curl --silent --show-error --fail \\\n      --user \"${{RUSTERM_API_USER}}:${{RUSTERM_API_PASSWORD}}\" \\\n      \"${{RUSTERM_API_URL}}/r\" | tr -d '\\r')\n\
    if [ -n \"$RUSTERM_HOSTS\" ]; then\n\
      export RUSTERM_HOSTS\n\
    fi\n\
  fi\n\
  RUSTERM_HOSTS=$(printf '%s' \"$RUSTERM_HOSTS\" | tr -d '\\r')\n\
  if [ -z \"$RUSTERM_HOSTS\" ]; then\n\
    printf '{no_hosts}\\n' >&2\n\
    return 1\n\
  fi\n\n\
  RUSTERM_FAILED=0\n\
  RUSTERM_ELEVATED='{elevated_query}'\n\
  RUSTERM_COUNT=0\n\
  for RUSTERM_TARGET in $RUSTERM_HOSTS; do\n\
    [ -z \"$RUSTERM_TARGET\" ] && continue\n\
    RUSTERM_COUNT=$((RUSTERM_COUNT + 1))\n\
  done\n\
  for RUSTERM_TARGET in $RUSTERM_HOSTS; do\n\
    [ -z \"$RUSTERM_TARGET\" ] && continue\n\
    [ \"$RUSTERM_COUNT\" -gt 1 ] && printf '\\n==> %s\\n' \"$RUSTERM_TARGET\"\n\
    if [ -n \"$RUSTERM_ELEVATED\" ]; then\n\
      RUSTERM_OUT=$(curl --silent --show-error --fail-with-body \\\n        --connect-timeout 10 --max-time 120 \\\n        --request POST \\\n        --user \"${{RUSTERM_API_USER}}:${{RUSTERM_API_PASSWORD}}\" \\\n        --header 'Accept: text/plain' \\\n        --header 'Content-Type: text/plain; charset=utf-8' \\\n        --data-binary \"$*\" \\\n        \"${{RUSTERM_API_URL}}/r/${{RUSTERM_TARGET}}${{RUSTERM_ELEVATED}}\" 2>&1)\n\
      RUSTERM_STATUS=$?\n\
      if [ \"$RUSTERM_STATUS\" -eq 0 ]; then\n\
        printf '%s\\n' \"$RUSTERM_OUT\"\n\
        continue\n\
      fi\n\
      case \"$RUSTERM_OUT\" in\n\
        *elevation_required*) ;;\n\
        *)\n\
          printf '%s\\n' \"$RUSTERM_OUT\" >&2\n\
          RUSTERM_FAILED=$RUSTERM_STATUS\n\
          continue\n\
          ;;\n\
      esac\n\
    fi\n\
    curl --silent --show-error --fail-with-body \\\n      --connect-timeout 10 --max-time 120 \\\n      --request POST \\\n      --user \"${{RUSTERM_API_USER}}:${{RUSTERM_API_PASSWORD}}\" \\\n      --header 'Accept: text/plain' \\\n      --header 'Content-Type: text/plain; charset=utf-8' \\\n      --data-binary \"$*\" \\\n      \"${{RUSTERM_API_URL}}/r/${{RUSTERM_TARGET}}\" || RUSTERM_FAILED=$?\n\
  done\n\n\
  if [ \"$RUSTERM_FAILED\" -ne 0 ]; then\n\
    printf '\\n{request_failed}\\n' \"$RUSTERM_FAILED\" >&2\n\
  fi\n\
  return \"$RUSTERM_FAILED\"\n\
}}\n\n\
# {command_marker_title}\n\
# {command_marker_help}\n\
{invocation_before}",
                url = shell_quote(url.trim_end_matches('/')),
                user = shell_quote(default_user),
            ),
            command: rendered_command,
            after_command: invocation_after,
        };
    }

    // Build the JSON body for one host. The payload variant selects the
    // field name (`command`, `script`, or `script_base64`); the relay
    // enforces mutual exclusivity server-side.
    let request_body = |host: &str| {
        let body = match payload {
            CurlPayload::Command(cmd) => serde_json::json!({
                "host_id": host,
                "command": cmd,
                "elevated": elevated,
            }),
            CurlPayload::Script(script) => serde_json::json!({
                "host_id": host,
                "script": script,
                "elevated": elevated,
            }),
            CurlPayload::ScriptBase64(b64) => serde_json::json!({
                "host_id": host,
                "script_base64": b64,
                "elevated": elevated,
            }),
        };
        body.to_string()
    };
    let invocation = |host: &str, body: &str| {
        format!(
            "rusterm_exec {} '{}' || RUSTERM_FAILED=$?\n",
            shell_quote(host),
            shell_escape(body),
        )
    };

    let first_host = &hosts[0];
    let first_body = request_body(first_host);
    // Locate the payload value inside the JSON so we can highlight it. The
    // field name differs per mode; find whichever key is present.
    let (field_key, field_value): (&'static str, &str) = match payload {
        CurlPayload::Command(v) => (r#""command":"#, v),
        CurlPayload::Script(v) => (r#""script":"#, v),
        CurlPayload::ScriptBase64(v) => (r#""script_base64":"#, v),
    };
    let field_json = serde_json::to_string(field_value).expect("strings always serialize to JSON");
    let value_start = first_body
        .find(field_key)
        .map(|start| start + field_key.len())
        .expect("generated JSON body always contains the payload field");
    let value_end = value_start + field_json.len();

    // Keep the JSON quotes outside the highlighted fragment. Escaping each
    // fragment independently is equivalent to escaping the concatenated body,
    // including payloads containing apostrophes or newlines.
    let body_before_command = &first_body[..value_start + 1];
    let body_command = &field_json[1..field_json.len() - 1];
    let body_after_command = &first_body[value_end - 1..];

    let mut after_command = format!(
        "{}' || RUSTERM_FAILED=$?\n",
        shell_escape(body_after_command)
    );
    for host in hosts.iter().skip(1) {
        after_command.push_str(&invocation(host, &request_body(host)));
    }
    after_command.push_str(&format!(
        "\nif [ \"$RUSTERM_FAILED\" -ne 0 ]; then\n\
  printf '\\n{request_failed}\\n' \"$RUSTERM_FAILED\" >&2\n\
fi\n"
    ));

    CurlPreviewParts {
        before_command: format!(
            "export RUSTERM_API_URL={url}\n\
export RUSTERM_API_USER={user}\n\
if [ -z \"${{RUSTERM_API_PASSWORD+x}}\" ]; then\n\
  if [ ! -t 0 ]; then\n\
    printf '{password_not_tty}\\n' >&2\n\
    exit 1\n\
  fi\n\
  printf '{password_prompt}' >&2\n\
  RUSTERM_STTY=$(stty -g)\n\
  trap 'stty \"$RUSTERM_STTY\"' 0 1 2 15\n\
  stty -echo\n\
  IFS= read -r RUSTERM_API_PASSWORD\n\
  stty \"$RUSTERM_STTY\"\n\
  trap - 0 1 2 15\n\
  printf '\\n' >&2\n\
  export RUSTERM_API_PASSWORD\n\
fi\n\n\
rusterm_pretty_json() {{\n\
  if command -v jq >/dev/null 2>&1; then\n\
    jq .\n\
  elif command -v python3 >/dev/null 2>&1; then\n\
    python3 -m json.tool\n\
  else\n\
    cat\n\
  fi\n\
}}\n\n\
rusterm_exec() {{\n\
  RUSTERM_TARGET=$1\n\
  RUSTERM_BODY=$2\n\
  printf '\\n==> %s\\n' \"$RUSTERM_TARGET\"\n\
  RUSTERM_RESPONSE=$(curl --silent --show-error --fail-with-body \\\n    --connect-timeout 10 --max-time 120 \\\n    --request POST \"${{RUSTERM_API_URL}}/api/v1/exec\" \\\n    --user \"${{RUSTERM_API_USER}}:${{RUSTERM_API_PASSWORD}}\" \\\n    --header 'Accept: application/json' \\\n    --header 'Content-Type: application/json' \\\n    --data \"$RUSTERM_BODY\")\n\
  RUSTERM_STATUS=$?\n\
  printf '%s\\n' \"$RUSTERM_RESPONSE\" | rusterm_pretty_json\n\
  return \"$RUSTERM_STATUS\"\n\
}}\n\n\
RUSTERM_FAILED=0\n\
# {command_marker_title}\n\
# {command_marker_help}\n\
rusterm_exec {host} '{body_before_command}",
            url = shell_quote(url.trim_end_matches('/')),
            user = shell_quote(default_user),
            host = shell_quote(first_host),
            body_before_command = shell_escape(body_before_command),
        ),
        command: shell_escape(body_command),
        after_command,
    }
}

/// Build a ready-to-run shell snippet. API credentials are exported once and
/// reused by curl; the password is read with terminal echo disabled instead of
/// being copied into the clipboard or shell history.
fn gen_curl(
    url: &str,
    default_user: &str,
    sessions: &[String],
    payload: &CurlPayload,
    elevated: bool,
) -> String {
    gen_curl_preview(url, default_user, sessions, payload, elevated).into_script()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_includes_endpoint_user_and_body() {
        let curl = gen_curl(
            "http://127.0.0.1:8080",
            "alice",
            &[],
            &CurlPayload::Command("uname -a".to_string()),
            true,
        );
        assert!(
            curl.contains("export RUSTERM_API_URL='http://127.0.0.1:8080'")
                && curl.contains("\"${RUSTERM_API_URL}/r/${RUSTERM_TARGET}${RUSTERM_ELEVATED}\""),
            "missing short-form endpoint export/use: {curl}"
        );
        assert!(curl.contains("export RUSTERM_API_USER='alice'"), "{curl}");
        assert!(
            curl.contains("rusterm() {"),
            "missing reusable function: {curl}"
        );
        // Hosts are discovered at runtime via GET /r, never baked in.
        assert!(
            curl.contains("\"${RUSTERM_API_URL}/r\"") && curl.contains("RUSTERM_HOSTS=$(curl"),
            "function must discover hosts dynamically: {curl}"
        );
        assert!(
            !curl.contains("prod-web"),
            "host id must not be baked in: {curl}"
        );
        assert!(
            curl.contains("if [ \"$1\" = \"--refresh\" ]; then"),
            "function must support --refresh: {curl}"
        );
        assert!(curl.contains("--data-binary \"$*\""), "{curl}");
        assert!(curl.contains("--header 'Accept: text/plain'"), "{curl}");
        assert!(
            curl.contains("--header 'Content-Type: text/plain; charset=utf-8'"),
            "{curl}"
        );
        assert!(curl.contains("IFS= read -r RUSTERM_API_PASSWORD"), "{curl}");
        assert!(curl.ends_with("rusterm uname -a\n"), "{curl}");
        assert!(!curl.contains("/api/v1/exec"), "{curl}");
        assert!(!curl.contains(r#"\"command\""#), "{curl}");
        assert!(!curl.contains(":PASS"), "{curl}");
    }

    #[test]
    fn curl_generates_one_pretty_request_per_selected_session() {
        let curl = gen_curl(
            "http://127.0.0.1:8080",
            "alice",
            &["host-a".to_string(), "host-b".to_string()],
            &CurlPayload::Command("uptime".to_string()),
            false,
        );

        assert_eq!(curl.matches("rusterm() {").count(), 1, "{curl}");
        // Hosts are not baked in; the loop iterates the discovered list.
        assert!(
            curl.contains("for RUSTERM_TARGET in $RUSTERM_HOSTS; do"),
            "loop must iterate discovered hosts: {curl}"
        );
        assert!(
            !curl.contains("host-a"),
            "host ids must not be baked in: {curl}"
        );
        assert!(
            !curl.contains("host-b"),
            "host ids must not be baked in: {curl}"
        );
        // Multi-host marker is emitted conditionally on RUSTERM_COUNT > 1.
        assert!(
            curl.contains(
                "[ \"$RUSTERM_COUNT\" -gt 1 ] && printf '\\n==> %s\\n' \"$RUSTERM_TARGET\""
            ),
            "{curl}"
        );
        assert!(curl.matches("--data-binary \"$*\"").count() >= 1, "{curl}");
        assert!(
            curl.contains("\"${RUSTERM_API_URL}/r/${RUSTERM_TARGET}\""),
            "non-elevated calls should not add an elevated query: {curl}"
        );
        assert!(curl.contains("if [ ! -t 0 ]; then"), "{curl}");
        assert!(curl.contains("RUSTERM_ELEVATED=''"), "{curl}");
        assert!(
            curl.contains("--silent --show-error --fail-with-body"),
            "{curl}"
        );
        assert!(
            curl.contains("--connect-timeout 10 --max-time 120"),
            "{curl}"
        );
    }

    #[test]
    fn curl_command_mode_discovers_hosts_at_runtime_and_never_bakes_them_in() {
        // The whole point of the dynamic-discovery rewrite: a copied function
        // must keep working after the session that was selected at copy time
        // goes away. We pass a non-empty session list to prove even selected
        // hosts are NOT baked in.
        let curl = gen_curl(
            "http://relay.example:8877",
            "ops",
            &[
                "100eac5a-2ef7-427f-84e1-9e03e2b61820".to_string(),
                "staging-host".to_string(),
            ],
            &CurlPayload::Command("uname -a".to_string()),
            true,
        );

        // Neither the UUID nor the friendly name may appear in the output.
        assert!(
            !curl.contains("100eac5a"),
            "selected host id must not be baked in: {curl}"
        );
        assert!(
            !curl.contains("staging-host"),
            "selected host name must not be baked in: {curl}"
        );

        // Discovery hits GET /r with the same credentials.
        assert!(
            curl.contains("RUSTERM_HOSTS=$(curl --silent --show-error --fail"),
            "must discover hosts via curl: {curl}"
        );
        assert!(
            curl.contains("--user \"${RUSTERM_API_USER}:${RUSTERM_API_PASSWORD}\""),
            "discovery must reuse the same credentials: {curl}"
        );
        assert!(
            curl.contains("\"${RUSTERM_API_URL}/r\""),
            "discovery must hit /r (no host_id): {curl}"
        );
        assert!(
            curl.contains("| tr -d '\\r'"),
            "discovery must strip carriage returns (CRLF-safe): {curl}"
        );
        assert!(
            curl.contains("RUSTERM_HOSTS=$(curl --silent --show-error --fail"),
            "discovery must surface failures with --show-error: {curl}"
        );

        // --refresh support.
        assert!(
            curl.contains("if [ \"$1\" = \"--refresh\" ]; then"),
            "function must handle --refresh: {curl}"
        );
        assert!(
            curl.contains("unset RUSTERM_HOSTS RUSTERM_API_PASSWORD"),
            "--refresh must clear cached hosts and credentials: {curl}"
        );

        // RUSTERM_HOSTS override path: if the user pre-exports it, discovery
        // is skipped.
        assert!(
            curl.contains("if [ -z \"${RUSTERM_HOSTS+x}\" ]; then"),
            "discovery must be gated on RUSTERM_HOSTS being unset: {curl}"
        );

        // No-hosts guard returns a clear error instead of a silent no-op.
        assert!(
            curl.contains("if [ -z \"$RUSTERM_HOSTS\" ]; then"),
            "must guard against an empty host list: {curl}"
        );

        // Sanitization: even if RUSTERM_HOSTS or RUSTERM_API_URL were
        // poisoned by a previous bad discovery (e.g. CRLF from a 502 page),
        // the function strips carriage returns before use.
        assert!(
            curl.contains("RUSTERM_HOSTS=$(printf '%s' \"$RUSTERM_HOSTS\" | tr -d '\\r')"),
            "must sanitize RUSTERM_HOSTS of carriage returns: {curl}"
        );
        assert!(
            curl.contains("RUSTERM_API_URL=$(printf '%s' \"$RUSTERM_API_URL\" | tr -d '\\r')"),
            "must sanitize RUSTERM_API_URL of carriage returns: {curl}"
        );

        // Empty-config guard: if RUSTERM_API_URL is unset/empty (e.g. the user
        // pasted only the function without the export lines, or opened a fresh
        // shell), the function must fail fast with a clear message instead of
        // building a relative URL like "/r/host" that curl rejects with (3).
        assert!(
            curl.contains("if [ -z \"$RUSTERM_API_URL\" ]; then"),
            "must guard against an empty RUSTERM_API_URL: {curl}"
        );
        assert!(
            curl.contains("if [ -z \"$RUSTERM_API_USER\" ]; then"),
            "must guard against an empty RUSTERM_API_USER: {curl}"
        );

        // Execution loop must skip empty targets (defensive against trailing
        // newlines or whitespace in the host list).
        assert!(
            curl.contains("[ -z \"$RUSTERM_TARGET\" ] && continue"),
            "execution loop must skip empty targets: {curl}"
        );

        // Elevated requests fall back per host when sudo authorization is
        // unavailable, without failing requests to other hosts.
        assert!(curl.contains("if [ ! -t 0 ]; then"), "{curl}");
        assert!(curl.contains("RUSTERM_ELEVATED='?elevated=true'"), "{curl}");
        assert!(curl.contains("*elevation_required*)"), "{curl}");
        assert!(
            curl.contains("\"${RUSTERM_API_URL}/r/${RUSTERM_TARGET}${RUSTERM_ELEVATED}\""),
            "elevated must be forwarded as a query param: {curl}"
        );
        assert!(
            curl.contains("\"${RUSTERM_API_URL}/r/${RUSTERM_TARGET}\""),
            "elevation_required must fall back to a normal request: {curl}"
        );
    }

    #[test]
    fn generated_curl_templates_are_valid_posix_shell() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let session = ["host-a".to_string()];
        let payloads = [
            CurlPayload::Command("printf '%s\\n' ok".to_string()),
            CurlPayload::Script("printf '%s\\n' ok".to_string()),
            CurlPayload::ScriptBase64("cHJpbnRmICclc1xcbicgb2s=".to_string()),
        ];

        for payload in payloads {
            let script = gen_curl("http://127.0.0.1:8877", "alice", &session, &payload, true);
            let mut child = Command::new("/bin/sh")
                .arg("-n")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("/bin/sh must be available");
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(script.as_bytes())
                .expect("write generated shell template");
            let output = child.wait_with_output().expect("wait for /bin/sh -n");
            assert!(
                output.status.success(),
                "generated template failed /bin/sh -n: {}\n{script}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn curl_script_copy_follows_the_requested_language() {
        let sessions = ["prod-web".to_string()];
        let english = gen_curl_preview_for_language(
            "http://127.0.0.1:8080",
            "alice",
            &sessions,
            &CurlPayload::Command("docker ps".to_string()),
            true,
            crate::i18n::Language::En,
        )
        .into_script();
        let chinese = gen_curl_preview_for_language(
            "http://127.0.0.1:8080",
            "alice",
            &sessions,
            &CurlPayload::Command("docker ps".to_string()),
            true,
            crate::i18n::Language::Zh,
        )
        .into_script();

        assert!(
            english.contains("# RUN NOW; THE FUNCTION REMAINS REUSABLE"),
            "{english}"
        );
        assert!(english.contains("RusTerm API password: "), "{english}");
        assert!(english.contains("Usage: rusterm <command...>"), "{english}");
        assert!(
            !english.contains("立即执行；此函数后续仍可复用"),
            "{english}"
        );
        assert!(
            chinese.contains("# 立即执行；此函数后续仍可复用"),
            "{chinese}"
        );
        assert!(chinese.contains("RusTerm API 密码："), "{chinese}");
        assert!(chinese.contains("用法：rusterm <命令...>"), "{chinese}");
        assert!(
            !chinese.contains("RUN NOW; THE FUNCTION REMAINS REUSABLE"),
            "{chinese}"
        );
        for script in [english, chinese] {
            assert!(script.ends_with("rusterm docker ps\n"), "{script}");
            assert!(!script.contains(r#"\"command\""#), "{script}");
        }
    }

    #[test]
    fn curl_escapes_json_special_chars_in_command() {
        let curl = gen_curl(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::Command(r#"echo "hi" 'there' \n"#.to_string()),
            false,
        );
        assert!(
            curl.ends_with("rusterm 'echo \"hi\" '\"'\"'there'\"'\"' \\n'\n"),
            "shell-sensitive commands must be emitted as one safely quoted argument: {curl}"
        );
        assert!(!curl.contains(r#"\"command\""#), "{curl}");
    }

    #[test]
    fn curl_preview_highlights_only_the_json_command_value() {
        let preview = gen_curl_preview(
            "http://repeat.example",
            "repeat",
            &["repeat".to_string()],
            &CurlPayload::Command("repeat".to_string()),
            true,
        );
        let marked = format!(
            "{}<mark>{}</mark>{}",
            preview.before_command, preview.command, preview.after_command
        );

        assert_eq!(marked.matches("<mark>").count(), 1);
        assert!(
            marked.ends_with("rusterm <mark>repeat</mark>\n"),
            "highlight must wrap only the sample command: {marked}"
        );
    }

    #[test]
    fn curl_preview_highlight_preserves_json_and_shell_escaping() {
        let preview = gen_curl_preview(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::Command(r#"echo "hi" 'there' \n"#.to_string()),
            false,
        );

        assert_eq!(preview.command, r#"echo "hi" '"'"'there'"'"' \n"#);
        let marked = format!(
            "{}<mark>{}</mark>{}",
            preview.before_command, preview.command, preview.after_command
        );
        assert!(
            marked.ends_with("rusterm '<mark>echo \"hi\" '\"'\"'there'\"'\"' \\n</mark>'\n"),
            "outer quotes must stay outside the highlighted command: {marked}"
        );
    }

    #[test]
    fn curl_preview_handles_an_empty_command() {
        let preview = gen_curl_preview(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::Command(String::new()),
            false,
        );

        assert!(preview.command.is_empty());
        let script = preview.into_script();
        assert!(
            script.ends_with("# rusterm <command...>\n"),
            "an empty command should leave a reusable example instead of executing: {script}"
        );
    }

    #[test]
    fn curl_handles_missing_session_with_placeholder() {
        // Command mode discovers hosts at runtime, so an empty session list
        // must still produce a working function (no HOST placeholder needed).
        let curl = gen_curl(
            "http://x",
            "u",
            &[],
            &CurlPayload::Command("ls".to_string()),
            false,
        );
        assert!(
            curl.contains("RUSTERM_HOSTS=$(curl"),
            "command mode must discover hosts dynamically even with no session: {curl}"
        );
        assert!(
            !curl.contains("'HOST'"),
            "command mode must not bake in a placeholder: {curl}"
        );

        // Script / base64 mode still bakes in a HOST placeholder when no
        // session is selected, because those payloads target a single host.
        let script = gen_curl(
            "http://x",
            "u",
            &[],
            &CurlPayload::Script("echo hi\n".to_string()),
            false,
        );
        assert!(
            script.contains("'HOST'"),
            "script mode should still use HOST placeholder: {script}"
        );
    }

    // ── Script / base64 mode (issue 73) ───────────────────────────────────

    #[test]
    fn curl_script_mode_emits_script_field() {
        let curl = gen_curl(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::Script("#!/bin/sh\necho hi\n".to_string()),
            false,
        );
        assert!(
            curl.contains(r#""script":""#),
            "script mode must emit a script field: {curl}"
        );
        assert!(
            !curl.contains(r#""command":""#),
            "script mode must not emit a command field: {curl}"
        );
        // Newlines in the script are JSON-escaped as the two-character
        // sequence \n so the --data arg stays a single shell-quoted string.
        // serde_json does not escape `/`, so the shebang line stays as `#!`.
        assert!(
            curl.contains(r##""script":"#!"##),
            "script must start with the shebang: {curl}"
        );
        // The literal two-character backslash-n sequence must appear
        // (JSON-escaped newline). serde_json escapes newline as \n.
        assert!(
            curl.contains(r##"echo hi\n""##),
            "script newline must be JSON-escaped as backslash-n: {curl}"
        );
    }

    #[test]
    fn curl_script_base64_mode_emits_script_base64_field() {
        let curl = gen_curl(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::ScriptBase64("IyEvYmluL3NoCmVjaG8gaGkK".to_string()),
            false,
        );
        assert!(
            curl.contains(r#""script_base64":"IyEvYmluL3NoCmVjaG8gaGkK""#),
            "base64 mode must emit a script_base64 field: {curl}"
        );
        assert!(
            !curl.contains(r#""command":""#) && !curl.contains(r#""script":""#),
            "base64 mode must not emit command or script fields: {curl}"
        );
    }

    #[test]
    fn curl_script_mode_highlights_script_value() {
        let preview = gen_curl_preview(
            "http://x",
            "u",
            &["h".to_string()],
            &CurlPayload::Script("echo hello".to_string()),
            false,
        );
        let marked = format!(
            "{}<mark>{}</mark>{}",
            preview.before_command, preview.command, preview.after_command
        );
        assert_eq!(marked.matches("<mark>").count(), 1);
        assert!(
            marked.contains(r#""script":"<mark>echo hello</mark>""#),
            "highlight must wrap the script field value: {marked}"
        );
    }

    #[test]
    fn curl_script_mode_supports_multiple_hosts() {
        let curl = gen_curl(
            "http://x",
            "u",
            &["host-a".to_string(), "host-b".to_string()],
            &CurlPayload::Script("uptime\n".to_string()),
            false,
        );
        assert_eq!(curl.matches("rusterm_exec '").count(), 2, "{curl}");
        assert!(curl.contains(r#"host_id":"host-a"#), "{curl}");
        assert!(curl.contains(r#"host_id":"host-b"#), "{curl}");
        // Both hosts carry the script field.
        assert_eq!(curl.matches(r#""script":""#).count(), 2, "{curl}");
    }

    #[test]
    fn base_url_prefers_running_url() {
        assert_eq!(
            base_url(&Some("http://1.2.3.4:90/".to_string()), "127.0.0.1", 8080),
            "http://1.2.3.4:90"
        );
    }

    #[test]
    fn base_url_falls_back_to_bind_and_port() {
        assert_eq!(base_url(&None, "127.0.0.1", 8080), "http://127.0.0.1:8080");
        // 0.0.0.0 is shown as 127.0.0.1 so the local curl preview works.
        assert_eq!(base_url(&None, "0.0.0.0", 8080), "http://127.0.0.1:8080");
    }
}
