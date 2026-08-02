use dioxus::prelude::*;

use rusterm_core::config::{ConnectionConfig, ConnectionGroup, ConnectionKind, ProxyKind, SshAuth};
use rusterm_ssh::{
    SshHostSuggestion, default_ssh_config_path, list_identity_files, list_ssh_config_hosts,
    lookup_host,
};

#[derive(Debug, Clone, Default)]
pub struct NewConnectionForm {
    pub name: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub auth_type: String,
    pub password: String,
    pub key_path: String,
    pub passphrase: String,
    pub terminal_type: String,
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_username: String,
    pub proxy_password: String,
    pub group_id: Option<String>,
    pub onekey: bool,
    /// Raw login-script DSL text (expect/send/send_onekey/delay lines).
    /// Empty string means "no script". Only meaningful for SSH/Shell kinds.
    pub login_script: String,
}

const TERMINAL_TYPES: &[&str] = &[
    "xterm-256color",
    "xterm",
    "vt100",
    "vt220",
    "vt320",
    "linux",
    "ansi",
    "screen-256color",
    "screen",
];

fn default_form() -> NewConnectionForm {
    NewConnectionForm {
        auth_type: "password".to_string(),
        terminal_type: "xterm-256color".to_string(),
        proxy_type: "none".to_string(),
        port: "22".to_string(),
        ..Default::default()
    }
}

/// Build a form pre-filled from an existing connection so the edit dialog
/// shows the saved values. Only SSH connections populate the host/port/auth
/// fields; non-SSH kinds only carry name + onekey (the SSH-specific inputs are
/// left at defaults and, on save, the original `kind` is preserved unchanged
/// — see `app::rebuild_connection`).
fn form_from_connection(c: &ConnectionConfig) -> NewConnectionForm {
    match &c.kind {
        ConnectionKind::Ssh(ssh) => {
            let (auth_type, password, key_path, passphrase) = match &ssh.auth {
                SshAuth::Password { password } => {
                    ("password", password.clone(), String::new(), String::new())
                }
                SshAuth::Key {
                    private_key_path,
                    passphrase,
                } => (
                    "key",
                    String::new(),
                    private_key_path.clone(),
                    passphrase.clone().unwrap_or_default(),
                ),
                SshAuth::Agent => ("agent", String::new(), String::new(), String::new()),
            };
            let (proxy_type, proxy_host, proxy_port, proxy_username, proxy_password) = ssh
                .proxy
                .as_ref()
                .map(|proxy| {
                    let proxy_type = match proxy.kind {
                        ProxyKind::Http => "http",
                        ProxyKind::Https => "https",
                        ProxyKind::Socks5 => "socks5",
                    };
                    (
                        proxy_type.to_string(),
                        proxy.host.clone(),
                        proxy.port.to_string(),
                        proxy.username.clone().unwrap_or_default(),
                        proxy.password.clone().unwrap_or_default(),
                    )
                })
                .unwrap_or_else(|| {
                    (
                        "none".to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                    )
                });
            NewConnectionForm {
                name: c.name.clone(),
                host: ssh.host.clone(),
                port: ssh.port.to_string(),
                username: ssh.username.clone(),
                auth_type: auth_type.to_string(),
                password,
                key_path,
                passphrase,
                terminal_type: ssh.terminal_type.clone(),
                proxy_type,
                proxy_host,
                proxy_port,
                proxy_username,
                proxy_password,
                group_id: c.group.clone(),
                onekey: c.onekey,
                    login_script: c.login_script.clone().unwrap_or_default(),
    }
        }
        // Non-SSH connections can still be renamed / onekey-toggled; the SSH
        // fields are irrelevant and ignored on save (kind is preserved).
        _ => NewConnectionForm {
            name: c.name.clone(),
            group_id: c.group.clone(),
            onekey: c.onekey,
            login_script: String::new(),
            ..default_form()
        },
    }
}

