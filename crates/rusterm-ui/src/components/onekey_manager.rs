use std::collections::HashSet;

use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

use rusterm_core::config::{
    DEFAULT_ONEKEY_PASSWORD_EXPECT, DEFAULT_ONEKEY_USERNAME_EXPECT, OneKey, OneKeyStep,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpectPreset {
    Password,
    Username,
    Custom,
}

fn expect_preset(expect: &str) -> ExpectPreset {
    match expect.trim() {
        DEFAULT_ONEKEY_PASSWORD_EXPECT => ExpectPreset::Password,
        DEFAULT_ONEKEY_USERNAME_EXPECT => ExpectPreset::Username,
        _ => ExpectPreset::Custom,
    }
}

fn expect_mode_key(onekey_id: &str, step_index: usize) -> String {
    format!("{onekey_id}:{step_index}")
}

fn validate_onekeys(onekeys: &[OneKey]) -> Option<String> {
    for (entry_index, onekey) in onekeys.iter().enumerate() {
        let entry_name = if onekey.name.trim().is_empty() {
            format!("OneKey #{}", entry_index + 1)
        } else {
            format!("OneKey ‘{}’", onekey.name.trim())
        };
        if onekey.name.trim().is_empty() {
            return Some(format!("{entry_name} needs a name."));
        }
        if onekey.steps.is_empty() {
            return Some(format!(
                "{entry_name} needs at least one Expect / Send step."
            ));
        }
        for (step_index, step) in onekey.steps.iter().enumerate() {
            let step_name = if step.label.trim().is_empty() {
                format!("step #{}", step_index + 1)
            } else {
                format!("step ‘{}’", step.label.trim())
            };
            if step.expect.trim().is_empty() {
                return Some(format!("{entry_name} {step_name} needs an Expect regex."));
            }
            if let Err(error) = regex::Regex::new(&format!("(?i){}", step.expect)) {
                return Some(format!(
                    "{entry_name} {step_name} has an invalid Expect regex: {error}"
                ));
            }
            if step.send.is_empty() {
                return Some(format!("{entry_name} {step_name} needs a Send value."));
            }
        }
    }
    None
}

const MIN_VISIBLE_DIALOG_WIDTH: f64 = 48.0;
const MIN_VISIBLE_TITLE_HEIGHT: f64 = 44.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct DialogPosition {
    left: f64,
    top: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DialogBounds {
    viewport_width: f64,
    viewport_height: f64,
    dialog_width: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DialogDrag {
    pointer_start_x: f64,
    pointer_start_y: f64,
    dialog_start: DialogPosition,
    moved: bool,
}

fn clamp_dialog_position(position: DialogPosition, bounds: DialogBounds) -> DialogPosition {
    let visible_width = MIN_VISIBLE_DIALOG_WIDTH.min(bounds.dialog_width);
    let min_left = visible_width - bounds.dialog_width;
    let max_left = (bounds.viewport_width - visible_width).max(min_left);
    let max_top = (bounds.viewport_height - MIN_VISIBLE_TITLE_HEIGHT).max(0.0);

    DialogPosition {
        left: position.left.clamp(min_left, max_left),
        top: position.top.clamp(0.0, max_top),
    }
}

fn moved_dialog_position(
    drag: DialogDrag,
    pointer_x: f64,
    pointer_y: f64,
    bounds: DialogBounds,
) -> DialogPosition {
    clamp_dialog_position(
        DialogPosition {
            left: drag.dialog_start.left + pointer_x - drag.pointer_start_x,
            top: drag.dialog_start.top + pointer_y - drag.pointer_start_y,
        },
        bounds,
    )
}

fn parse_dialog_geometry(value: &str) -> Option<(DialogPosition, DialogBounds)> {
    let values = value
        .split(',')
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if values.len() != 5 || values[2] <= 0.0 || values[3] <= 0.0 || values[4] <= 0.0 {
        return None;
    }

    Some((
        DialogPosition {
            left: values[0],
            top: values[1],
        },
        DialogBounds {
            dialog_width: values[2],
            viewport_width: values[3],
            viewport_height: values[4],
        },
    ))
}

/// Modal dialog for managing the OneKey library. Each OneKey is a named
/// sequence of Expect/Send steps (ZOC-style multi-expect). Edits a local copy;
/// "OK" persists the whole list via `on_save` (each step's `send` is encrypted).
#[component]
pub fn OneKeyManager(
    onekeys: Vec<OneKey>,
    on_close: EventHandler<()>,
    on_save: EventHandler<Vec<OneKey>>,
) -> Element {
    let initial_custom_expect_steps = onekeys
        .iter()
        .flat_map(|onekey| {
            onekey
                .steps
                .iter()
                .enumerate()
                .filter(|(_, step)| expect_preset(&step.expect) == ExpectPreset::Custom)
                .map(|(index, _)| expect_mode_key(&onekey.id, index))
        })
        .collect::<HashSet<_>>();
    let mut entries = use_signal(|| onekeys.clone());
    let mut selected = use_signal(|| (!onekeys.is_empty()).then_some(0));
    let mut custom_expect_steps = use_signal(|| initial_custom_expect_steps);
    let mut show_send_values = use_signal(|| false);
    let mut dialog_position = use_signal(|| None::<DialogPosition>);
    let mut dialog_bounds = use_signal(|| None::<DialogBounds>);
    let mut dialog_drag = use_signal(|| None::<DialogDrag>);
    let mut suppress_overlay_click = use_signal(|| false);

    let input_style = "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 4px; \
                       padding: 7px; color: #c0caf5; font-size: 13px; outline: none; width: 100%; \
                       box-sizing: border-box;";
    let label_style = "font-size: 11px; color: #565f89;";
    let send_input_type = if show_send_values() {
        "text"
    } else {
        "password"
    };
    let send_toggle_label = if show_send_values() {
        "Hide Send values"
    } else {
        "Show Send values"
    };
    let validation_error = validate_onekeys(&entries());
    let can_save = validation_error.is_none();
    let positioned_style = dialog_position()
        .map(|position| {
            format!(
                "position:fixed;left:{:.2}px;top:{:.2}px;",
                position.left, position.top
            )
        })
        .unwrap_or_default();
    let dialog_style = format!(
        "{positioned_style}background:#24283b;border-radius:8px;padding:20px;width:760px;\
         max-height:88vh;display:flex;flex-direction:column;color:#c0caf5;outline:none;"
    );

    rsx! {
        div {
            style: "position: fixed; top: 0; left: 0; right: 0; bottom: 0; \
                    background: rgba(0,0,0,0.6); display: flex; justify-content: center; \
                    align-items: center; z-index: 1500;",
            onclick: move |_| {
                if suppress_overlay_click() {
                    suppress_overlay_click.set(false);
                } else {
                    on_close.call(());
                }
            },
            onmousemove: move |e: MouseEvent| {
                let (Some(mut drag), Some(bounds)) = (dialog_drag(), dialog_bounds()) else {
                    return;
                };
                e.prevent_default();
                let pointer = e.client_coordinates();
                if (pointer.x - drag.pointer_start_x).abs() > 1.0
                    || (pointer.y - drag.pointer_start_y).abs() > 1.0
                {
                    drag.moved = true;
                    dialog_drag.set(Some(drag));
                }
                dialog_position.set(Some(moved_dialog_position(
                    drag, pointer.x, pointer.y, bounds,
                )));
            },
            onmouseup: move |e: MouseEvent| {
                let Some(drag) = dialog_drag() else {
                    return;
                };
                e.prevent_default();
                dialog_drag.set(None);
                if drag.moved {
                    suppress_overlay_click.set(true);
                    spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
                        suppress_overlay_click.set(false);
                    });
                }
            },

            div {
                id: "onekey-manager-dialog",
                style: "{dialog_style}",
                tabindex: "0",
                onmounted: move |_| {
                    spawn(async move {
                        let result = dioxus::document::eval(
                            "return (function() {\
                                const el = document.getElementById('onekey-manager-dialog');\
                                if (!el) return '';\
                                el.focus();\
                                const rect = el.getBoundingClientRect();\
                                return rect.left.toFixed(2) + ',' + rect.top.toFixed(2) + ',' +\
                                    rect.width.toFixed(2) + ',' + window.innerWidth.toFixed(2) + ',' +\
                                    window.innerHeight.toFixed(2);\
                            })()"
                        ).await;
                        if let Ok(value) = result {
                            if let Some((position, bounds)) =
                                value.as_str().and_then(parse_dialog_geometry)
                            {
                                dialog_bounds.set(Some(bounds));
                                dialog_position.set(Some(clamp_dialog_position(position, bounds)));
                            }
                        }
                    });
                },
                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    e.stop_propagation();
                    if matches!(e.key(), Key::Escape) {
                        e.prevent_default();
                        on_close.call(());
                    } else if matches!(e.key(), Key::Enter)
                        && (e.modifiers().ctrl() || e.modifiers().meta())
                        && validate_onekeys(&entries()).is_none()
                    {
                        e.prevent_default();
                        on_save.call(entries());
                    }
                },

                div {
                    style: "display:flex;align-items:center;justify-content:space-between;gap:12px;margin:-8px -8px 6px;padding:8px;cursor:move;user-select:none;-webkit-user-select:none;",
                    title: "Drag to move OneKey Manager",
                    onmousedown: move |e: MouseEvent| {
                        if e.trigger_button() != Some(MouseButton::Primary) {
                            return;
                        }
                        let (Some(position), Some(_bounds)) =
                            (dialog_position(), dialog_bounds())
                        else {
                            return;
                        };
                        e.prevent_default();
                        e.stop_propagation();
                        let pointer = e.client_coordinates();
                        dialog_drag.set(Some(DialogDrag {
                            pointer_start_x: pointer.x,
                            pointer_start_y: pointer.y,
                            dialog_start: position,
                            moved: false,
                        }));
                    },
                    h3 { style: "margin:0;font-size:16px;", "OneKeys (Expect / Send steps)" }
                    button {
                        r#type: "button",
                        style: "background:transparent;border:1px solid #2a2b3d;border-radius:4px;color:#7aa2f7;padding:4px 8px;cursor:pointer;font-size:11px;",
                        title: "Temporarily reveal Send values so they can be verified",
                        onmousedown: move |e: MouseEvent| e.stop_propagation(),
                        onclick: move |_| show_send_values.toggle(),
                        "{send_toggle_label}"
                    }
                }
                p { style: "margin: 0 0 6px; font-size: 12px; color: #565f89; line-height: 1.5;",
                    "Each OneKey is a sequence of prompt/Send steps. Choose a built-in prompt type for \
                     common Username, Password, sudo, Git, bastion, and SSH key passphrase prompts. \
                     Send values are encrypted at rest." }
                p { style: "margin: 0 0 14px; font-size: 11px; color: #414868; line-height: 1.5;",
                    "Use Custom regex only for unusual prompts. Matching is case-insensitive and runs \
                     only for connections with One-Key Connect enabled — set it in the connection's \
                     Edit dialog (checkbox right under Name)." }

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
                                // New OneKeys cover Git's Username prompt plus the
                                // common Password prompt forms emitted by sudo, SSH,
                                // Git HTTPS, and bastion hosts.
                                let new_ok = OneKey {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    name: "new".to_string(),
                                    steps: vec![
                                        OneKeyStep {
                                            label: "Username".to_string(),
                                            expect: DEFAULT_ONEKEY_USERNAME_EXPECT.to_string(),
                                            send: String::new(),
                                        },
                                        OneKeyStep {
                                            label: "Password".to_string(),
                                            expect: DEFAULT_ONEKEY_PASSWORD_EXPECT.to_string(),
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
                                                    // Covers bare and qualified password prompts.
                                                    expect: DEFAULT_ONEKEY_PASSWORD_EXPECT.to_string(),
                                                    send: String::new(),
                                                });
                                            },
                                            "+ Step"
                                        }
                                    }

                                    for (si, _step) in entries.read()[idx].steps.iter().enumerate() {
                                        {
                                            let step_idx = si;
                                            let mode_key = expect_mode_key(
                                                &entries.read()[idx].id,
                                                step_idx,
                                            );
                                            let preset = expect_preset(
                                                &entries.read()[idx].steps[step_idx].expect,
                                            );
                                            let is_custom_expect = preset == ExpectPreset::Custom
                                                || custom_expect_steps.read().contains(&mode_key);
                                            let expect_mode = if is_custom_expect {
                                                "custom"
                                            } else if preset == ExpectPreset::Username {
                                                "username"
                                            } else {
                                                "password"
                                            };
                                            let mode_key_for_change = mode_key.clone();
                                            let mode_prefix_for_remove = format!(
                                                "{}:",
                                                entries.read()[idx].id,
                                            );
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
                                                                custom_expect_steps
                                                                    .write()
                                                                    .retain(|key| !key.starts_with(&mode_prefix_for_remove));
                                                            },
                                                            "×"
                                                        }
                                                    }
                                                    select {
                                                        style: "{input_style}",
                                                        value: "{expect_mode}",
                                                        onchange: move |e| {
                                                            match e.value().as_str() {
                                                                "password" => {
                                                                    entries.write()[idx].steps[step_idx].expect =
                                                                        DEFAULT_ONEKEY_PASSWORD_EXPECT.to_string();
                                                                    custom_expect_steps
                                                                        .write()
                                                                        .remove(&mode_key_for_change);
                                                                }
                                                                "username" => {
                                                                    entries.write()[idx].steps[step_idx].expect =
                                                                        DEFAULT_ONEKEY_USERNAME_EXPECT.to_string();
                                                                    custom_expect_steps
                                                                        .write()
                                                                        .remove(&mode_key_for_change);
                                                                }
                                                                "custom" => {
                                                                    custom_expect_steps
                                                                        .write()
                                                                        .insert(mode_key_for_change.clone());
                                                                }
                                                                _ => {}
                                                            }
                                                        },
                                                        option {
                                                            value: "password",
                                                            selected: expect_mode == "password",
                                                            "Password prompt (recommended)"
                                                        }
                                                        option {
                                                            value: "username",
                                                            selected: expect_mode == "username",
                                                            "Git username prompt"
                                                        }
                                                        option {
                                                            value: "custom",
                                                            selected: expect_mode == "custom",
                                                            "Custom regex (advanced)"
                                                        }
                                                    }
                                                    if is_custom_expect {
                                                        input {
                                                            style: "{input_style}",
                                                            r#type: "text",
                                                            placeholder: "Custom Expect regex",
                                                            value: "{entries.read()[idx].steps[step_idx].expect}",
                                                            oninput: move |e| entries.write()[idx].steps[step_idx].expect = e.value(),
                                                        }
                                                    } else {
                                                        div {
                                                            style: "font-size: 11px; color: #565f89; line-height: 1.4;",
                                                            if expect_mode == "password" {
                                                                "Matches Password, sudo, Git/bastion password, and SSH key passphrase prompts."
                                                            } else {
                                                                "Matches Git HTTPS username prompts."
                                                            }
                                                        }
                                                    }
                                                    input {
                                                        style: "{input_style}",
                                                        r#type: "{send_input_type}",
                                                        placeholder: "Send (secret — encrypted)",
                                                        autocomplete: "off",
                                                        spellcheck: "false",
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
                if let Some(error) = validation_error.as_ref() {
                    div {
                        style: "color:#f7768e;font-size:11px;margin-top:12px;line-height:1.4;",
                        "{error}"
                    }
                }
                div {
                    style: "display: flex; justify-content: flex-end; align-items:center; gap: 8px; margin-top: 12px;",
                    span { style: "margin-right:auto;color:#414868;font-size:10px;", "Esc Cancel · Ctrl/Cmd+Enter Save" }
                    button {
                        style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; \
                                border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    button {
                        style: if can_save {
                            "background:#7aa2f7;border:none;color:#1a1b26;border-radius:4px;padding:8px 16px;cursor:pointer;font-size:13px;font-weight:600;"
                        } else {
                            "background:#2a2b3d;border:none;color:#565f89;border-radius:4px;padding:8px 16px;cursor:not-allowed;font-size:13px;"
                        },
                        disabled: !can_save,
                        onclick: move |_| {
                            if can_save {
                                on_save.call(entries());
                            }
                        },
                        "OK"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DialogBounds, DialogDrag, DialogPosition, ExpectPreset, clamp_dialog_position,
        expect_preset, moved_dialog_position, parse_dialog_geometry, validate_onekeys,
    };
    use rusterm_core::config::{
        DEFAULT_ONEKEY_PASSWORD_EXPECT, DEFAULT_ONEKEY_USERNAME_EXPECT, OneKey, OneKeyStep,
    };

    fn entry(expect: &str, send: &str) -> OneKey {
        OneKey {
            id: "id".to_string(),
            name: "account".to_string(),
            steps: vec![OneKeyStep {
                label: "Password".to_string(),
                expect: expect.to_string(),
                send: send.to_string(),
            }],
        }
    }

    #[test]
    fn valid_entry_can_be_saved() {
        assert_eq!(validate_onekeys(&[entry("password:", "sëcret🔒")]), None);
    }

    #[test]
    fn built_in_prompt_presets_are_distinct_from_custom_regexes() {
        assert_eq!(
            expect_preset(DEFAULT_ONEKEY_PASSWORD_EXPECT),
            ExpectPreset::Password
        );
        assert_eq!(
            expect_preset(DEFAULT_ONEKEY_USERNAME_EXPECT),
            ExpectPreset::Username
        );
        assert_eq!(expect_preset(r"PIN:\s*$"), ExpectPreset::Custom);
    }

    #[test]
    fn empty_send_and_invalid_regex_are_rejected() {
        assert!(
            validate_onekeys(&[entry("password:", "")])
                .unwrap()
                .contains("Send value")
        );
        assert!(
            validate_onekeys(&[entry("(", "secret")])
                .unwrap()
                .contains("invalid Expect regex")
        );
    }

    #[test]
    fn dragging_moves_dialog_by_the_pointer_delta() {
        let bounds = DialogBounds {
            viewport_width: 1_200.0,
            viewport_height: 800.0,
            dialog_width: 800.0,
        };
        let drag = DialogDrag {
            pointer_start_x: 500.0,
            pointer_start_y: 100.0,
            dialog_start: DialogPosition {
                left: 200.0,
                top: 48.0,
            },
            moved: false,
        };

        assert_eq!(
            moved_dialog_position(drag, 650.0, 220.0, bounds),
            DialogPosition {
                left: 350.0,
                top: 168.0,
            }
        );
    }

    #[test]
    fn dragging_keeps_a_recovery_strip_and_title_bar_visible() {
        let bounds = DialogBounds {
            viewport_width: 1_200.0,
            viewport_height: 800.0,
            dialog_width: 800.0,
        };

        assert_eq!(
            clamp_dialog_position(
                DialogPosition {
                    left: -10_000.0,
                    top: -10_000.0,
                },
                bounds,
            ),
            DialogPosition {
                left: -752.0,
                top: 0.0,
            }
        );
        assert_eq!(
            clamp_dialog_position(
                DialogPosition {
                    left: 10_000.0,
                    top: 10_000.0,
                },
                bounds,
            ),
            DialogPosition {
                left: 1_152.0,
                top: 756.0,
            }
        );
    }

    #[test]
    fn dialog_geometry_parser_rejects_missing_or_zero_dimensions() {
        assert_eq!(
            parse_dialog_geometry("200,48,800,1200,800"),
            Some((
                DialogPosition {
                    left: 200.0,
                    top: 48.0,
                },
                DialogBounds {
                    viewport_width: 1_200.0,
                    viewport_height: 800.0,
                    dialog_width: 800.0,
                },
            ))
        );
        assert_eq!(parse_dialog_geometry("200,48,0,1200,800"), None);
        assert_eq!(parse_dialog_geometry("not,geometry"), None);
    }
}
