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

#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingsSearchItem {
    target: &'static str,
    title: String,
    section: String,
    search_text: String,
}

const fn keybinding_target(action: KeybindingAction) -> &'static str {
    match action {
        KeybindingAction::CloseFocusedPane => "settings-keybinding-close-focused-pane",
        KeybindingAction::AppendPane => "settings-keybinding-append-pane",
        KeybindingAction::ToggleComparison => "settings-keybinding-toggle-comparison",
        KeybindingAction::TogglePaneZoom => "settings-keybinding-toggle-pane-zoom",
    }
}

fn translated_search_item(
    target: &'static str,
    section_key: &'static str,
    title_key: &'static str,
    description_key: Option<&'static str>,
    keywords: &'static str,
) -> SettingsSearchItem {
    let title = crate::i18n::t(title_key);
    let section = crate::i18n::t(section_key);
    let mut search_text = format!(
        "{} {} {} {} {keywords}",
        crate::i18n::t_for(title_key, Language::En),
        crate::i18n::t_for(title_key, Language::Zh),
        crate::i18n::t_for(section_key, Language::En),
        crate::i18n::t_for(section_key, Language::Zh),
    );
    if let Some(key) = description_key {
        search_text.push(' ');
        search_text.push_str(&crate::i18n::t_for(key, Language::En));
        search_text.push(' ');
        search_text.push_str(&crate::i18n::t_for(key, Language::Zh));
    }
    SettingsSearchItem {
        target,
        title,
        section,
        search_text,
    }
}

