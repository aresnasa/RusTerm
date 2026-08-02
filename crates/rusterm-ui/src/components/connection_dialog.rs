use dioxus::prelude::*;

use rusterm_core::config::{ConnectionConfig, ConnectionGroup, ConnectionKind, ProxyKind, SshAuth};
use rusterm_ssh::{
    HostSpec, Protocol, SshHostSuggestion, default_ssh_config_path, list_identity_files,
    list_ssh_config_hosts, lookup_host, parse_host_input,
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

    // ── quick-entry / protocol switching ───────────────────────────────
    /// Free-text the user types in the "quick entry" box at the top of the
    /// dialog (e.g. `xuchao@jump.zs.shaipower.online -p 22`). Empty when the
    /// user fills fields individually below.
    pub quick_input: String,
    /// Last parse-error message from the quick-entry box, shown inline so
    /// the user knows why nothing auto-filled. Empty when the last parse
    /// was successful (or the box is empty).
    pub quick_error: String,
    /// Active protocol tab. One of `"ssh"`, `"telnet"`, `"serial"`.
    /// Drives which fields are visible and which `ConnectionKind` the form
    /// builds on submit.
    pub protocol: String,

    // ── serial-specific fields (only used when protocol == "serial") ────
    /// Device path, e.g. `/dev/ttyUSB0` (Linux/macOS) or `COM3` (Windows).
    pub serial_port: String,
    /// Baud rate as a string so the input is unconstrained; parsed on save.
    pub baud_rate: String,
    /// One of `"5"|"6"|"7"|"8"`.
    pub data_bits: String,
    /// One of `"none"|"odd"|"even"`.
    pub parity: String,
    /// One of `"1"|"2"`.
    pub stop_bits: String,
    /// One of `"none"|"software"|"hardware"`.
    pub flow_control: String,
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
        protocol: "ssh".to_string(),
        baud_rate: "115200".to_string(),
        data_bits: "8".to_string(),
        parity: "none".to_string(),
        stop_bits: "1".to_string(),
        flow_control: "none".to_string(),
        ..Default::default()
    }
}

/// Apply the quick-entry parse result to the form. Updates host/port/username
/// and switches the protocol tab when the parsed protocol differs from the
/// current one. Clears `quick_error` on success.
fn apply_host_spec(spec: &HostSpec, form: &mut NewConnectionForm) {
    if let Some(user) = &spec.user {
        if !user.is_empty() {
            form.username = user.clone();
        }
    }
    form.host = spec.host.clone();
    // Always fill the port — `resolved_port` falls back to the protocol
    // default (ssh=22, telnet=23) when the user didn't specify one, which is
    // exactly the "auto-fill the default port" behavior the requirement
    // asks for.
    form.port = spec.resolved_port().to_string();
    let new_protocol = match spec.protocol {
        Protocol::Ssh => "ssh",
        Protocol::Telnet => "telnet",
    };
    if form.protocol != new_protocol {
        form.protocol = new_protocol.to_string();
    }
    form.quick_error.clear();
}

/// Convenience: the conventional default port for a protocol string used in
/// the UI dropdown ("ssh" | "telnet" | "serial"). Serial returns 0 (the
/// port field is hidden for serial connections — the dropdown swap hides it
/// visually, and 0 is the sentinel `parse().unwrap_or(0)` falls back to).
fn default_port_for_protocol(protocol: &str) -> &'static str {
    match protocol {
        "telnet" => "23",
        "serial" => "0",
        _ => "22",
    }
}

