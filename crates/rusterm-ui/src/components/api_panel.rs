//! Bottom-dock REST API relay panel.
//!
//! A compact, always-available counterpart to the full [`crate::components::RelayPanel`]
//! modal. Lives in the bottom dock (the "API" tab) so the user can:
//!   1. Toggle / configure the relay (bind addr, port, accounts) without
//!      opening a modal.
//!   2. Auto-generate ready-to-paste `curl` commands that execute a chosen
//!      command on a chosen connected SSH session through the relay, using
//!      HTTP BasicAuth (`-u USER:PASS`).
//!
//! The generated curl targets `POST /api/v1/exec` with a JSON body of
//! `{ "host_id": "<id-or-name>", "command": "<cmd>", "timeout_ms": <ms> }`,
//! matching the relay's [`ExecRequest`](rusterm_relay::server) schema.

use dioxus::prelude::*;
use rusterm_relay::{RelayAccount, hash_password};

use crate::state::AppState;

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

    // Connected SSH sessions → (name, label) pairs for the curl builder.
    // The relay's exec endpoint accepts either the connection id or its name
    // (server.rs matches `h.id == body.host_id || h.name == body.host_id`),
    // so we use the session's `name` (which equals the connection name) as
    // the host_id — it's human-readable in the generated curl.
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
                (tab.name.clone(), label)
            })
            .collect()
    };
    // Default the selected session to the first available; keep the last
    // selection if still valid.
    let mut selected_session = use_signal(|| sessions.first().map(|(id, _)| id.clone()));
    if selected_session()
        .as_ref()
        .is_some_and(|s| !sessions.iter().any(|(id, _)| id == s))
    {
        selected_session.set(sessions.first().map(|(id, _)| id.clone()));
    }

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
            .api-row{{display:flex;align-items:center;gap:8px;margin-bottom:8px;}}
            .api-btn{{border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:11px;padding:4px 10px;cursor:pointer;}}
            .api-btn:hover{{border-color:#7aa2f7;color:#7aa2f7;}}
            .api-btn.primary{{background:#7aa2f7;color:#1a1b26;border:1px solid #7aa2f7;font-weight:600;}}
            .api-btn.primary:hover{{color:#1a1b26;opacity:.9;}}
            .api-btn.danger:hover{{border-color:#f7768e;color:#f7768e;}}
            .api-status{{font-size:11px;padding:5px 9px;border-radius:4px;margin-bottom:8px;}}
            .api-status.ok{{background:rgba(76,175,80,.12);color:#9ece6a;}}
            .api-status.err{{background:rgba(247,118,142,.12);color:#f7768e;}}
            .api-status.idle{{background:#24283b;color:#9aa5ce;}}
            .api-account{{border:1px solid #2a2b3d;border-radius:4px;padding:6px 8px;margin-bottom:5px;background:#1a1b26;}}
            .api-code{{font-family:'JetBrains Mono','Fira Code',ui-monospace,monospace;font-size:11px;background:#16161e;border:1px solid #2a2b3d;border-radius:4px;padding:8px 10px;color:#9ece6a;white-space:pre-wrap;word-break:break-all;}}
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
                    div { class: "api-row",
                        span { style: "width:64px;font-size:11px;color:#9aa5ce;", { crate::i18n::t("api.session") } }
                        select {
                            class: "api-input",
                            style: "flex:1;",
                            value: "{selected_session().clone().unwrap_or_default()}",
                            onchange: move |e| selected_session.set(if e.value().is_empty() { None } else { Some(e.value()) }),
                            for (id, label) in sessions.iter() {
                                option { value: "{id}", "{label}" }
                            }
                        }
                    }
                    div { class: "api-field",
                        span { { crate::i18n::t("api.command") } }
                        input {
                            class: "api-input",
                            value: "{curl_command}",
                            placeholder: "kubectl get pods",
                            oninput: move |e| curl_command.set(e.value()),
                        }
                    }

                    div { class: "api-code",
                        { gen_curl(&url, &default_user, &selected_session(), &curl_command()) }
                    }
                    div { class: "api-row", style: "margin-top:8px;",
                        button {
                            class: "api-btn primary",
                            // Clone into the move closure so `url` itself isn't
                            // moved out of the render scope.
                            onclick: move |_| {
                                let curl = gen_curl(&url_for_copy, &default_user, &selected_session(), &curl_command());
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
                div { class: "api-sect", "Endpoints" }
                {
                    let endpoints = format!(
"GET  {url}/api/v1/health      # liveness, no auth
GET  {url}/api/v1/hosts        # list hosts (BasicAuth)
POST {url}/api/v1/exec         # {{ host_id, command, timeout_ms? }}
POST {url}/api/v1/parse-curl   # parse a pasted curl into JSON",
                        url = url.clone(),
                    );
                    rsx! {
                        div { class: "api-code", "{endpoints}" }
                    }
                }
            }
        }
    }
}

/// Build the curl command string for the current selection. The body JSON is
/// constructed by hand so we control the exact escaping: the command text is
/// embedded into a JSON string value (escaping `\` and `"`), and the whole
/// body is wrapped in single quotes for the shell so the user can paste it
/// verbatim. Embedded single quotes in the body are handled via the POSIX
/// `'\''` idiom so the pasted curl is always shell-safe.
fn gen_curl(url: &str, default_user: &str, session: &Option<String>, command: &str) -> String {
    let host = session.clone().unwrap_or_else(|| "HOST".to_string());
    // JSON-string-escape the host and command for the request body value.
    let json_escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let body = format!(
        "{{\"host_id\":\"{}\",\"command\":\"{}\"}}",
        json_escape(&host),
        json_escape(command),
    );
    // Wrap the body in single quotes for the shell; escape any literal single
    // quotes inside via the POSIX-safe `'\''` sequence.
    let body_sq = format!("'{}'", body.replace('\'', "'\\\''"));
    format!(
        "curl -X POST '{url}/api/v1/exec' \\
  -u '{user}:PASS' \\
  -H 'Content-Type: application/json' \\
  -d {body}",
        url = url.trim_end_matches('/'),
        user = default_user,
        body = body_sq,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_includes_endpoint_user_and_body() {
        let curl = gen_curl(
            "http://127.0.0.1:8080",
            "alice",
            &Some("prod-web".to_string()),
            "uname -a",
        );
        assert!(
            curl.contains("http://127.0.0.1:8080/api/v1/exec"),
            "missing endpoint: {curl}"
        );
        assert!(
            curl.contains("-u 'alice:PASS'"),
            "missing basic-auth: {curl}"
        );
        assert!(
            curl.contains("host_id\":\"prod-web\""),
            "missing host_id: {curl}"
        );
        assert!(
            curl.contains("command\":\"uname -a\""),
            "missing command: {curl}"
        );
    }

    #[test]
    fn curl_escapes_json_special_chars_in_command() {
        // A command with a double-quote and backslash must be JSON-escaped so
        // the generated body stays valid JSON the relay can parse. The raw
        // command `echo "hi" \n` must become `echo \"hi\" \\n` in the
        // JSON value (quotes → \", backslash → \\).
        let curl = gen_curl("http://x", "u", &Some("h".to_string()), r#"echo "hi" \n"#);
        assert!(
            curl.contains(r#"command":"echo \"hi\" \\n""#),
            "bad escape: {curl}"
        );
    }

    #[test]
    fn curl_handles_missing_session_with_placeholder() {
        let curl = gen_curl("http://x", "u", &None, "ls");
        assert!(
            curl.contains("HOST"),
            "missing-session should use HOST placeholder: {curl}"
        );
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
