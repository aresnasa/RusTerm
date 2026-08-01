use dioxus::prelude::*;

use rusterm_core::FocusedTabAppearance;
use rusterm_core::config::{KeybindingAction, Keybindings, SkinKind, SkinSettings};

use crate::keybindings::{event_chord, format_key_chord};

#[component]
fn SkinColorField(label: &'static str, value: String, on_change: EventHandler<String>) -> Element {
    rsx! {
        div {
            "data-rusterm-skin-color": "{label}",
            style: "display:flex;align-items:center;justify-content:space-between;gap:12px;",
            label { style: "font-size:12px;color:var(--settings-text);", "{label}" }
            div {
                style: "display:flex;align-items:center;gap:8px;",
                input {
                    r#type: "color",
                    value: "{value}",
                    style: "width:34px;height:26px;padding:1px;border:1px solid var(--settings-border-strong);border-radius:4px;background:var(--settings-bg);cursor:pointer;",
                    oninput: move |event| on_change.call(event.value()),
                }
                code { style: "min-width:64px;color:var(--settings-text);font-size:11px;", "{value}" }
            }
        }
    }
}

/// Settings dialog for appearance, suggestions, comparison warnings, keyboard
/// shortcuts, and application skin. Each `on_save_*` callback lets the caller
/// persist its setting group through the matching `ConfigManager` method.
#[component]
pub fn SettingsDialog(
    appearance: FocusedTabAppearance,
    /// Current suggestion-enabled state (loaded from settings.json).
    #[props(default)]
    suggestion_enabled: bool,
    /// Current suggestion count (3, 5, or 10).
    #[props(default)]
    suggestion_count: u8,
    on_close: EventHandler<()>,
    on_save: EventHandler<FocusedTabAppearance>,
    /// Fires with `(enabled, count)` when the user clicks Save.
    #[props(default)]
    on_save_suggestions: EventHandler<(bool, u8)>,
    /// Whether comparison mode warns before highlighting large diffs.
    comparison_diff_warning_enabled: bool,
    /// Fires with the comparison warning preference when the user clicks Save.
    on_save_comparison_diff_warning: EventHandler<bool>,
    #[props(default)] keybindings: Keybindings,
    #[props(default)] on_save_keybindings: EventHandler<Keybindings>,
    #[props(default)] skin: SkinSettings,
    #[props(default)] on_save_skin: EventHandler<SkinSettings>,
) -> Element {
    let mut draft = use_signal(|| appearance.normalized());
    let preview = draft().normalized();
    let preview_shadow = format!(
        "inset 0 0 0 {}px {}",
        preview.border_width, preview.border_color
    );
    let preview_radius = format!("{}px", preview.border_radius);

    // Suggestion draft state — edited locally, committed on Save.
    let mut sug_enabled = use_signal(|| suggestion_enabled);
    let mut sug_count = use_signal(|| suggestion_count);
    let mut comparison_warning_enabled = use_signal(|| comparison_diff_warning_enabled);
    let mut keybinding_draft = use_signal(|| keybindings.normalized());
    let mut skin_draft = use_signal(|| skin.normalized());
    let skin_preview = skin_draft().palette();
    let mut capturing_keybinding: Signal<Option<KeybindingAction>> = use_signal(|| None);
    let mut keybinding_error: Signal<Option<String>> = use_signal(|| None);

    rsx! {
        div {
            "data-rusterm-settings-overlay": "true",
            style: "--settings-bg:#141722;--settings-surface:#202438;--settings-surface-hover:#2a3046;--settings-border:#3b435d;--settings-border-strong:#66718f;--settings-text:#f4f6ff;--settings-text-muted:#b7bfd8;--settings-accent:#8ab4ff;--settings-danger:#ff8fa3;position:fixed;inset:0;background:rgba(5,6,10,0.86);display:flex;justify-content:center;align-items:center;padding:24px;box-sizing:border-box;isolation:isolate;z-index:20000;",

            div {
                "data-rusterm-settings-panel": "true",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Settings",
                style: "background:var(--settings-surface);border:1px solid var(--settings-border-strong);border-radius:10px;padding:24px;width:min(520px,100%);max-height:calc(100vh - 48px);box-sizing:border-box;overflow-y:auto;color:var(--settings-text);color-scheme:dark;accent-color:var(--settings-accent);opacity:1;box-shadow:0 20px 64px rgba(0,0,0,0.72);",

                h3 { style: "margin: 0 0 6px; font-size: 16px;", "Appearance" }
                p {
                    style: "margin: 0 0 20px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    "Customize the complete outline around the top tab for the focused pane."
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", "Outline color" }
                        div {
                            style: "display: flex; align-items: center; gap: 8px;",
                            input {
                                r#type: "color",
                                value: "{draft().border_color}",
                                style: "width: 38px; height: 28px; padding: 2px; border: 1px solid var(--settings-border-strong); border-radius: 4px; background: var(--settings-bg); cursor: pointer;",
                                oninput: move |e| draft.write().border_color = e.value(),
                            }
                            code {
                                style: "min-width: 64px; color: var(--settings-text); font-size: 12px;",
                                "{draft().border_color}"
                            }
                        }
                    }

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", "Outline width" }
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
                        label { style: "font-size: 12px; color: var(--settings-text);", "Corner radius" }
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
                        style: "background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 14px;",
                        div { style: "margin-bottom: 10px; color: var(--settings-text-muted); font-size: 11px;", "Preview" }
                        div {
                            style: "height: 36px; display: flex; align-items: stretch; border-bottom: 1px solid var(--settings-border);",
                            div {
                                style: "display: flex; align-items: center; gap: 6px; padding: 0 12px; color: var(--settings-text); background: var(--settings-surface); border-bottom: 2px solid var(--settings-accent); box-shadow: {preview_shadow}; border-radius: {preview_radius}; font-size: 12px;",
                                span { style: "width: 6px; height: 6px; border-radius: 50%; background: var(--settings-accent);" }
                                "Focused session"
                            }
                        }
                    }
                }

                // ── Application skin ────────────────────────────────────────
                h3 { style: "margin:24px 0 6px;font-size:16px;", "Application skin" }
                p {
                    style: "margin:0 0 12px;color:var(--settings-text-muted);font-size:12px;line-height:1.5;",
                    "Choose a built-in skin or tune the Custom palette. This changes application chrome only; terminal ANSI and xterm colors remain independent."
                }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:6px;margin-bottom:12px;",
                    for kind in SkinKind::ALL {
                        {
                            let selected = skin_draft().kind == kind;
                            let background = if selected { "var(--settings-accent)" } else { "var(--settings-bg)" };
                            let color = if selected { "var(--settings-bg)" } else { "var(--settings-text)" };
                            let border = if selected { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                            let label = kind.label();
                            rsx! {
                                button {
                                    key: "skin-{label}",
                                    style: "background:{background};color:{color};border:1px solid {border};border-radius:4px;padding:5px 9px;cursor:pointer;font-size:11px;",
                                    onclick: move |_| skin_draft.write().kind = kind,
                                    "{label}"
                                }
                            }
                        }
                    }
                }
                div {
                    style: "border:1px solid var(--settings-border);border-radius:6px;overflow:hidden;margin-bottom:12px;",
                    div {
                        style: "background:{skin_preview.background};color:{skin_preview.text};padding:10px;display:flex;align-items:center;justify-content:space-between;",
                        span { style: "font-size:12px;font-weight:600;", "Skin preview" }
                        span { style: "font-size:11px;color:{skin_preview.text_muted};", "{skin_draft().kind.label()}" }
                    }
                    div {
                        style: "background:{skin_preview.surface};color:{skin_preview.text};padding:9px;display:flex;align-items:center;gap:8px;",
                        span { style: "width:8px;height:8px;border-radius:50%;background:{skin_preview.success};" }
                        span { style: "font-size:11px;", "Connected" }
                        button { style: "margin-left:auto;background:{skin_preview.accent};color:{skin_preview.background};border:0;border-radius:3px;padding:3px 7px;font-size:10px;", "Action" }
                    }
                }
                if skin_draft().kind == SkinKind::Custom {
                    div {
                        style: "display:flex;flex-direction:column;gap:8px;background:var(--settings-bg);border:1px solid var(--settings-border);border-radius:6px;padding:12px;margin-bottom:12px;",
                        SkinColorField { label: "Background", value: skin_draft().custom.background.clone(), on_change: move |value| skin_draft.write().custom.background = value }
                        SkinColorField { label: "Surface", value: skin_draft().custom.surface.clone(), on_change: move |value| skin_draft.write().custom.surface = value }
                        SkinColorField { label: "Surface hover", value: skin_draft().custom.surface_hover.clone(), on_change: move |value| skin_draft.write().custom.surface_hover = value }
                        SkinColorField { label: "Border", value: skin_draft().custom.border.clone(), on_change: move |value| skin_draft.write().custom.border = value }
                        SkinColorField { label: "Strong border", value: skin_draft().custom.border_strong.clone(), on_change: move |value| skin_draft.write().custom.border_strong = value }
                        SkinColorField { label: "Text", value: skin_draft().custom.text.clone(), on_change: move |value| skin_draft.write().custom.text = value }
                        SkinColorField { label: "Muted text", value: skin_draft().custom.text_muted.clone(), on_change: move |value| skin_draft.write().custom.text_muted = value }
                        SkinColorField { label: "Accent", value: skin_draft().custom.accent.clone(), on_change: move |value| skin_draft.write().custom.accent = value }
                        SkinColorField { label: "Secondary accent", value: skin_draft().custom.accent_secondary.clone(), on_change: move |value| skin_draft.write().custom.accent_secondary = value }
                        SkinColorField { label: "Success", value: skin_draft().custom.success.clone(), on_change: move |value| skin_draft.write().custom.success = value }
                        SkinColorField { label: "Warning", value: skin_draft().custom.warning.clone(), on_change: move |value| skin_draft.write().custom.warning = value }
                        SkinColorField { label: "Danger", value: skin_draft().custom.danger.clone(), on_change: move |value| skin_draft.write().custom.danger = value }
                    }
                }

                // ── Suggestion preferences ──────────────────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    "Command suggestions"
                }
                p {
                    style: "margin: 0 0 16px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    "Inline fish-style suggestions based on your command history."
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    // Enable / disable toggle
                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", "Enable suggestions" }
                        div {
                            style: "display: flex; align-items: center; gap: 8px;",
                            input {
                                r#type: "checkbox",
                                checked: "{sug_enabled()}",
                                style: "width: 16px; height: 16px; cursor: pointer; accent-color: var(--settings-accent);",
                                onchange: move |e| sug_enabled.set(e.checked()),
                            }
                            span {
                                style: "font-size: 11px; color: var(--settings-text-muted);",
                                {sug_enabled().then_some("ON").unwrap_or("OFF")}
                            }
                        }
                    }

                    // Suggestion count selector (3 / 5 / 10)
                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", "Max suggestions shown" }
                        div {
                            style: "display: flex; gap: 6px;",
                            for &count in &[3u8, 5, 10] {
                                {
                                    let is_active = sug_count() == count;
                                    let bg = if is_active { "var(--settings-accent)" } else { "var(--settings-bg)" };
                                    let color = if is_active { "var(--settings-bg)" } else { "var(--settings-text)" };
                                    let border = if is_active { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                                    let weight = if is_active { "600" } else { "400" };
                                    rsx! {
                                        button {
                                            key: "sug-{count}",
                                            style: "background: {bg}; color: {color}; border: 1px solid {border}; border-radius: 4px; padding: 4px 14px; cursor: pointer; font-size: 12px; font-weight: {weight};",
                                            onclick: move |_| sug_count.set(count),
                                            "{count}"
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Description of the current selection
                    div {
                        style: "font-size: 11px; color: var(--settings-text-muted); line-height: 1.5;",
                        {
                            let c = sug_count();
                            let desc = match c {
                                3 => "3 — compact popup, minimal screen coverage",
                                5 => "5 — balanced view of recent commands",
                                10 => "10 — extensive history at a glance",
                                _ => "compact popup, minimal screen coverage",
                            };
                            rsx! { "{desc}" }
                        }
                    }
                }

                // ── Comparison preferences ──────────────────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    "Comparison mode"
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    "Control the warning shown before highlighting a comparison where more than half of the visible rows differ."
                }
                div {
                    style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                    label { style: "font-size: 12px; color: var(--settings-text);", "Large diff warning" }
                    div {
                        style: "display: flex; align-items: center; gap: 8px;",
                        input {
                            r#type: "checkbox",
                            checked: "{comparison_warning_enabled()}",
                            style: "width: 16px; height: 16px; cursor: pointer; accent-color: var(--settings-accent);",
                            onchange: move |e| comparison_warning_enabled.set(e.checked()),
                        }
                        span {
                            style: "font-size: 11px; color: var(--settings-text-muted);",
                            {comparison_warning_enabled().then_some("ON").unwrap_or("OFF")}
                        }
                    }
                }

                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    "Keyboard shortcuts"
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    "Click a shortcut, then press a new combination. Application shortcuts require Cmd/Ctrl + Shift so standard terminal controls remain available."
                }
                div {
                    style: "display: flex; flex-direction: column; gap: 8px;",
                    for action in KeybindingAction::ALL {
                        {
                            let action_label = action.label();
                            let is_capturing = capturing_keybinding() == Some(action);
                            let chord_label = if is_capturing {
                                "Press shortcut…".to_string()
                            } else {
                                format_key_chord(keybinding_draft().chord(action))
                            };
                            let button_border = if is_capturing { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                            let button_bg = if is_capturing { "var(--settings-surface-hover)" } else { "var(--settings-bg)" };
                            rsx! {
                                div {
                                    key: "keybinding-{action_label}",
                                    style: "display: flex; align-items: center; justify-content: space-between; gap: 12px;",
                                    span { style: "font-size: 12px; color: var(--settings-text);", "{action_label}" }
                                    div { style: "display: flex; align-items: center; gap: 6px;",
                                        button {
                                            style: "min-width: 146px; background: {button_bg}; border: 1px solid {button_border}; color: var(--settings-text); border-radius: 4px; padding: 6px 8px; cursor: pointer; font-family: 'JetBrains Mono', monospace; font-size: 12px;",
                                            onclick: move |_| {
                                                capturing_keybinding.set(Some(action));
                                                keybinding_error.set(None);
                                            },
                                            onkeydown: move |e: KeyboardEvent| {
                                                e.prevent_default();
                                                e.stop_propagation();
                                                if matches!(e.key(), Key::Escape) {
                                                    capturing_keybinding.set(None);
                                                    keybinding_error.set(None);
                                                    return;
                                                }
                                                let modifiers = e.modifiers();
                                                let Some(chord) = event_chord(
                                                    &e.key(),
                                                    modifiers.ctrl(),
                                                    modifiers.alt(),
                                                    modifiers.meta(),
                                                    modifiers.shift(),
                                                ) else {
                                                    return;
                                                };
                                                if !chord.is_safe_application_shortcut() {
                                                    keybinding_error.set(Some(
                                                        "Use Cmd/Ctrl + Shift plus a key to keep terminal controls safe."
                                                            .to_string(),
                                                    ));
                                                    return;
                                                }
                                                if let Some(conflict) = keybinding_draft()
                                                    .conflicting_action(action, &chord)
                                                {
                                                    keybinding_error.set(Some(format!(
                                                        "Already used by {}.",
                                                        conflict.label()
                                                    )));
                                                    return;
                                                }
                                                keybinding_draft.write().set_chord(action, Some(chord));
                                                capturing_keybinding.set(None);
                                                keybinding_error.set(None);
                                            },
                                            "{chord_label}"
                                        }
                                        button {
                                            style: "background: transparent; border: 1px solid var(--settings-border-strong); color: var(--settings-text-muted); border-radius: 4px; padding: 5px 7px; cursor: pointer; font-size: 11px;",
                                            onclick: move |_| {
                                                keybinding_draft.write().set_chord(action, None);
                                                if capturing_keybinding() == Some(action) {
                                                    capturing_keybinding.set(None);
                                                }
                                                keybinding_error.set(None);
                                            },
                                            "Disable"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(error) = keybinding_error() {
                        div { style: "font-size: 11px; color: var(--settings-danger); margin-top: 2px;", "{error}" }
                    }
                }

                div {
                    style: "display: flex; justify-content: space-between; gap: 8px; margin-top: 20px;",
                    button {
                        style: "background: transparent; border: 1px solid var(--settings-border-strong); color: var(--settings-text); border-radius: 4px; padding: 8px 12px; cursor: pointer; font-size: 12px;",
                        onclick: move |_| {
                            draft.set(FocusedTabAppearance::default());
                            sug_enabled.set(true);
                            sug_count.set(3);
                            comparison_warning_enabled.set(true);
                            keybinding_draft.set(Keybindings::default());
                            skin_draft.set(SkinSettings::default());
                            capturing_keybinding.set(None);
                            keybinding_error.set(None);
                        },
                        "Reset default"
                    }
                    div {
                        style: "display: flex; gap: 8px;",
                        button {
                            style: "background: transparent; border: 1px solid var(--settings-border); color: var(--settings-text); border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                            onclick: move |_| on_close.call(()),
                            "Cancel"
                        }
                        button {
                            style: "background: var(--settings-accent); border: none; color: var(--settings-bg); border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                            onclick: move |_| {
                                on_save.call(draft().normalized());
                                on_save_suggestions.call((sug_enabled(), sug_count()));
                                on_save_comparison_diff_warning.call(comparison_warning_enabled());
                                on_save_keybindings.call(keybinding_draft().normalized());
                                on_save_skin.call(skin_draft().normalized());
                            },
                            "Save"
                        }
                    }
                }
            }
        }
    }
}
