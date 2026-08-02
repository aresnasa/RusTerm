//! REST relay settings modal (feature #63). Shows whether the relay is
//! running and on what URL, edits `relay.json`, manages accounts (BasicAuth
//! users with Argon2-hashed passwords), and gates the dangerous
//! `0.0.0.0` bind behind an inline confirmation.

use dioxus::prelude::*;
use rusterm_relay::{RelayAccount, hash_password};

use crate::components::{Icon, IconName};

#[component]
pub fn RelayPanel(state: Signal<crate::state::AppState>, on_close: EventHandler<()>) -> Element {
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
    let mut confirm_public_bind = use_signal(|| false);
    // New-account scratch form.
    let mut new_username = use_signal(String::new);
    let mut new_password = use_signal(String::new);
    let mut new_allowed_hosts = use_signal(String::new);
    let mut new_allowed_commands = use_signal(String::new);
    let mut new_readonly = use_signal(|| false);
    let mut account_error = use_signal(String::new);

    let (enabled, bind_addr_str, port_str) = {
        let cfg = config.read();
        (cfg.enabled, cfg.bind_addr.to_string(), cfg.port.to_string())
    };

    rsx! {
        style { r#"
            .relay-overlay{{position:fixed;inset:0;background:rgba(0,0,0,.6);display:flex;align-items:center;justify-content:center;z-index:1000;}}
            .relay-card{{width:min(720px,92vw);max-height:86vh;background:#24283b;color:#c0caf5;border:1px solid #2a2b3d;border-radius:8px;display:flex;flex-direction:column;overflow:hidden;box-shadow:0 12px 40px rgba(0,0,0,.35);}}
            .relay-head{{display:flex;align-items:center;justify-content:space-between;padding:12px 16px;border-bottom:1px solid #2a2b3d;}}
            .relay-body{{flex:1;min-height:0;overflow-y:auto;padding:14px 16px;}}
            .relay-row{{display:flex;align-items:center;gap:10px;margin-bottom:12px;font-size:12px;}}
            .relay-input{{padding:5px 7px;border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:12px;}}
            .relay-btn{{border:1px solid #2a2b3d;border-radius:4px;background:#1a1b26;color:#c0caf5;font-size:11px;padding:4px 10px;cursor:pointer;}}
            .relay-btn:hover{{border-color:#7aa2f7;color:#7aa2f7;}}
            .relay-btn.warn{{border-color:#f7768e;color:#f7768e;}}
            .relay-btn.primary{{background:#7aa2f7;color:#1a1b26;border:1px solid #7aa2f7;font-weight:600;}}
            .relay-status{{font-size:11px;padding:6px 10px;border-radius:4px;margin-bottom:12px;}}
            .relay-status.ok{{background:rgba(76,175,80,.12);color:#4caf50;}}
            .relay-status.err{{background:rgba(229,57,53,.12);color:#e53935;}}
            .relay-account{{background:#1a1b26;border:1px solid #2a2b3d;border-radius:6px;padding:8px 10px;margin-bottom:6px;font-size:12px;}}
            .relay-sect{{font-size:11px;font-weight:600;color:#9aa5ce;text-transform:uppercase;letter-spacing:.5px;margin:14px 0 6px;}}
            .relay-field{{display:flex;flex-direction:column;gap:3px;margin-bottom:8px;font-size:11px;}}
        "# }

        div { class: "relay-overlay", onclick: move |_| on_close.call(()),
            div { class: "relay-card", onclick: move |e| e.stop_propagation(),
                div { class: "relay-head",
                    span { style: "font-size:16px;font-weight:600;", "REST API Relay" }
                    button { class: "relay-btn", onclick: move |_| on_close.call(()), "Close" }
                }

                div { class: "relay-body",
                    // ── status ──────────────────────────────────────────
                    if running() {
                        div { class: "relay-status ok",
                            "Running at {started_url().clone().unwrap_or_default()}"
                        }
                    } else {
                        div { class: "relay-status", style: "background:#24283b;color:#9aa5ce;",
                            "Stopped"
                        }
                    }
                    if !status_msg().is_empty() {
                        div { class: "relay-status err", "{status_msg()}" }
                    }

                    // ── server settings ─────────────────────────────────
                    div { class: "relay-sect", "Server" }

                    div { class: "relay-row",
                        label {
                            style: "display:flex;align-items:center;gap:6px;font-size:12px;",
                            input {
                                r#type: "checkbox",
                                checked: enabled,
                                onchange: move |e| {
                                    config.write().enabled = e.checked();
                                },
                            }
                            "Enable relay on startup"
                        }
                    }
                    div { class: "relay-row",
                        span { style: "width:80px;", "Bind addr" }
                        input {
                            class: "relay-input",
                            style: "width:110px;",
                            value: "{bind_addr_str}",
                            oninput: move |e| {
                                if let Ok(ip) = e.value().parse::<std::net::IpAddr>() {
                                    config.write().bind_addr = ip;
                                } else {
                                    status_msg.set(format!("Invalid bind addr: {}", e.value()));
                                }
                            },
                        }
                        span { "Port" }
                        input {
                            class: "relay-input",
                            style: "width:70px;",
                            value: "{port_str}",
                            oninput: move |e| {
                                if let Ok(p) = e.value().parse::<u16>() {
                                    config.write().port = p;
                                    status_msg.set(String::new());
                                } else {
                                    status_msg.set(format!("Invalid port: {}", e.value()));
                                }
                            },
                        }
                    }

                    if config.read().binds_publicly() && !confirm_public_bind() {
                        div { class: "relay-status err",
                            "Binding to a non-loopback address exposes the API on the network. Confirm you understand the risk."
                        }
                        button {
                            class: "relay-btn warn",
                            onclick: move |_| confirm_public_bind.set(true),
                            "I understand — allow public bind"
                        }
                    }

                    // ── lifecycle ───────────────────────────────────────
                    div { class: "relay-row",
                        if running() {
                            button {
                                class: "relay-btn",
                                onclick: move |_| {
                                    // Persist whatever is currently in the
                                    // form so the next startup picks it up.
                                    let cfg = config();
                                    if let Err(e) = cfg.save() {
                                        status_msg.set(e.to_string());
                                    }
                                    state.write().relay_config = cfg;
                                    let runtime = state.read().relay_runtime.clone();
                                    crate::relay_tunnel::stop_relay(runtime);
                                    running.set(false);
                                },
                                "Stop"
                            }
                        } else {
                            button {
                                class: "relay-btn",
                                onclick: move |_| {
                                    // Start with what's in the form — that is
                                    // the user's mental model of "current".
                                    let cfg = config();
                                    if cfg.binds_publicly() && !confirm_public_bind() {
                                        status_msg.set("Confirm public bind before starting".into());
                                        return;
                                    }
                                    if cfg.accounts.is_empty() {
                                        status_msg.set("Add at least one account before starting".into());
                                        return;
                                    }
                                    if let Err(e) = cfg.save() {
                                        status_msg.set(e.to_string());
                                        return;
                                    }
                                    state.write().relay_config = cfg.clone();
                                    let runtime = state.read().relay_runtime.clone();
                                    match crate::relay_tunnel::start_relay(cfg, runtime) {
                                        Ok(()) => {
                                            running.set(true);
                                            status_msg.set(String::new());
                                            started_url.set(
                                                state
                                                    .read()
                                                    .relay_runtime
                                                    .0
                                                    .read()
                                                    .ok()
                                                    .and_then(|g| g.as_ref().map(|h| h.url())),
                                            );
                                        }
                                        Err(e) => status_msg.set(e),
                                    }
                                },
                                "Start"
                            }
                        }
                        button {
                            class: "relay-btn primary",
                            onclick: move |_| {
                                // Persist current edits without toggling the server.
                                let cfg = config();
                                if let Err(e) = cfg.save() {
                                    status_msg.set(e.to_string());
                                } else {
                                    state.write().relay_config = cfg;
                                    status_msg.set("Saved to relay.json".into());
                                }
                            },
                            "Save config"
                        }
                    }

                    // ── accounts ────────────────────────────────────────
                    div { class: "relay-sect", "Accounts (BasicAuth)" }
                    for account in config.read().accounts.iter().cloned() {
                        {
                            let username = account.username.clone();
                            let username_for_remove = username.clone();
                            rsx! {
                                div { class: "relay-account", key: "{username}",
                                    div { style: "display:flex;align-items:center;justify-content:space-between;",
                                        span { style: "font-weight:600;", "{username}" }
                                        if account.readonly {
                                            span { style: "font-size:10px;color:#7aa2f7;", "read-only" }
                                        }
                                        button {
                                            class: "relay-btn",
                                            onclick: move |_| {
                                                config.write().accounts.retain(|a| a.username != username_for_remove);
                                            },
                                            Icon { name: IconName::Delete, size: 12 }
                                        }
                                    }
                                    {
                                        let hosts_text = if account.allowed_hosts.is_empty() {
                                            "*".to_string()
                                        } else {
                                            account.allowed_hosts.join(", ")
                                        };
                                        let commands_text = if account.allowed_commands.is_empty() {
                                            "* (validated)".to_string()
                                        } else {
                                            account.allowed_commands.join(", ")
                                        };
                                        rsx! {
                                            div { style: "font-size:10px;color:#9aa5ce;margin-top:3px;",
                                                "hosts: {hosts_text}"
                                            }
                                            div { style: "font-size:10px;color:#9aa5ce;",
                                                "commands: {commands_text}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div { class: "relay-sect", "Add account" }
                    div { class: "relay-field",
                        span { "Username" }
                        input {
                            class: "relay-input",
                            value: "{new_username()}",
                            oninput: move |e| new_username.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { "Password (stored only as Argon2id hash)" }
                        input {
                            class: "relay-input",
                            r#type: "password",
                            value: "{new_password()}",
                            oninput: move |e| new_password.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { "Allowed hosts (comma-separated ids/names, empty = all)" }
                        input {
                            class: "relay-input",
                            placeholder: "prod-web-1, ci-runner",
                            value: "{new_allowed_hosts()}",
                            oninput: move |e| new_allowed_hosts.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { "Allowed commands (comma-separated regex, empty = all non-dangerous)" }
                        input {
                            class: "relay-input",
                            placeholder: r"^docker\s+(ps|logs)",
                            value: "{new_allowed_commands()}",
                            oninput: move |e| new_allowed_commands.set(e.value()),
                        }
                    }
                    label { style: "display:flex;align-items:center;gap:6px;font-size:12px;margin-bottom:8px;",
                        input {
                            r#type: "checkbox",
                            checked: new_readonly(),
                            onchange: move |e| new_readonly.set(e.checked()),
                        }
                        "Read-only (reject any mutating command)"
                    }

                    if !account_error().is_empty() {
                        div { class: "relay-status err", "{account_error()}" }
                    }

                    button {
                        class: "relay-btn",
                        onclick: move |_| {
                            let username = new_username().trim().to_string();
                            let password = new_password();
                            if username.is_empty() {
                                account_error.set("Username is required".into());
                                return;
                            }
                            if password.is_empty() {
                                account_error.set("Password is required".into());
                                return;
                            }
                            let password_hash = match hash_password(&password) {
                                Ok(h) => h,
                                Err(e) => {
                                    account_error.set(format!("hashing failed: {e}"));
                                    return;
                                }
                            };
                            let allowed_hosts = split_csv(&new_allowed_hosts());
                            let allowed_commands_raw = split_csv(&new_allowed_commands());
                            match rusterm_relay::compile_allowlist(&allowed_commands_raw) {
                                Ok(_) => {}
                                Err(errors) => {
                                    account_error.set(format!(
                                        "Invalid regex(es) at index: {:?}",
                                        errors.iter().map(|(i, _)| *i).collect::<Vec<_>>()
                                    ));
                                    return;
                                }
                            }
                            let new_account = RelayAccount {
                                username: username.clone(),
                                password_hash,
                                allowed_hosts,
                                allowed_commands: allowed_commands_raw,
                                readonly: new_readonly(),
                            };
                            {
                                let mut cfg = config.write();
                                cfg.accounts.retain(|a| a.username != username);
                                cfg.accounts.push(new_account);
                            }
                            // Persist accounts immediately (start can wait).
                            if let Err(e) = config.read().save() {
                                account_error.set(e.to_string());
                            } else {
                                account_error.set(String::new());
                                new_username.set(String::new());
                                new_password.set(String::new());
                                new_allowed_hosts.set(String::new());
                                new_allowed_commands.set(String::new());
                            }
                        },
                        "Add / update account"
                    }
                }

                div { style: "padding:10px 16px;border-top:1px solid #2a2b3d;font-size:10px;color:#9aa5ce;",
                    {
                        let audit_dir = rusterm_core::logging::log_dir().display().to_string();
                        format!("Audit log: {audit_dir}/relay-audit.jsonl — every auth failure and command execution is recorded.")
                    }
                }
            }
        }
    }
}

fn split_csv(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}
