use dioxus::prelude::*;

use rusterm_core::FocusedTabAppearance;
use rusterm_core::config::{KeybindingAction, Keybindings, Language, SkinKind, SkinSettings};

use crate::keybindings::{event_chord, format_key_chord};

#[derive(Clone, Copy, PartialEq)]
enum KeybindingValidationError {
    UnsafeShortcut,
    Conflict(KeybindingAction),
}

const fn skin_kind_key(kind: SkinKind) -> &'static str {
    match kind {
        SkinKind::TokyoNight => "settings.skin_tokyo_night",
        SkinKind::OneDark => "settings.skin_one_dark",
        SkinKind::SolarizedDark => "settings.skin_solarized_dark",
        SkinKind::Custom => "settings.skin_custom",
    }
}

const fn keybinding_action_key(action: KeybindingAction) -> &'static str {
    match action {
        KeybindingAction::CloseFocusedPane => "settings.keybinding_close_focused_pane",
        KeybindingAction::AppendPane => "settings.keybinding_append_pane",
        KeybindingAction::ToggleComparison => "settings.keybinding_toggle_comparison",
        KeybindingAction::TogglePaneZoom => "settings.keybinding_toggle_pane_zoom",
    }
}

fn keybinding_error_text(error: KeybindingValidationError) -> String {
    match error {
        KeybindingValidationError::UnsafeShortcut => {
            crate::i18n::t("settings.keybinding_error_unsafe")
        }
        KeybindingValidationError::Conflict(action) => {
            let action = crate::i18n::t(keybinding_action_key(action));
            crate::i18n::tf("settings.keybinding_error_conflict", &[("action", &action)])
        }
    }
}

