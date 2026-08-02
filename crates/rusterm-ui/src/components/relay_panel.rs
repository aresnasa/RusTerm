//! REST relay settings modal (feature #63). Shows whether the relay is
//! running and on what URL, edits `relay.json`, manages accounts (BasicAuth
//! users with Argon2-hashed passwords), and gates the dangerous
//! `0.0.0.0` bind behind an inline confirmation.

use dioxus::prelude::*;
use rusterm_relay::{RelayAccount, hash_password};

use crate::components::{Icon, IconName};

#[component]
pub fn RelayPanel(state: Signal<crate::state::AppState>, on_close: EventHandler<()>) -> Element {
    let _lang = crate::i18n::LANGUAGE();
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
                    span { style: "font-size:16px;font-weight:600;", { crate::i18n::t("api.title") } }
                    button { class: "relay-btn", onclick: move |_| on_close.call(()), { crate::i18n::t("common.close") } }
                }

                div { class: "relay-body",
                    // ── status ──────────────────────────────────────────
                    if running() {
                        div { class: "relay-status ok",
                            { crate::i18n::tf(
                                                            "api.status_running",
                                                            &[("url", &started_url().clone().unwrap_or_default())],
                                                        ) }
                        }
                    } else {
                        div { class: "relay-status", style: "background:#24283b;color:#9aa5ce;",
                            { crate::i18n::t("api.status_stopped") }
                        }
                    }
                    if !status_msg().is_empty() {
                        div { class: "relay-status err", { crate::i18n::t(&status_msg()) } }
                    }

                    // ── server settings ─────────────────────────────────
                    div { class: "relay-sect", { crate::i18n::t("relay.server") } }

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
                            { crate::i18n::t("api.enable_on_startup") }
                        }
                    }
                    div { class: "relay-row",
                        span { style: "width:80px;", { crate::i18n::t("api.bind_addr") } }
                        input {
                            class: "relay-input",
                            style: "width:110px;",
                            value: "{bind_addr_str}",
                            oninput: move |e| {
                                if let Ok(ip) = e.value().parse::<std::net::IpAddr>() {
                                    config.write().bind_addr = ip;
                                } else {
                                    status_msg.set(crate::i18n::tf(
                                                                            "relay.invalid_bind_addr",
                                                                            &[("value", &e.value())],
                                                                        ));
                                }
                            },
                        }
                        span { { crate::i18n::t("api.port") } }
                        input {
                            class: "relay-input",
                            style: "width:70px;",
                            value: "{port_str}",
                            oninput: move |e| {
                                if let Ok(p) = e.value().parse::<u16>() {
                                    config.write().port = p;
                                    status_msg.set(String::new());
                                } else {
                                    status_msg.set(crate::i18n::tf(
                                                                            "api.invalid_port",
                                                                            &[("value", &e.value())],
                                                                        ));
                                }
                            },
                        }
                    }

                    if config.read().binds_publicly() && !confirm_public_bind() {
                        div { class: "relay-status err",
                            { crate::i18n::t("relay.public_bind_warning") }
                        }
                        button {
                            class: "relay-btn warn",
                            onclick: move |_| confirm_public_bind.set(true),
                            { crate::i18n::t("relay.confirm_public_bind") }
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
                                { crate::i18n::t("api.stop") }
                            }
                        } else {
                            button {
                                class: "relay-btn",
                                onclick: move |_| {
                                    // Start with what's in the form — that is
                                    // the user's mental model of "current".
                                    let cfg = config();
                                    if cfg.binds_publicly() && !confirm_public_bind() {
                                        status_msg.set("relay.confirm_public_bind_before_start".into());
                                        return;
                                    }
                                    if cfg.accounts.is_empty() {
                                        status_msg.set("relay.account_required_before_start".into());
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
                                { crate::i18n::t("api.start") }
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
                                    status_msg.set("api.saved".into());
                                }
                            },
                            { crate::i18n::t("relay.save_config") }
                        }
                    }

                    // ── accounts ────────────────────────────────────────
                    div { class: "relay-sect", { crate::i18n::t("api.accounts") } }
                    for account in config.read().accounts.iter().cloned() {
                        {
                            let username = account.username.clone();
                            let username_for_remove = username.clone();
                            rsx! {
                                div { class: "relay-account", key: "{username}",
                                    div { style: "display:flex;align-items:center;justify-content:space-between;",
                                        span { style: "font-weight:600;", "{username}" }
                                        if account.readonly {
                                            span { style: "font-size:10px;color:#7aa2f7;", { crate::i18n::t("api.readonly") } }
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
                                            crate::i18n::t("relay.all_commands_validated")
                                        } else {
                                            account.allowed_commands.join(", ")
                                        };
                                        rsx! {
                                            div { style: "font-size:10px;color:#9aa5ce;margin-top:3px;",
                                                { crate::i18n::tf("relay.hosts", &[("hosts", &hosts_text)]) }
                                            }
                                            div { style: "font-size:10px;color:#9aa5ce;",
                                                { crate::i18n::tf("relay.commands", &[("commands", &commands_text)]) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div { class: "relay-sect", { crate::i18n::t("relay.add_account") } }
                    div { class: "relay-field",
                        span { { crate::i18n::t("api.username") } }
                        input {
                            class: "relay-input",
                            value: "{new_username()}",
                            oninput: move |e| new_username.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { { crate::i18n::t("relay.password_hash_note") } }
                        input {
                            class: "relay-input",
                            r#type: "password",
                            value: "{new_password()}",
                            oninput: move |e| new_password.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { { crate::i18n::t("relay.allowed_hosts_help") } }
                        input {
                            class: "relay-input",
                            placeholder: "prod-web-1, ci-runner",
                            value: "{new_allowed_hosts()}",
                            oninput: move |e| new_allowed_hosts.set(e.value()),
                        }
                    }
                    div { class: "relay-field",
                        span { { crate::i18n::t("relay.allowed_commands_help") } }
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
                        { crate::i18n::t("relay.readonly_help") }
                    }

                    if !account_error().is_empty() {
                        div { class: "relay-status err", { crate::i18n::t(&account_error()) } }
                    }

                    button {
                        class: "relay-btn",
                        onclick: move |_| {
                            let username = new_username().trim().to_string();
                            let password = new_password();
                            if username.is_empty() {
                                account_error.set("relay.username_required".into());
                                return;
                            }
                            if password.is_empty() {
                                account_error.set("relay.password_required".into());
                                return;
                            }
                            let password_hash = match hash_password(&password) {
                                Ok(h) => h,
                                Err(e) => {
                                    account_error.set(crate::i18n::tf(
                                                                            "relay.hashing_failed",
                                                                            &[("error", &e)],
                                                                        ));
                                    return;
                                }
                            };
                            let allowed_hosts = split_csv(&new_allowed_hosts());
                            let allowed_commands_raw = split_csv(&new_allowed_commands());
                            match rusterm_relay::compile_allowlist(&allowed_commands_raw) {
                                Ok(_) => {}
                                Err(errors) => {
                                    let indices = format!(
                                        "{:?}",
                                        errors.iter().map(|(i, _)| *i).collect::<Vec<_>>()
                                    );
                                    account_error.set(crate::i18n::tf(
                                        "relay.invalid_regex_indices",
                                        &[("indices", &indices)],
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
                        { crate::i18n::t("relay.add_update_account") }
                    }
                }

                div { style: "padding:10px 16px;border-top:1px solid #2a2b3d;font-size:10px;color:#9aa5ce;",
                    {
                        let audit_dir = rusterm_core::logging::log_dir().display().to_string();
                        crate::i18n::tf(
                                                    "relay.audit_log",
                                                    &[("path", &format!("{audit_dir}/relay-audit.jsonl"))],
                                                )
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