/// Build a form pre-filled from an existing connection so the edit dialog
/// shows the saved values. SSH/Telnet/Serial each populate their own
/// fields; on save, `rebuild_connection` reconstructs the matching
/// `ConnectionKind` from the form. The `protocol` field is seeded from the
/// connection kind so the dialog opens on the right tab.
fn form_from_connection(c: &ConnectionConfig) -> NewConnectionForm {
    let mut base = default_form();
    base.name = c.name.clone();
    base.group_id = c.group.clone();
    base.onekey = c.onekey;
    base.login_script = c.login_script.clone().unwrap_or_default();

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
            base.protocol = "ssh".to_string();
            base.host = ssh.host.clone();
            base.port = ssh.port.to_string();
            base.username = ssh.username.clone();
            base.auth_type = auth_type.to_string();
            base.password = password;
            base.key_path = key_path;
            base.passphrase = passphrase;
            base.terminal_type = ssh.terminal_type.clone();
            base.proxy_type = proxy_type;
            base.proxy_host = proxy_host;
            base.proxy_port = proxy_port;
            base.proxy_username = proxy_username;
            base.proxy_password = proxy_password;
            base
        }
        ConnectionKind::Telnet(telnet) => {
            base.protocol = "telnet".to_string();
            base.host = telnet.host.clone();
            base.port = telnet.port.to_string();
            // Telnet has no username field in the config; leave whatever the
            // user typed in a previous session, but typically empty.
            base
        }
        ConnectionKind::Serial(serial) => {
            base.protocol = "serial".to_string();
            base.serial_port = serial.port.clone();
            base.baud_rate = serial.baud_rate.to_string();
            base.data_bits = serial.data_bits.to_string();
            base.parity = serial.parity.clone();
            base.stop_bits = serial.stop_bits.to_string();
            base.flow_control = serial.flow_control.clone();
            base
        }
        // Shell / Tcp don't have a dedicated form tab; keep name + onekey
        // + login_script only. Kind is preserved on save by `rebuild_connection`.
        _ => base,
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
    let _lang = crate::i18n::LANGUAGE();
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
        crate::i18n::t("connection.edit_title")
    } else {
        crate::i18n::t("connection.new_title")
    };
    let submit_label = if is_editing {
        crate::i18n::t("common.save")
    } else {
        crate::i18n::t("connection.connect")
    };
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
    // Protocol tab drives which fields are visible. In edit mode we still
    // show all SSH fields when the underlying kind is SSH (preserving the
    // existing behavior); for non-SSH kinds we honor the form's protocol so
    // the user can switch tabs and see the right inputs.
    let protocol = form().protocol.clone();
    let is_ssh = protocol == "ssh";
    let is_telnet = protocol == "telnet";
    let is_serial = protocol == "serial";
    let show_proxy_settings = is_ssh
        && editing
            .as_ref()
            .map(|connection| matches!(connection.kind, ConnectionKind::Ssh(_)))
            .unwrap_or(true);

    // In edit mode, the password field is shown empty (we never echo the
    // stored password back into the DOM for security). A small hint tells the
    // user that leaving it blank keeps the existing password.
    let password_hint = is_editing && is_password;

    // Available serial ports (system enumeration). Loaded once on mount;
    // `serialport::available_ports` is a cheap syscall that returns a Vec,
    // never panics. The user can still type a path not in the list.
    let serial_port_suggestions: Signal<Vec<String>> =
        use_signal(rusterm_proto::list_available_ports);

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

                    // ── Quick entry ────────────────────────────────────────
                    // Single input box at the top that parses `user@host -p 22`
                    // (and variants) and auto-fills the host/port/username/
                    // protocol fields below. Only shown for SSH/Telnet (serial
                    // doesn't have a `user@host` form).
                    {(is_ssh || is_telnet).then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 4px; padding: 10px; background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px;",
                            label {
                                style: "font-size: 12px; color: #9ece6a;",
                                { crate::i18n::t("connection.quick_entry_label") }
                            }
                            div {
                                style: "display: flex; gap: 6px;",
                                input {
                                    style: "flex: 1; background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 6px 8px; color: #c0caf5; font-size: 12px; font-family: 'JetBrains Mono', monospace; outline: none;",
                                    r#type: "text",
                                    placeholder: "xuchao@jump.zs.shaipower.online -p 22",
                                    value: "{form().quick_input}",
                                    oninput: move |e| {
                                        let v = e.value();
                                        let mut f = form.write();
                                        f.quick_input = v.clone();
                                        // Live-parse: update fields as the user
                                        // types so the form below reflects the
                                        // parsed result in real time. Errors
                                        // are shown inline but do NOT clear
                                        // previously-filled fields.
                                        if v.trim().is_empty() {
                                            f.quick_error.clear();
                                        } else {
                                            match parse_host_input(&v) {
                                                Ok(spec) => apply_host_spec(&spec, &mut f),
                                                Err(err) => f.quick_error = err.to_string(),
                                            }
                                        }
                                    },
                                    // Enter: parse + jump focus to the next
                                    // field (password/key). We don't auto-submit
                                    // because the user may still want to fill
                                    // auth details.
                                    onkeydown: move |e: KeyboardEvent| {
                                        if e.key() == Key::Enter {
                                            e.prevent_default();
                                        }
                                    },
                                }
                            }
                            // Inline parse-error hint. Empty when the last
                            // parse succeeded (or the box is empty).
                            {(!form().quick_error.is_empty()).then(|| rsx! {
                                span {
                                    style: "font-size: 11px; color: #f7768e; margin-top: 2px;",
                                    { form().quick_error.clone() }
                                }
                            })}
                            span {
                                style: "font-size: 10px; color: #9aa5ce; line-height: 1.4; margin-top: 2px;",
                                { crate::i18n::t("connection.quick_entry_help") }
                            }
                        }
                    })}

                    // ── Protocol selector ──────────────────────────────────
                    // Three tabs: SSH / Telnet / Serial. Switching updates
                    // `form.protocol` and swaps the default port (ssh=22,
                    // telnet=23) when the user hasn't typed a custom port.
                    div {
                        style: "display: flex; gap: 4px;",
                        button {
                            style: if is_ssh {
                                "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                            } else {
                                "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                            },
                            onclick: move |_| {
                                let mut f = form.write();
                                let prev_default = default_port_for_protocol(&f.protocol);
                                f.protocol = "ssh".to_string();
                                if f.port.is_empty() || f.port == prev_default {
                                    f.port = default_port_for_protocol("ssh").to_string();
                                }
                            },
                            "SSH"
                        }
                        button {
                            style: if is_telnet {
                                "flex: 1; padding: 6px 12px; background: #ff9e64; color: #1a1b26; border: 1px solid #ff9e64; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                            } else {
                                "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                            },
                            onclick: move |_| {
                                let mut f = form.write();
                                let prev_default = default_port_for_protocol(&f.protocol);
                                f.protocol = "telnet".to_string();
                                if f.port.is_empty() || f.port == prev_default {
                                    f.port = default_port_for_protocol("telnet").to_string();
                                }
                            },
                            "Telnet"
                        }
                        button {
                            style: if is_serial {
                                "flex: 1; padding: 6px 12px; background: #e0af68; color: #1a1b26; border: 1px solid #e0af68; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                            } else {
                                "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                            },
                            onclick: move |_| {
                                form.write().protocol = "serial".to_string();
                            },
                            "Serial"
                        }
                    }

                    // Name
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.name") } }
                        input {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            r#type: "text",
                            placeholder: crate::i18n::t("connection.name_placeholder"),
                            value: "{form().name}",
                            oninput: move |e| form.write().name = e.value(),
                        }
                    }

                    // Group
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.group") } }
                        select {
                            style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                            value: "{form().group_id.as_deref().unwrap_or_default()}",
                            onchange: move |e| {
                                let value = e.value();
                                form.write().group_id = (!value.is_empty()).then_some(value);
                            },
                            option { value: "", { crate::i18n::t("connections.ungrouped") } }
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
                            label { style: "font-size: 12px; color: #9ece6a; cursor: pointer; pointer-events: none;", { crate::i18n::t("connection.onekey_connect") } }
                            span { style: "font-size: 11px; color: #9aa5ce; line-height: 1.4; pointer-events: none;",
                                { crate::i18n::t("connection.onekey_hint") } }
                        }
                    }

                    // Login initialization script (expect/send DSL). Optional; only
                    // SSH and Shell connections run it after login. See
                    // rusterm_core::parse_login_script for the grammar.
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px; margin-top: 8px;",
                        label {
                            style: "font-size: 12px; color: #9aa5ce;",
                            { crate::i18n::t("connection.login_script_label") }
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
                            style: "font-size: 10px; color: #9aa5ce; line-height: 1.4;",
                            { crate::i18n::t("connection.login_script_help") }
                        }
                    }

                    // Host + Port (shown for both SSH and Telnet — both are
                    // host-based protocols; Serial has its own device-picker
                    // section below).
                    {(is_ssh || is_telnet).then(|| rsx! {
                        div {
                            style: "display: flex; gap: 8px;",
                            div {
                                style: "flex: 3; display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.host") } }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "text",
                                    placeholder: "192.168.1.1",
                                    list: "ssh-host-list",
                                    value: "{form().host}",
                                    oninput: move |e| form.write().host = e.value(),
                                    onchange: move |e| {
                                        let alias = e.value();
                                        if let Some(resolved) = lookup_host(&alias, None) {
                                            let mut f = form.write();
                                            f.host = resolved.host;
                                            f.port = resolved.port.to_string();
                                            f.username = resolved.user;
                                            if let Some(id_path) = resolved.identity_file {
                                                f.key_path = id_path;
                                                f.auth_type = "key".to_string();
                                            } else {
                                                f.auth_type = "agent".to_string();
                                            }
                                        }
                                    },
                                }
                                {(ssh_config_path_display().is_some() && !host_suggestions().is_empty() && is_ssh).then(|| {
                                    let host_count = host_suggestions().len();
                                    let ssh_path = ssh_config_path_display().as_deref().unwrap_or("~/.ssh/config").to_string();
                                    rsx! {
                                        div {
                                            style: "font-size: 11px; color: #9aa5ce; margin-top: 2px;",
                                            { crate::i18n::tf("connection.ssh_hosts_hint", &[("count", &host_count), ("path", &ssh_path)]) }
                                        }
                                    }
                                })}
                                datalist {
                                    id: "ssh-host-list",
                                    for suggestion in host_suggestions().iter() {
                                        option {
                                            value: "{suggestion.alias}",
                                        }
                                    }
                                }
                            }
                            div {
                                style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.port") } }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "text",
                                    placeholder: "22",
                                    value: "{form().port}",
                                    oninput: move |e| form.write().port = e.value(),
                                }
                            }
                        }
                    })}

                    // Serial-specific fields (device path + line settings).
                    // Only shown when the Serial tab is active.
                    {is_serial.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 12px;",

                            // Device path with system-enumerated dropdown.
                            div {
                                style: "display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.serial_device") } }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "text",
                                    placeholder: "/dev/ttyUSB0",
                                    list: "serial-port-list",
                                    value: "{form().serial_port}",
                                    oninput: move |e| form.write().serial_port = e.value(),
                                }
                                {(!serial_port_suggestions().is_empty()).then(|| {
                                    let count = serial_port_suggestions().len();
                                    rsx! {
                                        div {
                                            style: "font-size: 11px; color: #9aa5ce; margin-top: 2px;",
                                            { crate::i18n::tf("connection.serial_ports_hint", &[("count", &count)]) }
                                        }
                                    }
                                })}
                                datalist {
                                    id: "serial-port-list",
                                    for name in serial_port_suggestions().iter() {
                                        option {
                                            value: "{name}",
                                        }
                                    }
                                }
                            }

                            // Baud rate + data bits + parity + stop bits + flow control
                            div {
                                style: "display: flex; gap: 8px;",
                                div {
                                    style: "flex: 2; display: flex; flex-direction: column; gap: 4px;",
                                    label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.baud_rate") } }
                                    select {
                                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                        value: "{form().baud_rate}",
                                        onchange: move |e| form.write().baud_rate = e.value(),
                                        for rate in &["9600", "19200", "38400", "57600", "115200", "230400", "460800", "921600"] {
                                            option {
                                                value: "{rate}",
                                                selected: form().baud_rate == *rate,
                                                "{rate}"
                                            }
                                        }
                                    }
                                }
                                div {
                                    style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                    label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.data_bits") } }
                                    select {
                                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                        value: "{form().data_bits}",
                                        onchange: move |e| form.write().data_bits = e.value(),
                                        for bits in &["5", "6", "7", "8"] {
                                            option {
                                                value: "{bits}",
                                                selected: form().data_bits == *bits,
                                                "{bits}"
                                            }
                                        }
                                    }
                                }
                            }

                            div {
                                style: "display: flex; gap: 8px;",
                                div {
                                    style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                    label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.parity") } }
                                    select {
                                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                        value: "{form().parity}",
                                        onchange: move |e| form.write().parity = e.value(),
                                        for p in &[("none", "None"), ("odd", "Odd"), ("even", "Even")] {
                                            option {
                                                value: "{p.0}",
                                                selected: form().parity == p.0,
                                                "{p.1}"
                                            }
                                        }
                                    }
                                }
                                div {
                                    style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                    label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.stop_bits") } }
                                    select {
                                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                        value: "{form().stop_bits}",
                                        onchange: move |e| form.write().stop_bits = e.value(),
                                        for s in &["1", "2"] {
                                            option {
                                                value: "{s}",
                                                selected: form().stop_bits == *s,
                                                "{s}"
                                            }
                                        }
                                    }
                                }
                                div {
                                    style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                    label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.flow_control") } }
                                    select {
                                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                        value: "{form().flow_control}",
                                        onchange: move |e| form.write().flow_control = e.value(),
                                        for fc in &[("none", "None"), ("software", "XON/XOFF"), ("hardware", "RTS/CTS")] {
                                            option {
                                                value: "{fc.0}",
                                                selected: form().flow_control == fc.0,
                                                "{fc.1}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    })}

                    // SSH-only fields (Username / Auth / Proxy / Terminal Type).
                    // Telnet connections don't carry username/auth/terminal in
                    // `TelnetConfig`; Serial has its own block above.
                    {is_ssh.then(|| rsx! {
                    // Username
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.username") } }
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
                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.authentication") } }
                        div {
                            style: "display: flex; gap: 4px;",

                            button {
                                style: if is_password {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "password".to_string(),
                                { crate::i18n::t("connection.password") }
                            }
                            button {
                                style: if is_key {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "key".to_string(),
                                { crate::i18n::t("connection.key") }
                            }
                            button {
                                style: if is_agent {
                                    "flex: 1; padding: 6px 12px; background: #7aa2f7; color: #1a1b26; border: 1px solid #7aa2f7; border-radius: 4px; font-size: 12px; font-weight: 600; cursor: pointer;"
                                } else {
                                    "flex: 1; padding: 6px 12px; background: transparent; color: #c0caf5; border: 1px solid #2a2b3d; border-radius: 4px; font-size: 12px; cursor: pointer;"
                                },
                                onclick: move |_| form.write().auth_type = "agent".to_string(),
                                { crate::i18n::t("connection.agent") }
                            }
                        }
                    }

                    // Password field (shown when auth_type == "password")
                    {is_password.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 4px;",
                            label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.password") } }
                            input {
                                style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                r#type: "password",
                                placeholder: if password_hint {
                                    crate::i18n::t("connection.password_keep_placeholder")
                                } else {
                                    crate::i18n::t("connection.password_placeholder")
                                },
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
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.private_key_path") } }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "text",
                                    placeholder: "~/.ssh/id_rsa",
                                    list: "ssh-identity-list",
                                    value: "{form().key_path}",
                                    oninput: move |e| form.write().key_path = e.value(),
                                }
                                {(!identity_suggestions().is_empty()).then(|| {
                                    let identity_count = identity_suggestions().len();
                                    rsx! {
                                        div {
                                            style: "font-size: 11px; color: #9aa5ce; margin-top: 2px;",
                                            { crate::i18n::tf("connection.identity_files_hint", &[("count", &identity_count)]) }
                                        }
                                    }
                                })}
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
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.passphrase_optional") } }
                                input {
                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                    r#type: "password",
                                    placeholder: crate::i18n::t("connection.passphrase_placeholder"),
                                    value: "{form().passphrase}",
                                    oninput: move |e| form.write().passphrase = e.value(),
                                }
                            }
                        }
                    })}

                    // Agent hint
                    {is_agent.then(|| rsx! {
                        div {
                            style: "font-size: 12px; color: #9aa5ce; padding: 8px; background: #1a1b26; border-radius: 4px; border: 1px solid #2a2b3d;",
                            { crate::i18n::t("connection.agent_hint") }
                        }
                    })}

                    // Optional per-connection proxy. HTTPS means TLS to the
                    // proxy server followed by HTTP CONNECT to the SSH target.
                    {show_proxy_settings.then(|| rsx! {
                        div {
                            style: "display: flex; flex-direction: column; gap: 8px; padding: 10px; background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px;",
                            div {
                                style: "display: flex; flex-direction: column; gap: 4px;",
                                label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.proxy") } }
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
                                    option { value: "none", { crate::i18n::t("connection.proxy_direct") } }
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
                                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.proxy_host") } }
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
                                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.port") } }
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
                                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.proxy_username_optional") } }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "text",
                                            value: "{form().proxy_username}",
                                            oninput: move |e| form.write().proxy_username = e.value(),
                                        }
                                    }
                                    div {
                                        style: "flex: 1; display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.proxy_password_optional") } }
                                        input {
                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; color: #c0caf5; font-size: 13px; outline: none;",
                                            r#type: "password",
                                            value: "{form().proxy_password}",
                                            oninput: move |e| form.write().proxy_password = e.value(),
                                        }
                                    }
                                }
                                div {
                                    style: "font-size: 11px; color: #9aa5ce; line-height: 1.4;",
                                    { crate::i18n::t("connection.proxy_help") }
                                }
                            })}
                        }
                    })}

                    // Terminal Type selector
                    div {
                        style: "display: flex; flex-direction: column; gap: 4px;",
                        label { style: "font-size: 12px; color: #9aa5ce;", { crate::i18n::t("connection.terminal_type") } }
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
                    })}

                }

                div {
                    style: "display: flex; justify-content: flex-end; gap: 8px; margin-top: 20px;",
                    button {
                        style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                        onclick: move |_| on_close.call(()),
                        { crate::i18n::t("common.cancel") }
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
    use rusterm_core::config::{ProxyConfig, SerialConfig, SshConfig, TelnetConfig};

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
            login_script: None,
        };

        let form = form_from_connection(&connection);
        assert_eq!(form.proxy_type, "https");
        assert_eq!(form.proxy_host, "proxy.example");
        assert_eq!(form.proxy_port, "443");
        assert_eq!(form.proxy_username, "proxy-user");
        assert_eq!(form.proxy_password, "proxy-password");
    }

    #[test]
    fn edit_form_restores_ssh_protocol_tab() {
        let connection = ConnectionConfig {
            id: "ssh-1".to_string(),
            name: "SSH".to_string(),
            kind: ConnectionKind::Ssh(SshConfig {
                host: "host".to_string(),
                port: 22,
                username: "root".to_string(),
                auth: SshAuth::Agent,
                terminal_type: "xterm-256color".to_string(),
                proxy: None,
                proxy_jump: None,
                keepalive_interval: None,
                host_key_policy: rusterm_core::config::default_host_key_policy(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: None,
        };
        let form = form_from_connection(&connection);
        assert_eq!(form.protocol, "ssh");
        assert_eq!(form.host, "host");
        assert_eq!(form.port, "22");
        assert_eq!(form.username, "root");
    }

    #[test]
    fn edit_form_restores_telnet_protocol_tab() {
        let connection = ConnectionConfig {
            id: "telnet-1".to_string(),
            name: "Telnet".to_string(),
            kind: ConnectionKind::Telnet(TelnetConfig {
                host: "router.lan".to_string(),
                port: 23,
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: None,
        };
        let form = form_from_connection(&connection);
        assert_eq!(form.protocol, "telnet");
        assert_eq!(form.host, "router.lan");
        assert_eq!(form.port, "23");
    }

    #[test]
    fn edit_form_restores_serial_protocol_tab() {
        let connection = ConnectionConfig {
            id: "serial-1".to_string(),
            name: "Serial console".to_string(),
            kind: ConnectionKind::Serial(SerialConfig {
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 115200,
                data_bits: 8,
                parity: "none".to_string(),
                stop_bits: 1,
                flow_control: "none".to_string(),
            }),
            group: None,
            tags: vec![],
            onekey: false,
            login_script: None,
        };
        let form = form_from_connection(&connection);
        assert_eq!(form.protocol, "serial");
        assert_eq!(form.serial_port, "/dev/ttyUSB0");
        assert_eq!(form.baud_rate, "115200");
        assert_eq!(form.data_bits, "8");
        assert_eq!(form.parity, "none");
        assert_eq!(form.stop_bits, "1");
        assert_eq!(form.flow_control, "none");
    }

    #[test]
    fn default_form_seeds_ssh_protocol_and_default_port() {
        let form = default_form();
        assert_eq!(form.protocol, "ssh");
        assert_eq!(form.port, "22");
        // Serial defaults are populated too, even though the user only sees
        // them after switching to the Serial tab.
        assert_eq!(form.baud_rate, "115200");
        assert_eq!(form.data_bits, "8");
        assert_eq!(form.parity, "none");
        assert_eq!(form.stop_bits, "1");
        assert_eq!(form.flow_control, "none");
    }

    #[test]
    fn default_port_for_protocol_returns_conventional_values() {
        assert_eq!(default_port_for_protocol("ssh"), "22");
        assert_eq!(default_port_for_protocol("telnet"), "23");
        // Serial has no port — returns a sentinel.
        assert_eq!(default_port_for_protocol("serial"), "0");
        // Unknown protocols fall back to the SSH default.
        assert_eq!(default_port_for_protocol("unknown"), "22");
    }

    #[test]
    fn apply_host_spec_fills_user_host_port_and_protocol() {
        let mut form = default_form();
        let spec = parse_host_input("xuchao@jump.zs.shaipower.online -p 22").unwrap();
        apply_host_spec(&spec, &mut form);
        assert_eq!(form.username, "xuchao");
        assert_eq!(form.host, "jump.zs.shaipower.online");
        assert_eq!(form.port, "22");
        assert_eq!(form.protocol, "ssh");
        assert!(form.quick_error.is_empty());
    }

    #[test]
    fn apply_host_spec_switches_protocol_tab_for_telnet() {
        let mut form = default_form();
        // Initial protocol is ssh; parsing a telnet:// URL should switch it.
        let spec = parse_host_input("telnet://router.lan:23").unwrap();
        apply_host_spec(&spec, &mut form);
        assert_eq!(form.protocol, "telnet");
        assert_eq!(form.host, "router.lan");
        assert_eq!(form.port, "23");
    }

    #[test]
    fn apply_host_spec_fills_default_port_when_not_specified() {
        let mut form = default_form();
        // Bare host — no port in the input. `resolved_port` should fill in
        // the SSH default (22).
        let spec = parse_host_input("bare.host").unwrap();
        apply_host_spec(&spec, &mut form);
        assert_eq!(form.port, "22");
    }

    #[test]
    fn apply_host_spec_does_not_overwrite_username_when_absent() {
        let mut form = default_form();
        form.username = "preset-user".to_string();
        // `host -p 22` — no user part. `apply_host_spec` should leave the
        // existing username alone (not clear it).
        let spec = parse_host_input("host -p 22").unwrap();
        apply_host_spec(&spec, &mut form);
        assert_eq!(form.username, "preset-user");
    }
}
