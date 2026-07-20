use dioxus::prelude::*;

use rusterm_core::FocusedTabAppearance;

#[component]
pub fn SettingsDialog(
    appearance: FocusedTabAppearance,
    on_close: EventHandler<()>,
    on_save: EventHandler<FocusedTabAppearance>,
) -> Element {
    let mut draft = use_signal(|| appearance.normalized());
    let preview = draft().normalized();
    let preview_shadow = format!(
        "inset 0 0 0 {}px {}",
        preview.border_width, preview.border_color
    );
    let preview_radius = format!("{}px", preview.border_radius);

    rsx! {
        div {
            style: "position: fixed; inset: 0; background: rgba(0,0,0,0.6); display: flex; justify-content: center; align-items: center; z-index: 1000;",

            div {
                style: "background: #24283b; border-radius: 8px; padding: 24px; width: 420px; color: #c0caf5; box-shadow: 0 12px 36px rgba(0,0,0,0.45);",

                h3 { style: "margin: 0 0 6px; font-size: 16px;", "Appearance" }
                p {
                    style: "margin: 0 0 20px; color: #7f849c; font-size: 12px; line-height: 1.5;",
                    "Customize the complete outline around the top tab for the focused pane."
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: #a9b1d6;", "Outline color" }
                        div {
                            style: "display: flex; align-items: center; gap: 8px;",
                            input {
                                r#type: "color",
                                value: "{draft().border_color}",
                                style: "width: 38px; height: 28px; padding: 2px; border: 1px solid #414868; border-radius: 4px; background: #1a1b26; cursor: pointer;",
                                oninput: move |e| draft.write().border_color = e.value(),
                            }
                            code {
                                style: "min-width: 64px; color: #c0caf5; font-size: 12px;",
                                "{draft().border_color}"
                            }
                        }
                    }

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: #a9b1d6;", "Outline width" }
                        div {
                            style: "display: flex; align-items: center; gap: 10px;",
                            input {
                                r#type: "range",
                                min: "1",
                                max: "4",
                                step: "1",
                                value: "{draft().border_width}",
                                oninput: move |e| {
                                    if let Ok(value) = e.value().parse::<u8>() {
                                        draft.write().border_width = value;
                                    }
                                },
                            }
                            span { style: "width: 28px; font-size: 12px;", "{draft().border_width}px" }
                        }
                    }

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: #a9b1d6;", "Corner radius" }
                        div {
                            style: "display: flex; align-items: center; gap: 10px;",
                            input {
                                r#type: "range",
                                min: "0",
                                max: "12",
                                step: "1",
                                value: "{draft().border_radius}",
                                oninput: move |e| {
                                    if let Ok(value) = e.value().parse::<u8>() {
                                        draft.write().border_radius = value;
                                    }
                                },
                            }
                            span { style: "width: 28px; font-size: 12px;", "{draft().border_radius}px" }
                        }
                    }

                    div {
                        style: "background: #1a1b26; border: 1px solid #2a2b3d; border-radius: 6px; padding: 14px;",
                        div { style: "margin-bottom: 10px; color: #7f849c; font-size: 11px;", "Preview" }
                        div {
                            style: "height: 36px; display: flex; align-items: stretch; border-bottom: 1px solid #2a2b3d;",
                            div {
                                style: "display: flex; align-items: center; gap: 6px; padding: 0 12px; color: #c0caf5; background: #24283b; border-bottom: 2px solid #7aa2f7; box-shadow: {preview_shadow}; border-radius: {preview_radius}; font-size: 12px;",
                                span { style: "width: 6px; height: 6px; border-radius: 50%; background: #7aa2f7;" }
                                "Focused session"
                            }
                        }
                    }
                }

                div {
                    style: "display: flex; justify-content: space-between; gap: 8px; margin-top: 20px;",
                    button {
                        style: "background: transparent; border: 1px solid #414868; color: #a9b1d6; border-radius: 4px; padding: 8px 12px; cursor: pointer; font-size: 12px;",
                        onclick: move |_| draft.set(FocusedTabAppearance::default()),
                        "Reset default"
                    }
                    div {
                        style: "display: flex; gap: 8px;",
                        button {
                            style: "background: transparent; border: 1px solid #2a2b3d; color: #c0caf5; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                            onclick: move |_| on_close.call(()),
                            "Cancel"
                        }
                        button {
                            style: "background: #7aa2f7; border: none; color: #1a1b26; border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                            onclick: move |_| on_save.call(draft().normalized()),
                            "Save"
                        }
                    }
                }
            }
        }
    }
}