#[component]
fn SkinColorField(
    field: &'static str,
    label: String,
    value: String,
    on_change: EventHandler<String>,
) -> Element {
    rsx! {
        div {
            "data-rusterm-skin-color": "{field}",
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
    /// Whether local usage-habit collection is enabled (opt-in).
    #[props(default)]
    usage_habits_enabled: bool,
    /// Fires with the new enabled state when the user toggles the checkbox.
    #[props(default)]
    on_save_usage_habits: EventHandler<bool>,
    /// Fires when the user clicks "Export privacy-safe report". The handler
    /// in app.rs builds the JSON report and writes it to the downloads dir.
    #[props(default)]
    on_export_usage_habits: EventHandler<()>,
    /// Current UI language.
    #[props(default)]
    language: Language,
    /// Fires with the newly chosen language when the user picks one. Applied
    /// immediately (no Save needed) since it re-renders the whole dialog.
    #[props(default)]
    on_save_language: EventHandler<Language>,
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
    let mut usage_habits = use_signal(|| usage_habits_enabled);
    let mut keybinding_draft = use_signal(|| keybindings.normalized());
    let mut skin_draft = use_signal(|| skin.normalized());
    let skin_preview = skin_draft().palette();
    let mut capturing_keybinding: Signal<Option<KeybindingAction>> = use_signal(|| None);
    let mut keybinding_error: Signal<Option<KeybindingValidationError>> = use_signal(|| None);
    // Subscribe explicitly so every translated label updates with the global language.
    let _active_language = crate::i18n::LANGUAGE();
    // Current language code for the <select value=...> binding.
    let language_code = match language {
        Language::Zh => "zh",
        Language::En => "en",
    };

    rsx! {
        div {
            "data-rusterm-settings-overlay": "true",
            style: "--settings-bg:#1a1b26;--settings-surface:#24283b;--settings-surface-hover:#2a2b3d;--settings-border:#2a2b3d;--settings-border-strong:#2a2b3d;--settings-text:#c0caf5;--settings-text-muted:#9aa5ce;--settings-accent:#7aa2f7;--settings-danger:#f7768e;position:fixed;inset:0;background:rgba(0,0,0,0.6);display:flex;justify-content:center;align-items:center;padding:24px;box-sizing:border-box;isolation:isolate;z-index:20000;",

            div {
                "data-rusterm-settings-panel": "true",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": crate::i18n::t("settings.title"),
                style: "background:var(--settings-surface);border:1px solid var(--settings-border-strong);border-radius:10px;padding:24px;width:min(520px,100%);max-height:calc(100vh - 48px);box-sizing:border-box;overflow-y:auto;color:var(--settings-text);color-scheme:dark;accent-color:var(--settings-accent);opacity:1;box-shadow:0 20px 64px rgba(0,0,0,0.72);",

                h3 { style: "margin: 0 0 6px; font-size: 16px;", { crate::i18n::t("settings.title") } }

                // Language selector — top of the dialog since it affects how
                // every other label reads. Applied immediately on change.
                div {
                    style: "display:flex;align-items:center;justify-content:space-between;gap:16px;margin:0 0 20px;padding-bottom:16px;border-bottom:1px solid var(--settings-border);",
                    div {
                        label {
                            style: "font-size:12px;color:var(--settings-text);display:block;margin-bottom:3px;",
                            { crate::i18n::t("settings.language") }
                        }
                        span {
                            style: "font-size:11px;color:var(--settings-text-muted);",
                            { crate::i18n::t("settings.language_help") }
                        }
                    }
                    select {
                        style: "min-width:120px;background:var(--settings-bg);color:var(--settings-text);border:1px solid var(--settings-border-strong);border-radius:4px;padding:5px 8px;font-size:12px;cursor:pointer;",
                        value: "{language_code}",
                        onchange: move |e| {
                            let lang = if e.value() == "en" { Language::En } else { Language::Zh };
                            on_save_language.call(lang);
                        },
                        option { value: "zh", selected: language == Language::Zh, { Language::Zh.label() } }
                        option { value: "en", selected: language == Language::En, { Language::En.label() } }
                    }
                }

                h3 { style: "margin: 0 0 6px; font-size: 16px;", { crate::i18n::t("settings.appearance") } }
                p {
                    style: "margin: 0 0 20px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.appearance_help") }
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.outline_color") } }
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
                        label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.outline_width") } }
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
                        label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.corner_radius") } }
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
                        div { style: "margin-bottom: 10px; color: var(--settings-text-muted); font-size: 11px;", { crate::i18n::t("settings.preview") } }
                        div {
                            style: "height: 36px; display: flex; align-items: stretch; border-bottom: 1px solid var(--settings-border);",
                            div {
                                style: "display: flex; align-items: center; gap: 6px; padding: 0 12px; color: var(--settings-text); background: var(--settings-surface); border-bottom: 2px solid var(--settings-accent); box-shadow: {preview_shadow}; border-radius: {preview_radius}; font-size: 12px;",
                                span { style: "width: 6px; height: 6px; border-radius: 50%; background: var(--settings-accent);" }
                                { crate::i18n::t("settings.focused_session") }
                            }
                        }
                    }
                }

                // ── Application skin ────────────────────────────────────────
                h3 { style: "margin:24px 0 6px;font-size:16px;", { crate::i18n::t("settings.skin") } }
                p {
                    style: "margin:0 0 12px;color:var(--settings-text-muted);font-size:12px;line-height:1.5;",
                    { crate::i18n::t("settings.skin_help") }
                }
                div {
                    style: "display:flex;flex-wrap:wrap;gap:6px;margin-bottom:12px;",
                    for kind in SkinKind::ALL {
                        {
                            let selected = skin_draft().kind == kind;
                            let background = if selected { "var(--settings-accent)" } else { "var(--settings-bg)" };
                            let color = if selected { "var(--settings-bg)" } else { "var(--settings-text)" };
                            let border = if selected { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                            let key = skin_kind_key(kind);
                            let label = crate::i18n::t(key);
                            rsx! {
                                button {
                                    key: "skin-{key}",
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
                        span { style: "font-size:12px;font-weight:600;", { crate::i18n::t("settings.skin_preview") } }
                        span { style: "font-size:11px;color:{skin_preview.text_muted};", { crate::i18n::t(skin_kind_key(skin_draft().kind)) } }
                    }
                    div {
                        style: "background:{skin_preview.surface};color:{skin_preview.text};padding:9px;display:flex;align-items:center;gap:8px;",
                        span { style: "width:8px;height:8px;border-radius:50%;background:{skin_preview.success};" }
                        span { style: "font-size:11px;", { crate::i18n::t("settings.skin_preview_connected") } }
                        button { style: "margin-left:auto;background:{skin_preview.accent};color:{skin_preview.background};border:0;border-radius:3px;padding:3px 7px;font-size:10px;", { crate::i18n::t("settings.skin_preview_action") } }
                    }
                }
                if skin_draft().kind == SkinKind::Custom {
                    div {
                        style: "display:flex;flex-direction:column;gap:8px;background:var(--settings-bg);border:1px solid var(--settings-border);border-radius:6px;padding:12px;margin-bottom:12px;",
                        SkinColorField { field: "background", label: crate::i18n::t("settings.color_background"), value: skin_draft().custom.background.clone(), on_change: move |value| skin_draft.write().custom.background = value }
                        SkinColorField { field: "surface", label: crate::i18n::t("settings.color_surface"), value: skin_draft().custom.surface.clone(), on_change: move |value| skin_draft.write().custom.surface = value }
                        SkinColorField { field: "surface_hover", label: crate::i18n::t("settings.color_surface_hover"), value: skin_draft().custom.surface_hover.clone(), on_change: move |value| skin_draft.write().custom.surface_hover = value }
                        SkinColorField { field: "border", label: crate::i18n::t("settings.color_border"), value: skin_draft().custom.border.clone(), on_change: move |value| skin_draft.write().custom.border = value }
                        SkinColorField { field: "border_strong", label: crate::i18n::t("settings.color_border_strong"), value: skin_draft().custom.border_strong.clone(), on_change: move |value| skin_draft.write().custom.border_strong = value }
                        SkinColorField { field: "text", label: crate::i18n::t("settings.color_text"), value: skin_draft().custom.text.clone(), on_change: move |value| skin_draft.write().custom.text = value }
                        SkinColorField { field: "text_muted", label: crate::i18n::t("settings.color_text_muted"), value: skin_draft().custom.text_muted.clone(), on_change: move |value| skin_draft.write().custom.text_muted = value }
                        SkinColorField { field: "accent", label: crate::i18n::t("settings.color_accent"), value: skin_draft().custom.accent.clone(), on_change: move |value| skin_draft.write().custom.accent = value }
                        SkinColorField { field: "accent_secondary", label: crate::i18n::t("settings.color_accent_secondary"), value: skin_draft().custom.accent_secondary.clone(), on_change: move |value| skin_draft.write().custom.accent_secondary = value }
                        SkinColorField { field: "success", label: crate::i18n::t("settings.color_success"), value: skin_draft().custom.success.clone(), on_change: move |value| skin_draft.write().custom.success = value }
                        SkinColorField { field: "warning", label: crate::i18n::t("settings.color_warning"), value: skin_draft().custom.warning.clone(), on_change: move |value| skin_draft.write().custom.warning = value }
                        SkinColorField { field: "danger", label: crate::i18n::t("settings.color_danger"), value: skin_draft().custom.danger.clone(), on_change: move |value| skin_draft.write().custom.danger = value }
                    }
                }

                // ── Suggestion preferences ──────────────────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.suggestions") }
                }
                p {
                    style: "margin: 0 0 16px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.suggestions_help") }
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    // Enable / disable toggle
                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.enable_suggestions") } }
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
                                {if sug_enabled() { crate::i18n::t("settings.on") } else { crate::i18n::t("settings.off") }}
                            }
                        }
                    }

                    // Suggestion count selector (3 / 5 / 10)
                    div {
                        style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                        label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.suggestion_count") } }
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
                            let count = sug_count();
                            let desc = match count {
                                3 => crate::i18n::t("settings.suggestion_count_compact"),
                                5 => crate::i18n::t("settings.suggestion_count_balanced"),
                                10 => crate::i18n::t("settings.suggestion_count_extensive"),
                                _ => crate::i18n::t("settings.suggestion_count_compact"),
                            };
                            rsx! { "{count} — {desc}" }
                        }
                    }
                }

                // ── Comparison preferences ──────────────────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.comparison") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.comparison_help") }
                }
                div {
                    style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                    label { style: "font-size: 12px; color: var(--settings-text);", { crate::i18n::t("settings.comparison_diff_warning") } }
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
                            {if comparison_warning_enabled() { crate::i18n::t("settings.on") } else { crate::i18n::t("settings.off") }}
                        }
                    }
                }

                // ── Usage habits (privacy) ────────────────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.usage_habits") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.usage_habits_help") }
                }
                div {
                    style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                    label {
                        style: "font-size: 12px; color: var(--settings-text);",
                        { crate::i18n::t("settings.collect_usage_habits") }
                    }
                    div {
                        style: "display: flex; align-items: center; gap: 8px;",
                        input {
                            r#type: "checkbox",
                            checked: "{usage_habits()}",
                            style: "width: 16px; height: 16px; cursor: pointer; accent-color: var(--settings-accent);",
                            onchange: move |e| usage_habits.set(e.checked()),
                        }
                        span {
                            style: "font-size: 11px; color: var(--settings-text-muted);",
                            {if usage_habits() { crate::i18n::t("settings.on") } else { crate::i18n::t("settings.off") }}
                        }
                    }
                }
                div {
                    style: "background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 12px; margin-top: 8px; font-size: 11px; color: var(--settings-text-muted); line-height: 1.6;",
                    div { style: "color: var(--settings-text); font-weight: 600; margin-bottom: 6px;", { crate::i18n::t("settings.what_is_collected") } }
                    div { { crate::i18n::t("settings.collected_command_category") } }
                    div { { crate::i18n::t("settings.collected_activity_counts") } }
                    div { { crate::i18n::t("settings.collected_corrections") } }
                    div { { crate::i18n::t("settings.collected_host_count") } }
                    div { style: "color: var(--settings-text); font-weight: 600; margin: 10px 0 6px;", { crate::i18n::t("settings.never_collected") } }
                    div { { crate::i18n::t("settings.never_collected_credentials") } }
                    div { { crate::i18n::t("settings.never_collected_onekey") } }
                    div { { crate::i18n::t("settings.never_collected_session_data") } }
                    div { { crate::i18n::t("settings.never_collected_sensitive_arguments") } }
                    div { style: "margin-top: 10px; color: var(--settings-text-muted);", { crate::i18n::t("settings.privacy_sanitizer_help") } }
                }
                div {
                    style: "display: flex; gap: 8px; margin-top: 10px;",
                    button {
                        style: "background: var(--settings-bg); border: 1px solid var(--settings-border-strong); color: var(--settings-text); border-radius: 4px; padding: 6px 12px; cursor: pointer; font-size: 11px;",
                        disabled: "{!usage_habits()}",
                        onclick: move |_| on_export_usage_habits.call(()),
                        { crate::i18n::t("settings.export_report") }
                        " (JSON)"
                    }
                }
                div {
                    style: "font-size: 10px; color: var(--settings-text-muted); margin-top: 6px; line-height: 1.5;",
                    { crate::i18n::t("settings.export_report_help") }
                }

                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.keybindings") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.keybindings_help") }
                }
                div {
                    style: "display: flex; flex-direction: column; gap: 8px;",
                    for action in KeybindingAction::ALL {
                        {
                            let action_key = keybinding_action_key(action);
                            let action_label = crate::i18n::t(action_key);
                            let is_capturing = capturing_keybinding() == Some(action);
                            let chord_label = if is_capturing {
                                crate::i18n::t("settings.keybinding_press_shortcut")
                            } else if let Some(chord) = keybinding_draft().chord(action) {
                                format_key_chord(Some(chord))
                            } else {
                                crate::i18n::t("settings.keybinding_disabled")
                            };
                            let button_border = if is_capturing { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                            let button_bg = if is_capturing { "var(--settings-surface-hover)" } else { "var(--settings-bg)" };
                            rsx! {
                                div {
                                    key: "keybinding-{action_key}",
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
                                                        KeybindingValidationError::UnsafeShortcut,
                                                    ));
                                                    return;
                                                }
                                                if let Some(conflict) = keybinding_draft()
                                                    .conflicting_action(action, &chord)
                                                {
                                                    keybinding_error.set(Some(
                                                        KeybindingValidationError::Conflict(conflict),
                                                    ));
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
                                            { crate::i18n::t("settings.keybinding_disable") }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(error) = keybinding_error() {
                        div { style: "font-size: 11px; color: var(--settings-danger); margin-top: 2px;", { keybinding_error_text(error) } }
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
                            usage_habits.set(false);
                            keybinding_draft.set(Keybindings::default());
                            skin_draft.set(SkinSettings::default());
                            capturing_keybinding.set(None);
                            keybinding_error.set(None);
                        },
                        { crate::i18n::t("settings.reset_default") }
                    }
                    div {
                        style: "display: flex; gap: 8px;",
                        button {
                            style: "background: transparent; border: 1px solid var(--settings-border); color: var(--settings-text); border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px;",
                            onclick: move |_| on_close.call(()),
                            { crate::i18n::t("common.cancel") }
                        }
                        button {
                            style: "background: var(--settings-accent); border: none; color: var(--settings-bg); border-radius: 4px; padding: 8px 16px; cursor: pointer; font-size: 13px; font-weight: 600;",
                            onclick: move |_| {
                                on_save.call(draft().normalized());
                                on_save_suggestions.call((sug_enabled(), sug_count()));
                                on_save_comparison_diff_warning.call(comparison_warning_enabled());
                                on_save_keybindings.call(keybinding_draft().normalized());
                                on_save_skin.call(skin_draft().normalized());
                                on_save_usage_habits.call(usage_habits());
                            },
                            { crate::i18n::t("common.save") }
                        }
                    }
                }
            }
        }
    }
}