fn settings_search_items(
    custom_models: &[rusterm_core::config::ModelConfig],
) -> Vec<SettingsSearchItem> {
    let definitions = [
        ("settings-language", "settings.language", "settings.language", Some("settings.language_help"), "english chinese 中文 英文 locale i18n"),
        ("settings-appearance", "settings.appearance", "settings.appearance", Some("settings.appearance_help"), "focused tab pane outline 聚焦 标签 窗格 轮廓"),
        ("settings-outline-color", "settings.appearance", "settings.outline_color", None, "border colour 边框 颜色"),
        ("settings-outline-width", "settings.appearance", "settings.outline_width", None, "border thickness px 边框 粗细"),
        ("settings-corner-radius", "settings.appearance", "settings.corner_radius", None, "rounded corners radius 圆角"),
        ("settings-appearance-preview", "settings.appearance", "settings.preview", None, "focused session preview 预览 聚焦会话"),
        ("settings-skin", "settings.skin", "settings.skin", Some("settings.skin_help"), "theme palette chrome 主题 配色 调色板"),
        ("settings-skin-tokyo-night", "settings.skin", "settings.skin_tokyo_night", None, "theme 主题"),
        ("settings-skin-one-dark", "settings.skin", "settings.skin_one_dark", None, "theme 主题"),
        ("settings-skin-solarized-dark", "settings.skin", "settings.skin_solarized_dark", None, "theme 主题"),
        ("settings-skin-custom", "settings.skin", "settings.skin_custom", None, "theme palette 自定义 主题 配色"),
        ("settings-color-background", "settings.skin", "settings.color_background", None, "custom palette color 自定义 调色板 颜色"),
        ("settings-color-surface", "settings.skin", "settings.color_surface", None, "custom palette color 自定义 调色板 颜色"),
        ("settings-color-surface_hover", "settings.skin", "settings.color_surface_hover", None, "custom palette hover color 自定义 调色板 悬停 颜色"),
        ("settings-color-border", "settings.skin", "settings.color_border", None, "custom palette color 自定义 调色板 边框 颜色"),
        ("settings-color-border_strong", "settings.skin", "settings.color_border_strong", None, "custom palette color 自定义 调色板 强调边框 颜色"),
        ("settings-color-text", "settings.skin", "settings.color_text", None, "custom palette foreground 自定义 调色板 前景 文本"),
        ("settings-color-text_muted", "settings.skin", "settings.color_text_muted", None, "custom palette secondary text 自定义 调色板 弱化文本"),
        ("settings-color-accent", "settings.skin", "settings.color_accent", None, "custom palette highlight 自定义 调色板 强调色"),
        ("settings-color-accent_secondary", "settings.skin", "settings.color_accent_secondary", None, "custom palette highlight 自定义 调色板 次强调色"),
        ("settings-color-success", "settings.skin", "settings.color_success", None, "custom palette status green 自定义 调色板 成功"),
        ("settings-color-warning", "settings.skin", "settings.color_warning", None, "custom palette status yellow 自定义 调色板 警告"),
        ("settings-color-danger", "settings.skin", "settings.color_danger", None, "custom palette status red 自定义 调色板 危险"),
        ("settings-suggestions", "settings.suggestions", "settings.suggestions", Some("settings.suggestions_help"), "command history inline fish autocomplete 命令历史 行内 自动补全"),
        ("settings-enable-suggestions", "settings.suggestions", "settings.enable_suggestions", Some("settings.suggestions_help"), "toggle command history autocomplete 开关 命令历史 自动补全"),
        ("settings-suggestion-count", "settings.suggestions", "settings.suggestion_count", None, "3 5 10 compact balanced extensive 数量 紧凑 均衡"),
        ("settings-comparison", "settings.comparison", "settings.comparison", Some("settings.comparison_help"), "compare diff synchronized input 比对 差异 同步输入"),
        ("settings-comparison-warning", "settings.comparison", "settings.comparison_diff_warning", Some("settings.comparison_help"), "large diff highlight warning 大量 差异 高亮 警告"),
        ("settings-usage-habits", "settings.usage_habits", "settings.usage_habits", Some("settings.usage_habits_help"), "privacy telemetry analytics duckdb 隐私 遥测 习惯 本地"),
        ("settings-collect-usage", "settings.usage_habits", "settings.collect_usage_habits", Some("settings.usage_habits_help"), "opt in telemetry analytics 收集 开关 选择加入"),
        ("settings-collected-data", "settings.usage_habits", "settings.what_is_collected", None, "command category activity corrections host count 收集内容 命令类别 活动 更正 主机数"),
        ("settings-never-collected", "settings.usage_habits", "settings.never_collected", None, "password key token credential secret privacy 密码 私钥 令牌 凭据 机密"),
        ("settings-export-report", "settings.usage_habits", "settings.export_report", Some("settings.export_report_help"), "json download sanitized privacy 导出 下载 清理 隐私报告"),
        ("settings-local-ai", "ai_runtime.local.enable", "ai_runtime.local.enable", Some("ai_runtime.local.enable_hint"), "qwen llm offline template script 本地 模型 离线 模板 脚本"),
        ("settings-local-ai-mirror", "ai_runtime.local.enable", "ai_runtime.local.mirror_url", Some("ai_runtime.local.mirror_url_hint"), "hf huggingface endpoint download mirror 下载 镜像 地址"),
        ("settings-local-ai-model", "ai_runtime.local.enable", "ai_runtime.local.model_select", None, "qwen llm active model 当前 模型 选择"),
        ("settings-local-ai-custom", "ai_runtime.local.enable", "ai_runtime.local.custom_form_show", None, "add custom model repo prompt eos 添加 自定义 模型 仓库 提示词 结束符"),
        ("settings-local-ai-custom", "ai_runtime.local.enable", "ai_runtime.local.custom_name", None, "display name custom model 显示名称 自定义模型"),
        ("settings-local-ai-custom", "ai_runtime.local.enable", "ai_runtime.local.custom_repo", None, "repository huggingface custom model 仓库 自定义模型"),
        ("settings-local-ai-custom", "ai_runtime.local.enable", "ai_runtime.local.custom_template", None, "prompt chat template custom model 提示词 模板 自定义模型"),
        ("settings-local-ai-custom", "ai_runtime.local.enable", "ai_runtime.local.custom_eos", None, "end token custom model 结束符 自定义模型"),
        ("settings-keybindings", "settings.keybindings", "settings.keybindings", Some("settings.keybindings_help"), "keyboard hotkey shortcut 键盘 热键 快捷键"),
        ("settings-reset-default", "settings.title", "settings.reset_default", None, "restore factory defaults reset 恢复 默认 重置"),
    ];
    let mut items = definitions
        .into_iter()
        .map(|(target, section, title, description, keywords)| {
            translated_search_item(target, section, title, description, keywords)
        })
        .collect::<Vec<_>>();

    for action in KeybindingAction::ALL {
        items.push(translated_search_item(
            keybinding_target(action),
            "settings.keybindings",
            keybinding_action_key(action),
            Some("settings.keybindings_help"),
            "keyboard hotkey shortcut disable capture 键盘 热键 快捷键 禁用 录入",
        ));
    }

    let local_ai_section = crate::i18n::t("ai_runtime.local.enable");
    for model in custom_models {
        items.push(SettingsSearchItem {
            target: "settings-local-ai-model",
            title: model.name.clone(),
            section: local_ai_section.clone(),
            search_text: format!(
                "{} {} {} custom model qwen huggingface 自定义 模型",
                model.name, model.repo_id, model.id
            ),
        });
    }
    items
}

