use dioxus::prelude::*;

use rusterm_core::config::{OneKey, OneKeyStep};

/// Modal dialog for managing the OneKey library. Each OneKey is a named
/// sequence of Expect/Send steps (ZOC-style multi-expect). Edits a local copy;
/// "OK" persists the whole list via `on_save` (each step's `send` is encrypted).
#[component]
pub fn OneKeyManager(
    onekeys: Vec<OneKey>,
    on_close: EventHandler<()>,
    on_save: EventHandler<Vec<OneKey>>,
) -> Element {
    let mut entries = use_signal(|| onekeys.clone());
    let mut selected = use_signal(|| None::<usize>);

    let input_style = "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; \
                       padding: 7px; color: #c0caf5; font-size: 13px; outline: none; width: 100%; \
                       box-sizing: border-box;";
    let label_style = "font-size: 11px; color: #565f89;";

    rsx! {
        div {
            style: "position: fixed; top: 0; left: 0; right: 0; bottom: 0; \
                    background: rgba(0,0,0,0.6); display: flex; justify-content: center; \
                    align-items: center; z-index: 1500;",
            onclick: move |_| on_close.call(()),

            div {
                style: "background: #24283b; border-radius: 8px; padding: 20px; width: 760px; \
                        max-height: 88vh; display: flex; flex-direction: column; color: #c0caf5;",
                onclick: move |e: Event<MouseData>| e.stop_propagation(),

                h3 { style: "margin: 0 0 6px; font-size: 16px;", "OneKeys (Expect / Send steps)" }
                p { style: "margin: 0 0 6px; font-size: 12px; color: #565f89; line-height: 1.5;",
                    "Each OneKey is a sequence of Expect/Send steps. When terminal output matches a \
                     step's Expect (regex), that step's Send is offered. e.g. a Username step then a \
                     Password step. Send values are encrypted at rest." }
                p { style: "margin: 0 0 14px; font-size: 11px; color: #414868; line-height: 1.5;",
                    "Common expects — git HTTPS: \
                     `Username for \\S+:` and `password for \\S+:` \
                     (note: git prints `Password for 'host': ` so a bare `password:` will NOT match). \
                     SSH password: `password:`. Bastion login: `Password for \\S+:`." }

                div {
                    style: "display: flex; gap: 12px; flex: 1; min-height: 360px;",

                    // Left: list of OneKeys
                    div {
                        style: "width: 200px; display: flex; flex-direction: column; \
                                background: #1a1b26; border-radius: 4px; border: 1px solid #2a2b3d;",
                        div {
                            style: "flex: 1; overflow-y: auto;",
                            for (i, ok) in entries().iter().enumerate() {
                                {
                                    let is_sel = selected() == Some(i);
                                    let bg = if is_sel { "#283457" } else { "transparent" };
                                    let i_clone = i;
                                    rsx! {
                                        div {
                                            key: "{ok.id}",
                                            style: "padding: 7px 10px; cursor: pointer; font-size: 13px; \
                                                    background: {bg}; border-bottom: 1px solid #2a2b3d; \
                                                    white-space: nowrap; overflow: hidden; text-overflow: ellipsis;",
                                            onclick: move |_| selected.set(Some(i_clone)),
                                            "{ok.name}"
                                            span { style: "color:#565f89;font-size:10px;margin-left:6px;", {format!("({} steps)", ok.steps.len())} }
                                        }
                                    }
                                }
                            }
                            if entries().is_empty() {
                                div { style: "padding: 16px 10px; color: #565f89; font-size: 12px;",
                                    "No OneKeys yet.\nClick + to add one." }
                            }
                        }
                        button {
                            style: "margin: 8px; padding: 6px; background: #7aa2f7; color: #1a1b26; \
                                    border: none; border-radius: 4px; cursor: pointer; font-size: 12px; \
                                    font-weight: 600;",
                            onclick: move |_| {
                                // Default new OneKey ships with BOTH a Username and
                                // a Password step pre-configured for git HTTPS auth
                                // (the most common use case). A bare `password:`
                                // expect does NOT match git's actual prompt
                                // `Password for 'host': ` (the `for 'host'` sits
                                // between "Password" and ":"), so the popup would
                                // never fire for the password step — the user
                                // would have to type it manually. Pre-filling the
                                // correct expects avoids that foot-gun.
                                let new_ok = OneKey {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    name: "new".to_string(),
                                    steps: vec![
                                        OneKeyStep {
                                            label: "Username".to_string(),
                                            expect: r"Username for \S+:".to_string(),
                                            send: String::new(),
                                        },
                                        OneKeyStep {
                                            label: "Password".to_string(),
                                            expect: r"password for \S+:".to_string(),
                                            send: String::new(),
                                        },
                                    ],
                                };
                                entries.write().push(new_ok);
                                selected.set(Some(entries().len() - 1));
                            },
                            "+ Add OneKey"
                        }
                    }

                    // Right: form for the selected OneKey (name + steps)
                    div {
                        style: "flex: 1; display: flex; flex-direction: column; gap: 10px; overflow-y: auto;",

                        if let Some(idx) = selected() {
                            if entries().get(idx).is_some() {
                                {rsx! {
                                    div {
                                        style: "display: flex; flex-direction: column; gap: 4px;",
                                        label { style: "{label_style}", "Name" }
                                        input {
                                            style: "{input_style}",
                                            r#type: "text",
                                            placeholder: "ecs-user / git-inesa",
                                            value: "{entries.read()[idx].name}",
                                            oninput: move |e| entries.write()[idx].name = e.value(),
                                        }
                                    }

                                    div {
                                        style: "display: flex; justify-content: space-between; align-items: center; margin-top: 4px;",
                                        span { style: "{label_style}", "Expect / Send steps" }
                                        button {
                                            style: "padding: 3px 8px; background: transparent; color: #7aa2f7; \
                                                    border: 1px solid #2a2b3d; border-radius: 4px; cursor: pointer; font-size: 11px;",
                                            onclick: move |_| {
                                                entries.write()[idx].steps.push(OneKeyStep {
                                                    label: "Password".to_string(),
                                                    // Matches git's "Password for 'https://…': " prompt
                                                    // (case-insensitive). A bare "password:" would NOT
                                                    // match "Password for …" (the "for" is in between).
                                                    expect: r"password for \S+:".to_string(),
                                                    send: String::new(),
                                                });
                                            },
                                            "+ Step"
                                        }
                                    }

                                    for (si, _step) in entries.read()[idx].steps.iter().enumerate() {
                                        {
                                            let step_idx = si;
                                            rsx! {
                                                div {
                                                    key: "{step_idx}",
                                                    style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; padding: 8px; display: flex; flex-direction: column; gap: 6px;",
                                                    div {
                                                        style: "display: flex; gap: 6px; align-items: center;",
                                                        input {
                                                            style: "background: #16161e; border: 1px solid #2a2b3d; border-radius: 3px; padding: 5px; color: #9ece6a; font-size: 12px; outline: none; width: 120px;",
                                                            r#type: "text",
                                                            placeholder: "label (Username)",
                                                            value: "{entries.read()[idx].steps[step_idx].label}",
                                                            oninput: move |e| entries.write()[idx].steps[step_idx].label = e.value(),
                                                        }
                                                        button {
                                                            style: "margin-left: auto; background: transparent; color: #f7768e; border: none; cursor: pointer; font-size: 14px; padding: 0 4px;",
                                                            title: "Remove step",
                                                            onclick: move |_| {
                                                                entries.write()[idx].steps.remove(step_idx);
                                                            },
                                                            "×"
                                                        }
                                                    }
                                                    input {
                                                        style: "{input_style}",
                                                        r#type: "text",
                                                        placeholder: r"Expect (regex): Username for \S+:",
                                                        value: "{entries.read()[idx].steps[step_idx].expect}",
                                                        oninput: move |e| entries.write()[idx].steps[step_idx].expect = e.value(),
                                                    }
                                                    input {
                                                        style: "{input_style}",
                                                        r#type: "password",
                                                        placeholder: "Send (secret — encrypted)",
                                                        value: "{entries.read()[idx].steps[step_idx].send}",
                                                        oninput: move |e| entries.write()[idx].steps[step_idx].send = e.value(),
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    button {
                                        style: "align-self: flex-start; margin-top: 4px; padding: 6px 12px; \
                                                background: transparent; color: #f7768e; border: 1px solid #f7768e; \
                                                border-radius: 4px; cursor: pointer; font-size: 12px;",
                                        onclick: move |_| {
                                            entries.write().remove(idx);
                                            selected.set(None);
                                        },
                                        "Delete OneKey"
                                    }
                                }}
                            }
                        } else {
                            div { style: "color: #565f89; font-size: 13px; padding: 20px 0; \
                                          text-align: center; flex: 1; display: flex; align-items: center; \
                                          justify-content: center;",
                                "Select a OneKey, or click + Add OneKey to create one." }
                        }
                    }
                }

                // Footer
                div {
                    style: "display: flex; justify-content: flex-end; gap: 8px; margin-top: 16px;",
                    button {
                        style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; \
                                border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        style: "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; \
                                padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                        onclick: move |_| {
                            on_save.call(entries());
                            on_close.call(());
                        },
                        "OK"
                    }
                }
            }
        }
    }
}
