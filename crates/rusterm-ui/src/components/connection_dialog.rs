use dioxus::prelude::*;

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
    pub onekey: bool,
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

#[component]
pub fn ConnectionDialog(
    visible: bool,
    on_close: EventHandler<()>,
    on_create: EventHandler<NewConnectionForm>,
) -> Element {
    let mut form = use_signal(|| NewConnectionForm {
        auth_type: "password".to_string(),
        terminal_type: "xterm-256color".to_string(),
        port: "22".to_string(),
        ..Default::default()
    });

    if !visible {
        return rsx! {};
    }

    let auth_type = form().auth_type.clone();
    let is_password = auth_type == "password";
    let is_key = auth_type == "key";
    let is_agent = auth_type == "agent";

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

                h3 { style: "margin: 0 0 16px; font-size: 16px;", "New SSH Connection" }

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
                                value: "{form().host}",
                                oninput: move |e| form.write().host = e.value(),
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
                                placeholder: "Enter password",
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
                                    value: "{form().key_path}",
                                    oninput: move |e| form.write().key_path = e.value(),
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

                    // One-key connect
                    div {
                        style: "display: flex; align-items: center; gap: 8px;",
                        input {
                            r#type: "checkbox",
                            checked: form().onekey,
                            onchange: move |e| form.write().onekey = e.checked(),
                        }
                        label { style: "font-size: 12px; color: #9ece6a; cursor: pointer;", "One-Key Connect" }
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
                            on_create.call(form());
                            form.set(NewConnectionForm {
                                auth_type: "password".to_string(),
                                terminal_type: "xterm-256color".to_string(),
                                port: "22".to_string(),
                                ..Default::default()
                            });
                        },
                        "Connect"
                    }
                }
            }
        }
    }
}