fn subsequence_score(needle: &str, haystack: &str) -> Option<usize> {
    let needle = needle.chars().collect::<Vec<_>>();
    if needle.is_empty() {
        return Some(0);
    }
    let mut matched = 0usize;
    let mut first = None;
    let mut last = 0usize;
    for (index, ch) in haystack.chars().enumerate() {
        if ch == needle[matched] {
            first.get_or_insert(index);
            last = index;
            matched += 1;
            if matched == needle.len() {
                let span = last.saturating_sub(first.unwrap_or(0));
                return Some(300usize.saturating_sub(span));
            }
        }
    }
    None
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let candidate = candidate.to_lowercase();
    if candidate == query {
        return Some(1_000);
    }
    if let Some(index) = candidate.find(&query) {
        return Some(800usize.saturating_sub(index));
    }

    let mut total = 0usize;
    for token in query.split_whitespace() {
        if let Some(index) = candidate.find(token) {
            total += 500usize.saturating_sub(index.min(400));
        } else {
            total += subsequence_score(token, &candidate)?;
        }
    }
    Some(total)
}

fn settings_search_matches(
    query: &str,
    custom_models: &[rusterm_core::config::ModelConfig],
) -> Vec<SettingsSearchItem> {
    let mut matches = settings_search_items(custom_models)
        .into_iter()
        .filter_map(|item| fuzzy_score(query, &item.search_text).map(|score| (score, item)))
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.title.cmp(&right.title))
    });
    matches.into_iter().map(|(_, item)| item).collect()
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
    /// Whether local AI template generation (Qwen2.5-Coder-1.5B) is enabled.
    /// Full local-AI settings: mirror URL, active model, custom models.
    #[props(default)]
    qwen_local_settings: rusterm_core::config::QwenLocalSettings,
    /// Hardware warning text for the local AI toggle (empty if OK).
    #[props(default)]
    qwen_local_warning: String,
    /// Fires with the new settings when the user clicks Save.
    #[props(default)]
    on_save_qwen_local: EventHandler<rusterm_core::config::QwenLocalSettings>,
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
    let mut qwen_local = use_signal(|| qwen_local_settings.clone());
    // Custom-model form state (collapsible "Add custom model" section).
    let mut show_custom_form = use_signal(|| false);
    let mut custom_name = use_signal(String::new);
    let mut custom_repo = use_signal(String::new);
    let mut custom_template = use_signal(String::new);
    let mut custom_eos = use_signal(String::new);
    let mut custom_error = use_signal(String::new);
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

                // ── Local AI template generation ───────────────────────
                h3 {
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("ai_runtime.local.enable") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("ai_runtime.local.enable_hint") }
                }
                div {
                    style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
                    label {
                        style: "font-size: 12px; color: var(--settings-text);",
                        { crate::i18n::t("ai_runtime.local.enable") }
                    }
                    div {
                        style: "display: flex; align-items: center; gap: 8px;",
                        input {
                            r#type: "checkbox",
                            checked: "{qwen_local().enabled}",
                            style: "width: 16px; height: 16px; cursor: pointer; accent-color: var(--settings-accent);",
                            onchange: move |e| {
                                let mut s = qwen_local();
                                s.enabled = e.checked();
                                qwen_local.set(s);
                            },
                        }
                        span {
                            style: "font-size: 11px; color: var(--settings-text-muted);",
                            {if qwen_local().enabled { crate::i18n::t("settings.on") } else { crate::i18n::t("settings.off") }}
                        }
                    }
                }
                if !qwen_local_warning.is_empty() {
                    div {
                        style: "font-size: 11px; color: #e0af68; margin-top: 8px; line-height: 1.5;",
                        { crate::i18n::tf("ai_runtime.local.hw_warning", &[("warning", &qwen_local_warning)]) }
                    }
                }

                // ── Mirror URL ───────────────────────────────────────────
                div {
                    style: "margin-top: 12px;",
                    label {
                        style: "font-size: 12px; color: var(--settings-text); display: block; margin-bottom: 4px;",
                        { crate::i18n::t("ai_runtime.local.mirror_url") }
                    }
                    input {
                        r#type: "text",
                        value: "{qwen_local().mirror_url}",
                        style: "width: 100%; box-sizing: border-box; padding: 6px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 12px;",
                        oninput: move |e| {
                            let mut s = qwen_local();
                            s.mirror_url = e.value();
                            qwen_local.set(s);
                        },
                    }
                    div {
                        style: "font-size: 10px; color: var(--settings-text-muted); margin-top: 4px; line-height: 1.5;",
                        { crate::i18n::t("ai_runtime.local.mirror_url_hint") }
                    }
                }

                // ── Model selector ───────────────────────────────────────
                div {
                    style: "margin-top: 12px;",
                    label {
                        style: "font-size: 12px; color: var(--settings-text); display: block; margin-bottom: 4px;",
                        { crate::i18n::t("ai_runtime.local.model_select") }
                    }
                    select {
                        style: "width: 100%; box-sizing: border-box; padding: 6px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 12px;",
                        onchange: move |e| {
                            let mut s = qwen_local();
                            s.active_model_id = e.value();
                            qwen_local.set(s);
                        },
                        {
                            let current_id = qwen_local().active_model_id.clone();
                            let mut options: Vec<rusterm_core::config::ModelConfig> =
                                rusterm_core::config::builtin_models();
                            options.extend(qwen_local().custom_models.iter().cloned());
                            rsx! {
                                for m in options {
                                    option {
                                        value: "{m.id}",
                                        selected: "{current_id == m.id}",
                                        {m.name}
                                    }
                                }
                            }
                        }
                    }
                }

                // ── Custom model form (collapsible) ──────────────────────
                div {
                    style: "margin-top: 8px;",
                    button {
                        style: "background: transparent; border: 1px dashed var(--settings-border); color: var(--settings-text-muted); border-radius: 4px; padding: 6px 12px; cursor: pointer; font-size: 11px; width: 100%; box-sizing: border-box;",
                        onclick: move |_| show_custom_form.set(!show_custom_form()),
                        { if show_custom_form() {
                            crate::i18n::t("ai_runtime.local.custom_form_hide")
                        } else {
                            crate::i18n::t("ai_runtime.local.custom_form_show")
                        }}
                    }
                }
                if show_custom_form() {
                    div {
                        style: "margin-top: 8px; padding: 12px; border: 1px solid var(--settings-border); border-radius: 4px; display: flex; flex-direction: column; gap: 8px;",
                        div {
                            style: "font-size: 11px; font-weight: 600; color: var(--settings-text);",
                            { crate::i18n::t("ai_runtime.local.custom_form_title") }
                        }
                        div {
                            label {
                                style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;",
                                { crate::i18n::t("ai_runtime.local.custom_name") }
                            }
                            input {
                                r#type: "text",
                                value: "{custom_name()}",
                                placeholder: "My Custom Model",
                                style: "width: 100%; box-sizing: border-box; padding: 4px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 11px;",
                                oninput: move |e| custom_name.set(e.value()),
                            }
                        }
                        div {
                            label {
                                style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;",
                                { crate::i18n::t("ai_runtime.local.custom_repo") }
                            }
                            input {
                                r#type: "text",
                                value: "{custom_repo()}",
                                placeholder: "Qwen/Qwen2.5-Coder-1.5B-Instruct",
                                style: "width: 100%; box-sizing: border-box; padding: 4px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 11px;",
                                oninput: move |e| custom_repo.set(e.value()),
                            }
                        }
                        div {
                            label {
                                style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;",
                                { crate::i18n::t("ai_runtime.local.custom_template") }
                                span { style: "font-family: monospace; color: var(--settings-text-muted);", " {{prompt}} " }
                            }
                            input {
                                r#type: "text",
                                value: "{custom_template()}",
                                placeholder: "<|im_start|>user\n{{prompt}}<|im_end|>\n<|im_start|>assistant\n",
                                style: "width: 100%; box-sizing: border-box; padding: 4px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 11px;",
                                oninput: move |e| custom_template.set(e.value()),
                            }
                        }
                        div {
                            label {
                                style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;",
                                { crate::i18n::t("ai_runtime.local.custom_eos") }
                            }
                            input {
                                r#type: "text",
                                value: "{custom_eos()}",
                                placeholder: "<|im_end|>",
                                style: "width: 100%; box-sizing: border-box; padding: 4px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 11px;",
                                oninput: move |e| custom_eos.set(e.value()),
                            }
                        }
                        if !custom_error().is_empty() {
                            div {
                                style: "font-size: 10px; color: var(--settings-danger);",
                                { custom_error() }
                            }
                        }
                        div {
                            style: "display: flex; gap: 8px;",
                            button {
                                style: "background: var(--settings-accent); border: none; color: var(--settings-bg); border-radius: 4px; padding: 4px 12px; cursor: pointer; font-size: 11px; font-weight: 600;",
                                onclick: move |_| {
                                    let name = custom_name().trim().to_string();
                                    let repo = custom_repo().trim().to_string();
                                    let tmpl = custom_template().trim().to_string();
                                    let eos = custom_eos().trim().to_string();
                                    if name.is_empty() || repo.is_empty() || tmpl.is_empty() || eos.is_empty() {
                                        custom_error.set(crate::i18n::t("ai_runtime.local.custom_err_empty").to_string());
                                        return;
                                    }
                                    if !tmpl.contains("{prompt}") {
                                        custom_error.set(crate::i18n::t("ai_runtime.local.custom_err_template").to_string());
                                        return;
                                    }
                                    let mut s = qwen_local();
                                    // Generate id from name: lowercase + replace non-alphanum with hyphens.
                                    let id = name.to_lowercase().chars().map(|c| {
                                        if c.is_alphanumeric() { c } else { '-' }
                                    }).collect::<String>();
                                    if s.custom_models.iter().any(|m| m.id == id)
                                        || rusterm_core::config::builtin_models().iter().any(|m| m.id == id)
                                    {
                                        custom_error.set(crate::i18n::t("ai_runtime.local.custom_err_dup").to_string());
                                        return;
                                    }
                                    s.custom_models.push(rusterm_core::config::ModelConfig {
                                        id: id.clone(),
                                        name: name.clone(),
                                        repo_id: repo,
                                        architecture: "qwen2".to_string(),
                                        prompt_template: tmpl,
                                        eos_token: eos,
                                    });
                                    s.active_model_id = id;
                                    qwen_local.set(s);
                                    custom_name.set(String::new());
                                    custom_repo.set(String::new());
                                    custom_template.set(String::new());
                                    custom_eos.set(String::new());
                                    custom_error.set(String::new());
                                    show_custom_form.set(false);
                                },
                                { crate::i18n::t("ai_runtime.local.custom_add") }
                            }
                            button {
                                style: "background: transparent; border: 1px solid var(--settings-border); color: var(--settings-text-muted); border-radius: 4px; padding: 4px 12px; cursor: pointer; font-size: 11px;",
                                onclick: move |_| {
                                    custom_name.set(String::new());
                                    custom_repo.set(String::new());
                                    custom_template.set(String::new());
                                    custom_eos.set(String::new());
                                    custom_error.set(String::new());
                                    show_custom_form.set(false);
                                },
                                { crate::i18n::t("common.cancel") }
                            }
                        }
                    }
                }

                // ── Custom models list (with delete buttons) ──────────────
                if !qwen_local().custom_models.is_empty() {
                    div {
                        style: "margin-top: 8px; display: flex; flex-direction: column; gap: 4px;",
                        for m in qwen_local().custom_models.clone() {
                            div {
                                style: "display: flex; align-items: center; justify-content: space-between; gap: 8px; padding: 4px 8px; border: 1px solid var(--settings-border); border-radius: 4px;",
                                span {
                                    style: "font-size: 11px; color: var(--settings-text);",
                                    {m.name.clone()}
                                    span {
                                        style: "font-size: 10px; color: var(--settings-text-muted); margin-left: 6px;",
                                        {m.repo_id.clone()}
                                    }
                                }
                                button {
                                    style: "background: transparent; border: none; color: var(--settings-danger); cursor: pointer; font-size: 11px; padding: 2px 6px;",
                                    onclick: move |_| {
                                        let mut s = qwen_local();
                                        s.custom_models.retain(|x| x.id != m.id);
                                        // If we just deleted the active model, fall back to default.
                                        if s.active_model_id == m.id {
                                            s.active_model_id = "qwen25-coder-1.5b".to_string();
                                        }
                                        qwen_local.set(s);
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
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
                            qwen_local.set(rusterm_core::config::QwenLocalSettings::default());
                            show_custom_form.set(false);
                            custom_name.set(String::new());
                            custom_repo.set(String::new());
                            custom_template.set(String::new());
                            custom_eos.set(String::new());
                            custom_error.set(String::new());
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
                                on_save_qwen_local.call(qwen_local());
                            },
                            { crate::i18n::t("common.save") }
                        }
                    }
                }
            }
        }
    }
}