#[component]
pub fn ConnectionDialog(
    visible: bool,
    groups: Vec<ConnectionGroup>,
    on_close: EventHandler<()>,
    on_create: EventHandler<NewConnectionForm>,
    /// When `Some`, the dialog operates in edit mode: fields are pre-filled
    /// from this connection and the submit button routes to `on_edit`
    /// (carrying the connection id) instead of `on_create`. The connection id
    /// is preserved so the existing entry is replaced in place rather than
    /// duplicated.
    editing: Option<ConnectionConfig>,
    on_edit: EventHandler<(String, NewConnectionForm)>,
) -> Element {
    let mut form = use_signal(default_form);
    // Tracks the id of the connection currently reflected in `form`. When the
    // `editing` prop changes (e.g. user clicks Edit on a different row, or
    // switches back to New), we re-seed the form. Setting a signal during
    // render is safe here because the guard makes the write idempotent — no
    // re-render loop.
    let mut seeded_id = use_signal(String::new);

    // Local SSH config + identity-file suggestions, loaded ONCE on first
    // mount. We read `~/.ssh/config` and `~/.ssh/` synchronously here
    // because (a) both reads are tiny (one small text file + one
    // directory listing), (b) they're tolerant of missing files (return
    // empty Vec), and (c) `use_signal` only invokes its initializer on
    // first mount, so the I/O happens exactly once per dialog lifetime.
    // The dialog itself isn't mounted until the user opens it, so this
    // I/O doesn't happen at app startup.
    //
    // We use `use_signal` (not `use_resource`) because the reads are
    // synchronous and fast — `use_resource` would add async overhead
    // and a loading state for no benefit.
    let host_suggestions: Signal<Vec<SshHostSuggestion>> = use_signal(list_ssh_config_hosts);
    let identity_suggestions: Signal<Vec<String>> = use_signal(list_identity_files);
    // The resolved `~/.ssh/config` path (for display in the UI hint).
    // Computed once on mount.
    let ssh_config_path_display: Signal<Option<String>> =
        use_signal(|| default_ssh_config_path().map(|p| p.to_string_lossy().into_owned()));

    if !visible {
        return rsx! {};
    }

    let editing_id = editing.as_ref().map(|c| c.id.clone()).unwrap_or_default();
    if seeded_id() != editing_id {
        match &editing {
            Some(c) => form.set(form_from_connection(c)),
            None => form.set(default_form()),
        }
        seeded_id.set(editing_id.clone());
    }

    let is_editing = editing.is_some();
    let title = if is_editing {
        "Edit SSH Connection"
    } else {
        "New SSH Connection"
    };
    let submit_label = if is_editing { "Save" } else { "Connect" };
    // Diagnostic: log every render where the dialog is visible. We need to
    // know whether app.rs passed `editing` for the session the user thinks
    // they're editing. If logs say `editing=none` while the user states they
    // clicked Edit on an existing connection, the wiring from sidebar →
    // modal → editing_conn → prop is broken upstream of this component.
    tracing::info!(
        "[CONN-DIALOG] render editing={} editing_id={} form.onekey={} form.name='{}'",
        if is_editing { "some" } else { "none" },
        editing_id,
        form().onekey,
        form().name
    );

    let auth_type = form().auth_type.clone();
    let is_password = auth_type == "password";
    let is_key = auth_type == "key";
    let is_agent = auth_type == "agent";
    let proxy_type = form().proxy_type.clone();
    let proxy_enabled = proxy_type != "none";
    let proxy_port_placeholder = match proxy_type.as_str() {
        "https" => "443",
        "socks5" => "1080",
        _ => "8080",
    };
    let show_proxy_settings = editing
        .as_ref()
        .map(|connection| matches!(connection.kind, ConnectionKind::Ssh(_)))
        .unwrap_or(true);

    // In edit mode, the password field is shown empty (we never echo the
    // stored password back into the DOM for security). A small hint tells the
    // user that leaving it blank keeps the existing password.
    let password_hint = is_editing && is_password;

    rsx! {
        div {
            style: "
                position: fixed;
                top: 0; left: 0; right: 0; bottom: 0;
                background: rgba(0,0,0,0.6);
                display: flex;
                justify-content: center;
                align-items: center;
                z-index: 1000;
            ",

            div {
                style: "
                    background: #24283b;
                    border-radius: 8px;
                    padding: 24px;
                    width: 480px;
                    max-height: 90vh;
                    overflow-y: auto;
                    color: #c0caf5;
                ",

                h3 { style: "margin: 0 0 16px; font-size: 16px;", "{title}" }

                div {
                    style: "display: flex; flex-direction: column; gap: 12px;",

                    // Name
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #565f89;", "Name" }
                        input {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            r#type: "text",
                            placeholder: "My Server",
                            value: "{form().name}",
                            oninput: move |e| form.write().name = e.value(),
                        }
                    }

                    // Group
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #565f89;", "Group" }
                        select {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            value: "{form().group_id.as_deref().unwrap_or_default()}",
                            onchange: move |e| {
                                let value = e.value();
                                form.write().group_id = (!value.is_empty()).then_some(value);
                            },
                            option { value: "", "Ungrouped" }
                            for group in groups.iter() {
                                option {
                                    value: "{group.id}",
                                    "{group.name}"
                                }
                            }
                        }
                    }

                    // One-Key Connect — placed directly under Name so it stays
                    // visible without scrolling in the (fixed-height) dialog.
                    //
                    // Event flow: we do NOT rely on the checkbox's `onchange`.
                    // In close_confirmation_dialog.rs we observed that Dioxus's
                    // checkbox `onchange` was unreliable in this WebView (the
                    // same pattern there has an explicit comment about letting
                    // the click bubble up to the parent instead), so here the
                    // clickable wrapper toggles the form field directly. The
                    // inner checkbox is purely visual (checked: reflects the
                    // form state); `pointer-events: none` lets the single
                    // onclick on the wrapper handle everything — meaning the
                    // user can click the checkbox, the label, or the hint text
                    // and the toggle always fires.
                    //
                    // Symptom being fixed: the previous `onchange: move |e| ...`
                    // never actually fired in the WebView, so `form.onekey`
                    // stayed `false` no matter how often the user toggled it,
                    // settings.json was written with `onekey: false`, and the
                    // resulting `disabled_for_session` skip made the OneKey
                    // popup never appear even after `sudo` printed the
                    // password prompt.
                    div {
                        style: "display: flex; align-items: center; gap: 8px; padding: 8px 10px; background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; cursor: pointer;",
                        onclick: move |_| {
                            let next = !form().onekey;
                            form.write().onekey = next;
                            tracing::info!("[ONEKEY] connection-dialog checkbox toggled onekey={}", next);
                        },
                        input {
                            r#type: "checkbox",
                            checked: form().onekey,
                            style: "pointer-events: none; cursor: pointer;",
                        }
                        div {
                            style: "display: flex; flex-direction: column; gap: 2px;",
                            label { style: "font-size: 12px; color: #9ece6a; cursor: pointer; pointer-events: none;", "One-Key Connect" }
                            span { style: "font-size: 11px; color: #565f89; line-height: 1.4; pointer-events: none;",
                                "Auto-suggest a matching OneKey credential when this host asks for a password." }
                        }
                    }

                    // Login initialization script (expect/send DSL). Optional; only
                    // SSH and Shell connections run it after login. See
                    // rusterm_core::parse_login_script for the grammar.
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px; margin-top: 8px;",
                        label {
                            style: "font-size: 12px; color: #565f89;",
                            "Login script (optional, expect/send DSL)"
                        }
                        textarea {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 12px; font-family: 'JetBrains Mono', monospace; outline: none; min-height: 80px; resize: vertical;",
                            placeholder: "expect [sudo] password for alice: $
send_onekey prod-sudo
send source /etc/profile.d/prod.sh
delay 250",
                            value: "{form().login_script}",
                            oninput: move |e| form.write().login_script = e.value(),
                        }
                        span {
                            style: "font-size: 10px; color: #565f89; line-height: 1.4;",
                            "Lines: expect <regex> | send <text> | send_onekey <name> | delay <ms>. Credentials are resolved from your unlocked OneKey library — never paste passwords here."
                        }
                    }

                    // Host + Port
                    div {
                        style: "display: flex; gap: 8px;",
                        div {
                            style: "flex: 3; display: flex; flex-direction: column; gap: 4px;",
                            label { style: "font-size: 12px; color: #565f89;", "Host" }
                            input {
                                style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                r#type: "text",
                                placeholder: "192.168.1.1",
                                // `list` attribute links this input to the
                                // `<datalist id=\"ssh-host-list\">` below, enabling
                                // the browser's native autocomplete dropdown.
                                list: "ssh-host-list",
                                value: "{form().host}",
                                oninput: move |e| form.write().host = e.value(),
                                // Auto-fill from `~/.ssh/config` when the user
                                // picks (or types a complete match for) a
                                // configured host alias. `onchange` fires on
                                // blur or when the user selects a datalist
                                // suggestion — NOT on every keystroke, so we
                                // don't interrupt typing.
                                onchange: move |e| {
                                    let alias = e.value();
                                    if let Some(resolved) = lookup_host(&alias, None) {
                                        let mut f = form.write();
                                        // Only overwrite fields that the
                                        // config actually specifies — leave
                                        // user-typed values alone for fields
                                        // the config doesn't set.
                                        // `lookup_host` returns resolved
                                        // values for user/port/identity_file
                                        // (with sane defaults when the
                                        // config doesn't set them), so we
                                        // always fill those.
                                        f.host = resolved.host;
                                        f.port = resolved.port.to_string();
                                        f.username = resolved.user;
                                        if let Some(id_path) = resolved.identity_file {
                                            f.key_path = id_path;
                                            f.auth_type = "key".to_string();
                                        } else {
                                            // No IdentityFile in the config —
                                            // fall back to agent auth (the
                                            // OpenSSH convention when no
                                            // IdentityFile is specified).
                                            f.auth_type = "agent".to_string();
                                        }
                                    }
                                },
                            }
                            // Path hint: tells the user where the suggestions
                            // come from (so they know to edit `~/.ssh/config`
                            // if a host is missing). Only shown when the
                            // config file exists / is readable.
                            {(ssh_config_path_display().is_some() && !host_suggestions().is_empty()).then(|| rsx! {
                                div {
                                    style: "font-size: 11px; color: #565f89; margin-top: 2px;",
                                    "提示：从 {ssh_config_path_display().as_deref().unwrap_or(\"~/.ssh/config\")} 读取到 {host_suggestions().len()} 个主机配置"
                                }
                            })}
                            // Datalist of host aliases from `~/.ssh/config`.
                            // The browser renders these as a native dropdown
                            // under the input as the user types. We include
                            // both the alias AND the resolved hostname (if
                            // set) as the value, so the user can see what
                            // they're picking. The `value` attribute is what
                            // gets filled into the input on selection; the
                            // text content is what's shown in the dropdown.
                            datalist {
                                id: "ssh-host-list",
                                for suggestion in host_suggestions().iter() {
                                    option {
                                        // `value` is what gets filled into
                                        // the input on selection. We use
                                        // the alias (not the resolved
                                        // hostname) because the user wants
                                        // to type/pick the alias, and the
                                        // `onchange` handler resolves it
                                        // via `lookup_host`.
                                        value: "{suggestion.alias}",
                                    }
                                }
                            }
                        }
                        div {
                            style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                            label { style: "font-size: 12px; color: #565f89;", "Port" }
                            input {
                                style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                r#type: "text",
                                placeholder: "22",
                                value: "{form().port}",
                                oninput: move |e| form.write().port = e.value(),
                            }
                        }
                    }

                    // Username
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #565f89;", "Username" }
                        input {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            r#type: "text",
                            placeholder: "root",
                            value: "{form().username}",
                            oninput: move |e| form.write().username = e.value(),
                        }
                    }

                    // Auth Type selector
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #565f89;", "Authentication" }
                        div {
                            style: "display: flex; gap: 4px;",

                            button {
                                style: if is_password {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "password".to_string(),
                                "Password"
                            }
                            button {
                                style: if is_key {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "key".to_string(),
                                "Key"
                            }
                            button {
                                style: if is_agent {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "agent".to_string(),
                                "Agent"
                            }
                        }
                    }

                    // Password field (shown when auth_type == "password")
                    {is_password.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 4px;",
                            label { style: "font-size: 12px; color: #565f89;", "Password" }
                            input {
                                style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                r#type: "password",
                                placeholder: if password_hint { "Leave blank to keep current password" } else { "Enter password" },
                                value: "{form().password}",
                                oninput: move |e| form.write().password = e.value(),
                            }
                        }
                    })}

                    // Key path + passphrase (shown when auth_type == "key")
                    {is_key.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 8px;",

                            div {
                                style: "display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #565f89;", "Private Key Path" }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "text",
                                    placeholder: "~/.ssh/id_rsa",
                                    // Link to the identity-file datalist
                                    // below — the browser will offer the
                                    // `~/.ssh/id_*` files it found as the
                                    // user types.
                                    list: "ssh-identity-list",
                                    value: "{form().key_path}",
                                    oninput: move |e| form.write().key_path = e.value(),
                                }
                                // Hint showing how many identity files were
                                // found in `~/.ssh/` (so the user knows the
                                // dropdown is populated).
                                {(!identity_suggestions().is_empty()).then(|| rsx! {
                                    div {
                                        style: "font-size: 11px; color: #565f89; margin-top: 2px;",
                                        "提示：从 ~/.ssh/ 找到 {identity_suggestions().len()} 个私钥文件"
                                    }
                                })}
                                // Datalist of identity files from `~/.ssh/`.
                                datalist {
                                    id: "ssh-identity-list",
                                    for path in identity_suggestions().iter() {
                                        option {
                                            value: "{path}",
                                        }
                                    }
                                }
                            }

                            div {
                                style: "display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #565f89;", "Passphrase (optional)" }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "password",
                                    placeholder: "Leave blank if no passphrase",
                                    value: "{form().passphrase}",
                                    oninput: move |e| form.write().passphrase = e.value(),
                                }
                            }
                        }
                    })}

                    // Agent hint
                    {is_agent.then(|| rsx! {
                        div {
                            style: "font-size: 12px; color: #565f89; padding: 8px; background: #1a1b26; border-radius: 4px; border: 1px solid #2a2b3d;",
                            "Will use SSH agent (ssh-agent) for authentication."
                        }
                    })}

                    // Optional per-connection proxy. HTTPS means TLS to the
                    // proxy server followed by HTTP CONNECT to the SSH target.
                    {show_proxy_settings.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 8px; padding: 10px; background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px;",
                            div {
                                style: "display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #565f89;", "Proxy" }
                                select {
                                    style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    value: "{form().proxy_type}",
                                    onchange: move |e| {
                                        let next = e.value();
                                        let mut current = form.write();
                                        let previous_default = match current.proxy_type.as_str() {
                                            "https" => "443",
                                            "socks5" => "1080",
                                            "http" => "8080",
                                            _ => "",
                                        };
                                        if current.proxy_port.is_empty() || current.proxy_port == previous_default {
                                            current.proxy_port = match next.as_str() {
                                                "https" => "443",
                                                "socks5" => "1080",
                                                "http" => "8080",
                                                _ => "",
                                            }
                                            .to_string();
                                        }
                                        current.proxy_type = next;
                                    },
                                    option { value: "none", "None (direct)" }
                                    option { value: "http", "HTTP CONNECT" }
                                    option { value: "https", "HTTPS CONNECT" }
                                    option { value: "socks5", "SOCKS5" }
                                }
                            }

                            {proxy_enabled.then(|| rsx! {
                                div {
                                    style: "display: flex; gap: 8px;",
                                    div {
                                        style: "flex: 3; display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "font-size: 12px; color: #565f89;", "Proxy Host" }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "text",
                                            placeholder: "proxy.example.com",
                                            value: "{form().proxy_host}",
                                            oninput: move |e| form.write().proxy_host = e.value(),
                                        }
                                    }
                                    div {
                                        style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "font-size: 12px; color: #565f89;", "Port" }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "text",
                                            placeholder: "{proxy_port_placeholder}",
                                            value: "{form().proxy_port}",
                                            oninput: move |e| form.write().proxy_port = e.value(),
                                        }
                                    }
                                }
                                div {
                                    style: "display: flex; gap: 8px;",
                                    div {
                                        style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "font-size: 12px; color: #565f89;", "Proxy Username (optional)" }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "text",
                                            value: "{form().proxy_username}",
                                            oninput: move |e| form.write().proxy_username = e.value(),
                                        }
                                    }
                                    div {
                                        style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "font-size: 12px; color: #565f89;", "Proxy Password (optional)" }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "password",
                                            value: "{form().proxy_password}",
                                            oninput: move |e| form.write().proxy_password = e.value(),
                                        }
                                    }
                                }
                                div {
                                    style: "font-size: 11px; color: #565f89; line-height: 1.4;",
                                    "Authentication requires both username and password. HTTPS secures the connection to the proxy before CONNECT."
                                }
                            })}
                        }
                    })}

                    // Terminal Type selector
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #565f89;", "Terminal Type" }
                        select {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            value: "{form().terminal_type}",
                            onchange: move |e| form.write().terminal_type = e.value(),
                            for term_type in TERMINAL_TYPES {
                                option {
                                    value: "{term_type}",
                                    selected: form().terminal_type == *term_type,
                                    "{term_type}"
                                }
                            }
                        }
                    }

                }

                div {
                    style: "display: flex; justify-content: flex-end; gap: 8px; margin-top: 20px;",
                    button {
                        style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        style: "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                        onclick: move |_| {
                            // CRITICAL diagnostic: we MUST see this logline
                            // every time the user clicks Save/Connect. If the
                            // dialog visually closes but no log appears, the
                            // WebView ate the click. If log shows editing=none
                            // while the user was definitely in edit mode, the
                            // `editing_conn` prop never got passed down.
                            tracing::info!(
                                "[CONN-DIALOG] submit clicked editing={} form.onekey={} form.name='{}'",
                                if editing.is_some() { "some" } else { "none" },
                                form().onekey,
                                form().name
                            );
                            if let Some(ref c) = editing {
                                // Edit mode: preserve the id so the existing
                                // entry is replaced. Non-form fields (tags,
                                // proxy_jump, keepalive_interval, and the whole
                                // kind for non-SSH) are preserved by
                                // `rebuild_connection` in app.rs.
                                on_edit.call((c.id.clone(), form()));
                            } else {
                                on_create.call(form());
                                form.set(default_form());
                            }
                        },
                        "{submit_label}"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusterm_core::config::{ProxyConfig, SshConfig};

    #[test]
    fn edit_form_restores_proxy_configuration() {
        let connection = ConnectionConfig {
            id: "proxied".to_string(),
            name: "Proxied SSH".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "ssh.example".to_string(),
                port: 22,
                username: "alice".to_string(),
                auth: SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy: Some(ProxyConfig {
                    kind: ProxyKind::Https,
                    host: "proxy.example".to_string(),
                    port: 443,
                    username: Some("proxy-user".to_string()),
                    password: Some("proxy-password".to_string()),
                }),
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: rusterm_core::config::default_host_key_policy(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: String::new(),
        };

        let form = form_from_connection(&connection);
        assert_eq!(form.proxy_type, "https");
        assert_eq!(form.proxy_host, "proxy.example");
        assert_eq!(form.proxy_port, "443");
        assert_eq!(form.proxy_username, "proxy-user");
        assert_eq!(form.proxy_password, "proxy-password");
    }
}
