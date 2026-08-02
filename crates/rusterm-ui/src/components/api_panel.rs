//! Bottom-dock REST API relay panel.
//!
//! A compact, always-available counterpart to the full [`crate::components::RelayPanel`]
//! modal. Lives in the bottom dock (the "API" tab) so the user can:
//!   1. Toggle / configure the relay (bind addr, port, accounts) without
//!      opening a modal.
//!   2. Auto-generate ready-to-paste `curl` commands that execute a chosen
//!      command on a chosen connected SSH session through the relay, using
//!      HTTP BasicAuth through exported runtime environment variables.
//!
//! The generated curl targets `POST /api/v1/exec` with a JSON body of
//! `{ "host_id": "<id-or-name>", "command": "<cmd>", "elevated": <bool> }`,
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
            .api-command-field{{padding:8px;border:1px solid rgba(224,175,104,.55);border-radius:5px;background:rgba(224,175,104,.08);}}
            .api-command-field > span{{color:#e0af68;font-weight:600;}}
            .api-command-field .api-input{{border-color:#e0af68;box-shadow:0 0 0 1px rgba(224,175,104,.18);}}
            .api-command-edit-hint{{font-size:11px;color:#e0af68;line-height:1.4;}}
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
                    div { class: "api-field api-command-field",
                        span { { crate::i18n::t("api.command") } }
                        div { class: "api-command-edit-hint",
                            { crate::i18n::t("api.command_edit_hint") }
                        }
                        input {
                            class: "api-input",
                            value: "{curl_command}",
                            placeholder: "kubectl get pods",
                            oninput: move |e| curl_command.set(e.value()),
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
                        let preview = gen_curl_preview(
                            &url,
                            &default_user,
                            &selected_session(),
                            &curl_command(),
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
                            // Clone into the move closure so `url` itself isn't
                            // moved out of the render scope.
                            onclick: move |_| {
                                let curl = gen_curl(
                                    &url_for_copy,
                                    &default_user,
                                    &selected_session(),
                                    &curl_command(),
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
                div { class: "api-sect", "Endpoints" }
                {
                    let endpoints = format!(
"GET  {url}/api/v1/health      # liveness, no auth
GET  {url}/api/v1/hosts        # list hosts (BasicAuth)
POST {url}/api/v1/exec         # {{ host_id, command, elevated?, timeout_ms? }}
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

/// Build the shell snippet as three text fragments so the JSON `command` value
/// can be styled independently without injecting HTML or matching duplicate
/// command text elsewhere in the script.
fn gen_curl_preview(
    url: &str,
    default_user: &str,
    session: &Option<String>,
    command: &str,
    elevated: bool,
) -> CurlPreviewParts {
    let host = session.clone().unwrap_or_else(|| "HOST".to_string());
    let body = serde_json::json!({
        "host_id": host,
        "command": command,
        "elevated": elevated,
    })
    .to_string();
    let command_json = serde_json::to_string(command).expect("strings always serialize to JSON");
    let command_key = "\"command\":";
    let value_start = body
        .find(command_key)
        .map(|start| start + command_key.len())
        .expect("generated JSON body always contains the command field");
    let value_end = value_start + command_json.len();

    // Keep the JSON quotes outside the highlighted fragment. Escaping each
    // fragment independently is equivalent to escaping the concatenated body,
    // including commands containing apostrophes.
    let body_before_command = &body[..value_start + 1];
    let body_command = &command_json[1..command_json.len() - 1];
    let body_after_command = &body[value_end - 1..];
    let shell_escape = |value: &str| value.replace('\'', "'\"'\"'");
    let shell_quote = |value: &str| format!("'{}'", shell_escape(value));

    CurlPreviewParts {
        before_command: format!(
            "export RUSTERM_API_URL={url}\n\
export RUSTERM_API_USER={user}\n\
if [ -z \"${{RUSTERM_API_PASSWORD+x}}\" ]; then\n\
  printf 'RusTerm API password: ' >&2\n\
  stty -echo\n\
  IFS= read -r RUSTERM_API_PASSWORD\n\
  stty echo\n\
  printf '\\n' >&2\n\
  export RUSTERM_API_PASSWORD\n\
fi\n\n\
# EDIT REMOTE COMMAND BELOW / 在下方修改远程命令\n\
# Change the JSON \"command\" value / 请替换 JSON 中的 \"command\" 值\n\
curl -X POST \"${{RUSTERM_API_URL}}/api/v1/exec\" \\\n  -u \"${{RUSTERM_API_USER}}:${{RUSTERM_API_PASSWORD}}\" \\\n  -H 'Content-Type: application/json' \\\n  -d '{body_before_command}",
            url = shell_quote(url.trim_end_matches('/')),
            user = shell_quote(default_user),
            body_before_command = shell_escape(body_before_command),
        ),
        command: shell_escape(body_command),
        after_command: format!("{}'", shell_escape(body_after_command)),
    }
}

/// Build a ready-to-run shell snippet. API credentials are exported once and
/// reused by curl; the password is read with terminal echo disabled instead of
/// being copied into the clipboard or shell history.
fn gen_curl(
    url: &str,
    default_user: &str,
    session: &Option<String>,
    command: &str,
    elevated: bool,
) -> String {
    gen_curl_preview(url, default_user, session, command, elevated).into_script()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_includes_endpoint_user_and_body() {
        let curl = gen_curl(
            "http://127.0.0.1:8080",
            "alice",
            &["prod-web".to_string()],
            "uname -a",
            true,
        );
        assert!(
            curl.contains("export RUSTERM_API_URL='http://127.0.0.1:8080'")
                && curl.contains("\"${RUSTERM_API_URL}/api/v1/exec\""),
            "missing endpoint export/use: {curl}"
        );
        assert!(
            curl.contains("export RUSTERM_API_USER='alice'"),
            "missing configured user export: {curl}"
        );
        assert!(
            curl.contains("-u \"${RUSTERM_API_USER}:${RUSTERM_API_PASSWORD}\""),
            "missing environment-based basic-auth: {curl}"
        );
        assert!(
            curl.contains("IFS= read -r RUSTERM_API_PASSWORD"),
            "password should be read once at runtime: {curl}"
        );
        assert!(
            !curl.contains(":PASS"),
            "placeholder must be removed: {curl}"
        );
        assert!(
            curl.contains("host_id\":\"prod-web\""),
            "missing host_id: {curl}"
        );
        assert!(
            curl.contains("command\":\"uname -a\""),
            "missing command: {curl}"
        );
        assert!(
            curl.contains("elevated\":true"),
            "missing elevation flag: {curl}"
        );
    }

    #[test]
    fn curl_marks_the_remote_command_edit_location() {
        let curl = gen_curl(
            "http://127.0.0.1:8080",
            "alice",
            &["prod-web".to_string()],
            "docker ps",
            true,
        );
        assert!(
            curl.contains("# EDIT REMOTE COMMAND BELOW / 在下方修改远程命令"),
            "missing command edit marker: {curl}"
        );
        assert!(
            curl.contains("command\":\"docker ps\""),
            "the editable command should remain in the JSON body: {curl}"
        );
    }

    #[test]
    fn curl_escapes_json_special_chars_in_command() {
        // A command with a double-quote and backslash must be JSON-escaped so
        // the generated body stays valid JSON the relay can parse. The raw
        // command `echo "hi" \n` must become `echo \"hi\" \\n` in the
        // JSON value (quotes → \", backslash → \\).
        let curl = gen_curl(
            "http://x",
            "u",
            &["h".to_string()],
            r#"echo "hi" \n"#,
            false,
        );
        assert!(
            curl.contains(r#"command":"echo \"hi\" \\n""#),
            "bad escape: {curl}"
        );
    }

    #[test]
    fn curl_preview_highlights_only_the_json_command_value() {
        let preview = gen_curl_preview(
            "http://repeat.example",
            "repeat",
            &["repeat".to_string()],
            "repeat",
            true,
        );
        let marked = format!(
            "{}<mark>{}</mark>{}",
            preview.before_command, preview.command, preview.after_command
        );

        assert_eq!(marked.matches("<mark>").count(), 1);
        assert!(
            marked.contains(r#""command":"<mark>repeat</mark>""#),
            "highlight must wrap only the command field value: {marked}"
        );
    }

    #[test]
    fn curl_preview_highlight_preserves_json_and_shell_escaping() {
        let preview = gen_curl_preview(
            "http://x",
            "u",
            &["h".to_string()],
            r#"echo "hi" 'there' \n"#,
            false,
        );

        assert_eq!(preview.command, r#"echo \"hi\" '"'"'there'"'"' \\n"#);
        let script = preview.into_script();
        assert!(
            script.contains(r#"command":"echo \"hi\" '"'"'there'"'"' \\n""#),
            "highlight fragments must concatenate into the escaped shell script: {script}"
        );
    }

    #[test]
    fn curl_preview_handles_an_empty_command() {
        let preview = gen_curl_preview("http://x", "u", &["h".to_string()], "", false);

        assert!(preview.command.is_empty());
        assert!(
            preview.into_script().contains(r#""command":"""#),
            "empty command must remain valid JSON"
        );
    }

    #[test]
    fn curl_handles_missing_session_with_placeholder() {
        let curl = gen_curl("http://x", "u", &[], "ls", false);
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
