use dioxus::prelude::*;

use rusterm_core::FocusedTabAppearance;
use rusterm_core::config::{
    KeybindingAction, Keybindings, Language, OtpWebhookConfig, SkinKind, SkinSettings, ThemeMode,
};

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

const fn theme_mode_key(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Dark => "settings.theme_dark",
        ThemeMode::Light => "settings.theme_light",
        ThemeMode::System => "settings.theme_system",
    }
}

const fn keybinding_action_key(action: KeybindingAction) -> &'static str {
    match action {
        KeybindingAction::CloseFocusedPane => "settings.keybinding_close_focused_pane",
        KeybindingAction::AppendPane => "settings.keybinding_append_pane",
        KeybindingAction::ToggleComparison => "settings.keybinding_toggle_comparison",
        KeybindingAction::TogglePaneZoom => "settings.keybinding_toggle_pane_zoom",
        KeybindingAction::ToggleChat => "settings.keybinding_toggle_chat",
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
        KeybindingAction::ToggleChat => "settings-keybinding-toggle-chat",
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
        (
            "settings-language",
            "settings.language",
            "settings.language",
            Some("settings.language_help"),
            "english chinese 中文 英文 locale i18n",
        ),
        (
            "settings-appearance",
            "settings.appearance",
            "settings.appearance",
            Some("settings.appearance_help"),
            "focused tab pane outline 聚焦 标签 窗格 轮廓",
        ),
        (
            "settings-outline-color",
            "settings.appearance",
            "settings.outline_color",
            None,
            "border colour 边框 颜色",
        ),
        (
            "settings-outline-width",
            "settings.appearance",
            "settings.outline_width",
            None,
            "border thickness px 边框 粗细",
        ),
        (
            "settings-corner-radius",
            "settings.appearance",
            "settings.corner_radius",
            None,
            "rounded corners radius 圆角",
        ),
        (
            "settings-appearance-preview",
            "settings.appearance",
            "settings.preview",
            None,
            "focused session preview 预览 聚焦会话",
        ),
        (
            "settings-theme-system",
            "settings.skin",
            "settings.theme_system",
            Some("settings.theme_mode_help"),
            "dark light system 暗色 亮色 系统 主题模式 外观",
        ),
        (
            "settings-skin",
            "settings.skin",
            "settings.skin",
            Some("settings.skin_help"),
            "theme palette chrome 主题 配色 调色板",
        ),
        (
            "settings-skin-tokyo-night",
            "settings.skin",
            "settings.skin_tokyo_night",
            None,
            "theme 主题",
        ),
        (
            "settings-skin-one-dark",
            "settings.skin",
            "settings.skin_one_dark",
            None,
            "theme 主题",
        ),
        (
            "settings-skin-solarized-dark",
            "settings.skin",
            "settings.skin_solarized_dark",
            None,
            "theme 主题",
        ),
        (
            "settings-skin-custom",
            "settings.skin",
            "settings.skin_custom",
            None,
            "theme palette 自定义 主题 配色",
        ),
        (
            "settings-color-background",
            "settings.skin",
            "settings.color_background",
            None,
            "custom palette color 自定义 调色板 颜色",
        ),
        (
            "settings-color-surface",
            "settings.skin",
            "settings.color_surface",
            None,
            "custom palette color 自定义 调色板 颜色",
        ),
        (
            "settings-color-surface_hover",
            "settings.skin",
            "settings.color_surface_hover",
            None,
            "custom palette hover color 自定义 调色板 悬停 颜色",
        ),
        (
            "settings-color-border",
            "settings.skin",
            "settings.color_border",
            None,
            "custom palette color 自定义 调色板 边框 颜色",
        ),
        (
            "settings-color-border_strong",
            "settings.skin",
            "settings.color_border_strong",
            None,
            "custom palette color 自定义 调色板 强调边框 颜色",
        ),
        (
            "settings-color-text",
            "settings.skin",
            "settings.color_text",
            None,
            "custom palette foreground 自定义 调色板 前景 文本",
        ),
        (
            "settings-color-text_muted",
            "settings.skin",
            "settings.color_text_muted",
            None,
            "custom palette secondary text 自定义 调色板 弱化文本",
        ),
        (
            "settings-color-accent",
            "settings.skin",
            "settings.color_accent",
            None,
            "custom palette highlight 自定义 调色板 强调色",
        ),
        (
            "settings-color-accent_secondary",
            "settings.skin",
            "settings.color_accent_secondary",
            None,
            "custom palette highlight 自定义 调色板 次强调色",
        ),
        (
            "settings-color-success",
            "settings.skin",
            "settings.color_success",
            None,
            "custom palette status green 自定义 调色板 成功",
        ),
        (
            "settings-color-warning",
            "settings.skin",
            "settings.color_warning",
            None,
            "custom palette status yellow 自定义 调色板 警告",
        ),
        (
            "settings-color-danger",
            "settings.skin",
            "settings.color_danger",
            None,
            "custom palette status red 自定义 调色板 危险",
        ),
        (
            "settings-suggestions",
            "settings.suggestions",
            "settings.suggestions",
            Some("settings.suggestions_help"),
            "command history inline fish autocomplete 命令历史 行内 自动补全",
        ),
        (
            "settings-enable-suggestions",
            "settings.suggestions",
            "settings.enable_suggestions",
            Some("settings.suggestions_help"),
            "toggle command history autocomplete 开关 命令历史 自动补全",
        ),
        (
            "settings-suggestion-count",
            "settings.suggestions",
            "settings.suggestion_count",
            None,
            "3 5 10 compact balanced extensive 数量 紧凑 均衡",
        ),
        (
            "settings-comparison",
            "settings.comparison",
            "settings.comparison",
            Some("settings.comparison_help"),
            "compare diff synchronized input 比对 差异 同步输入",
        ),
        (
            "settings-comparison-warning",
            "settings.comparison",
            "settings.comparison_diff_warning",
            Some("settings.comparison_help"),
            "large diff highlight warning 大量 差异 高亮 警告",
        ),
        (
            "settings-usage-habits",
            "settings.usage_habits",
            "settings.usage_habits",
            Some("settings.usage_habits_help"),
            "privacy telemetry analytics duckdb 隐私 遥测 习惯 本地",
        ),
        (
            "settings-collect-usage",
            "settings.usage_habits",
            "settings.collect_usage_habits",
            Some("settings.usage_habits_help"),
            "opt in telemetry analytics 收集 开关 选择加入",
        ),
        (
            "settings-collected-data",
            "settings.usage_habits",
            "settings.what_is_collected",
            None,
            "command category activity corrections host count 收集内容 命令类别 活动 更正 主机数",
        ),
        (
            "settings-never-collected",
            "settings.usage_habits",
            "settings.never_collected",
            None,
            "password key token credential secret privacy 密码 私钥 令牌 凭据 机密",
        ),
        (
            "settings-export-report",
            "settings.usage_habits",
            "settings.export_report",
            Some("settings.export_report_help"),
            "json download sanitized privacy 导出 下载 清理 隐私报告",
        ),
        (
            "settings-otp-webhook",
            "settings.otp_webhook",
            "settings.otp_webhook",
            Some("settings.otp_webhook_help"),
            "otp mfa 2fa jumpServer jumpserver otp feishu webhook verification one time password second factor 二次认证 验证码 飞书 堡垒机 机器人 webhook",
        ),
        (
            "settings-local-ai",
            "ai_runtime.local.enable",
            "ai_runtime.local.enable",
            Some("ai_runtime.local.enable_hint"),
            "qwen llm offline template script 本地 模型 离线 模板 脚本",
        ),
        (
            "settings-local-ai-mirror",
            "ai_runtime.local.enable",
            "ai_runtime.local.mirror_url",
            Some("ai_runtime.local.mirror_url_hint"),
            "hf huggingface endpoint download mirror 下载 镜像 地址",
        ),
        (
            "settings-local-ai-model",
            "ai_runtime.local.enable",
            "ai_runtime.local.model_select",
            None,
            "qwen llm active model 当前 模型 选择",
        ),
        (
            "settings-local-ai-download",
            "ai_runtime.local.enable",
            "ai_runtime.local.download_model",
            Some("ai_runtime.local.download_hint"),
            "download model weights quantize cache 下载 模型 权重 量化 缓存",
        ),
        (
            "settings-local-ai-custom",
            "ai_runtime.local.enable",
            "ai_runtime.local.custom_form_show",
            None,
            "add custom model repo prompt eos 添加 自定义 模型 仓库 提示词 结束符",
        ),
        (
            "settings-local-ai-custom",
            "ai_runtime.local.enable",
            "ai_runtime.local.custom_name",
            None,
            "display name custom model 显示名称 自定义模型",
        ),
        (
            "settings-local-ai-custom",
            "ai_runtime.local.enable",
            "ai_runtime.local.custom_repo",
            None,
            "repository huggingface custom model 仓库 自定义模型",
        ),
        (
            "settings-local-ai-custom",
            "ai_runtime.local.enable",
            "ai_runtime.local.custom_template",
            None,
            "prompt chat template custom model 提示词 模板 自定义模型",
        ),
        (
            "settings-local-ai-custom",
            "ai_runtime.local.enable",
            "ai_runtime.local.custom_eos",
            None,
            "end token custom model 结束符 自定义模型",
        ),
        (
            "settings-keybindings",
            "settings.keybindings",
            "settings.keybindings",
            Some("settings.keybindings_help"),
            "keyboard hotkey shortcut 键盘 热键 快捷键",
        ),
        (
            "settings-reset-default",
            "settings.title",
            "settings.reset_default",
            None,
            "restore factory defaults reset 恢复 默认 重置",
        ),
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
    let builtin_models = rusterm_core::config::builtin_models();
    for model in builtin_models.iter().chain(custom_models) {
        items.push(SettingsSearchItem {
            target: "settings-local-ai-model",
            title: model.name.clone(),
            section: local_ai_section.clone(),
            search_text: format!(
                "{} {} {} model qwen huggingface 模型",
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
    for (index, ch) in haystack.chars().enumerate() {
        if ch == needle[matched] {
            first.get_or_insert(index);
            matched += 1;
            if matched == needle.len() {
                let span = index.saturating_sub(first.unwrap_or(0));
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
            id: "settings-color-{field}",
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

#[cfg_attr(not(feature = "qwen-local"), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum ModelDownloadState {
    Idle,
    Starting,
    Downloading(String),
    Quantizing {
        current: usize,
        total: usize,
        tensor: String,
    },
    Ready,
    Failed(String),
}

#[cfg(feature = "qwen-local")]
enum ModelDownloadEvent {
    Progress(rusterm_ai::SetupProgress),
    Finished(Result<(), String>),
}

#[cfg(feature = "qwen-local")]
fn local_model_cache_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir()
        .or_else(dirs::home_dir)
        .map(|dir| dir.join("RusTerm").join("qwen-local"))
}

#[cfg(feature = "qwen-local")]
fn model_download_status_for(
    settings: &rusterm_core::config::QwenLocalSettings,
) -> ModelDownloadState {
    let Some(cache_dir) = local_model_cache_dir() else {
        return ModelDownloadState::Idle;
    };
    let model = rusterm_core::config::resolve_model(settings);
    if rusterm_ai::is_model_ready(&cache_dir, &model) {
        ModelDownloadState::Ready
    } else {
        ModelDownloadState::Idle
    }
}

#[cfg(not(feature = "qwen-local"))]
fn model_download_status_for(
    _settings: &rusterm_core::config::QwenLocalSettings,
) -> ModelDownloadState {
    ModelDownloadState::Idle
}

#[cfg(feature = "qwen-local")]
fn render_model_download(
    settings: rusterm_core::config::QwenLocalSettings,
    qwen_local: Signal<rusterm_core::config::QwenLocalSettings>,
    mut download_state: Signal<ModelDownloadState>,
    mut download_task: Signal<Option<(String, String)>>,
) -> Element {
    let state = download_state();
    let active_download = download_task();
    let busy = active_download.is_some();
    let ready = state == ModelDownloadState::Ready;
    let button_label = if ready {
        crate::i18n::t("ai_runtime.local.ready")
    } else {
        crate::i18n::t("ai_runtime.local.download_model")
    };
    let status_text = match &state {
        ModelDownloadState::Idle | ModelDownloadState::Ready => String::new(),
        ModelDownloadState::Starting => crate::i18n::t("ai_runtime.local.download_starting"),
        ModelDownloadState::Downloading(file) => {
            crate::i18n::tf("ai_runtime.local.downloading", &[("file", file)])
        }
        ModelDownloadState::Quantizing {
            current,
            total,
            tensor,
        } => crate::i18n::tf(
            "ai_runtime.local.quantizing",
            &[
                ("current", &current.to_string()),
                ("total", &total.to_string()),
                ("tensor", tensor),
            ],
        ),
        ModelDownloadState::Failed(error) => {
            crate::i18n::tf("ai_runtime.local.download_failed", &[("error", error)])
        }
    };
    let background_status = active_download
        .as_ref()
        .filter(|(model_id, _)| model_id != &settings.active_model_id)
        .map(|(_, model_name)| {
            crate::i18n::tf(
                "ai_runtime.local.download_background",
                &[("model", model_name)],
            )
        })
        .unwrap_or_default();
    let status_color = if matches!(state, ModelDownloadState::Failed(_)) {
        "var(--settings-danger)"
    } else {
        "var(--settings-text-muted)"
    };

    rsx! {
        div {
            id: "settings-local-ai-download",
            style: "margin-top:8px;",
            button {
                style: "background:var(--settings-accent);border:none;color:var(--settings-bg);border-radius:4px;padding:6px 12px;cursor:pointer;font-size:11px;font-weight:600;",
                disabled: busy || ready,
                onclick: move |_| {
                    if download_task().is_some() {
                        return;
                    }
                    let Some(cache_dir) = local_model_cache_dir() else {
                        download_state.set(ModelDownloadState::Failed(
                            crate::i18n::t("ai_runtime.local.cache_unavailable"),
                        ));
                        return;
                    };
                    let task_settings = settings.clone();
                    let task_model = rusterm_core::config::resolve_model(&task_settings);
                    let task_model_id = task_model.id.clone();
                    download_task.set(Some((task_model_id.clone(), task_model.name.clone())));
                    download_state.set(ModelDownloadState::Starting);

                    spawn(async move {
                        let (tx, rx) = std::sync::mpsc::channel::<ModelDownloadEvent>();
                        std::thread::spawn(move || {
                            let progress_tx = tx.clone();
                            let result = rusterm_ai::ensure_model(
                                &cache_dir,
                                &task_model,
                                &task_settings.mirror_url,
                                move |progress| {
                                    let _ = progress_tx.send(ModelDownloadEvent::Progress(progress));
                                },
                            )
                            .map(|_| ())
                            .map_err(|error| error.to_string());
                            let _ = tx.send(ModelDownloadEvent::Finished(result));
                        });

                        loop {
                            match rx.try_recv() {
                                Ok(ModelDownloadEvent::Progress(progress)) => {
                                    if qwen_local().active_model_id != task_model_id {
                                        continue;
                                    }
                                    match progress {
                                        rusterm_ai::SetupProgress::Downloading { file, .. } => {
                                            download_state.set(ModelDownloadState::Downloading(file));
                                        }
                                        rusterm_ai::SetupProgress::Quantizing {
                                            current,
                                            total,
                                            tensor,
                                        } => download_state.set(ModelDownloadState::Quantizing {
                                            current,
                                            total,
                                            tensor,
                                        }),
                                        rusterm_ai::SetupProgress::Done => {}
                                    }
                                }
                                Ok(ModelDownloadEvent::Finished(result)) => {
                                    download_task.set(None);
                                    if qwen_local().active_model_id == task_model_id {
                                        match result {
                                            Ok(()) => download_state.set(ModelDownloadState::Ready),
                                            Err(error) => {
                                                download_state.set(ModelDownloadState::Failed(error))
                                            }
                                        }
                                    }
                                    return;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    download_task.set(None);
                                    if qwen_local().active_model_id == task_model_id {
                                        download_state.set(ModelDownloadState::Failed(
                                            "model download thread stopped unexpectedly".to_string(),
                                        ));
                                    }
                                    return;
                                }
                            }
                        }
                    });
                },
                {button_label}
            }
            div {
                style: "font-size:10px;color:var(--settings-text-muted);margin-top:4px;line-height:1.5;",
                { crate::i18n::t("ai_runtime.local.download_hint") }
            }
            if !status_text.is_empty() {
                div {
                    style: "font-size:10px;color:{status_color};margin-top:4px;line-height:1.5;overflow-wrap:anywhere;",
                    {status_text}
                }
            }
            if !background_status.is_empty() {
                div {
                    style: "font-size:10px;color:var(--settings-text-muted);margin-top:4px;line-height:1.5;overflow-wrap:anywhere;",
                    {background_status}
                }
            }
        }
    }
}

#[cfg(not(feature = "qwen-local"))]
fn render_model_download(
    _settings: rusterm_core::config::QwenLocalSettings,
    _qwen_local: Signal<rusterm_core::config::QwenLocalSettings>,
    _download_state: Signal<ModelDownloadState>,
    _download_task: Signal<Option<(String, String)>>,
) -> Element {
    rsx! {}
}

/// Default Feishu-bot OTP webhook used when the user switches the provider
/// selector to `feishubot` and no prior config exists.
fn otp_default_feishubot() -> OtpWebhookConfig {
    OtpWebhookConfig::Feishubot {
        app_id: String::new(),
        app_secret: String::new(),
        chat_id: String::new(),
        code_pattern: rusterm_core::config::default_otp_code_pattern(),
        sender_open_id: None,
        max_age_secs: rusterm_core::config::default_otp_max_age_secs(),
        base_url: rusterm_core::config::default_feishu_base_url(),
    }
}

/// Default Feishu *user-token* provider used when the selector switches to
/// `feishuuser` and no prior config exists.
fn otp_default_feishuuser() -> OtpWebhookConfig {
    OtpWebhookConfig::FeishuUser {
        app_id: String::new(),
        app_secret: String::new(),
        bot_open_id: String::new(),
        code_pattern: rusterm_core::config::default_otp_code_pattern(),
        request_text: rusterm_core::config::default_feishu_otp_request_text(),
        base_url: rusterm_core::config::default_feishu_base_url(),
    }
}

/// Default generic HTTP OTP webhook used when the user switches the provider
/// selector to `http` and no prior config exists.
fn otp_default_http() -> OtpWebhookConfig {
    OtpWebhookConfig::Http {
        url: String::new(),
        method: "get".to_string(),
        body: None,
        headers: Vec::new(),
        code_pattern: rusterm_core::config::default_otp_code_pattern(),
        timeout_secs: 10,
    }
}

const OTP_INPUT_STYLE: &str = "width: 100%; box-sizing: border-box; padding: 6px 8px; border: 1px solid var(--settings-border); border-radius: 4px; background: var(--settings-bg); color: var(--settings-text); font-size: 12px;";

fn render_otp_webhook_settings(
    mut setting: Signal<Option<OtpWebhookConfig>>,
    mut enabled: Signal<bool>,
    on_save: EventHandler<Option<OtpWebhookConfig>>,
    on_save_enabled: EventHandler<bool>,
) -> Element {
    let current = setting();
    let kind = match current {
        Some(OtpWebhookConfig::Feishubot { .. }) => "feishubot",
        Some(OtpWebhookConfig::Http { .. }) => "http",
        Some(OtpWebhookConfig::FeishuUser { .. }) => "feishuuser",
        Some(OtpWebhookConfig::Manual) | None => "manual",
    };

    let feishubot_fields = match current {
        Some(OtpWebhookConfig::Feishubot {
            ref app_id,
            ref app_secret,
            ref chat_id,
            ref code_pattern,
            ref sender_open_id,
            max_age_secs,
            ..
        }) => Some((
            app_id.clone(),
            app_secret.clone(),
            chat_id.clone(),
            code_pattern.clone(),
            sender_open_id.clone().unwrap_or_default(),
            max_age_secs,
        )),
        _ => None,
    };

    let feishuuser_fields = match current {
        Some(OtpWebhookConfig::FeishuUser {
            ref app_id,
            ref app_secret,
            ref bot_open_id,
            ref code_pattern,
            ref request_text,
            ..
        }) => Some((
            app_id.clone(),
            app_secret.clone(),
            bot_open_id.clone(),
            code_pattern.clone(),
            request_text.clone(),
        )),
        _ => None,
    };

    let http_fields = match current {
        Some(OtpWebhookConfig::Http {
            ref url,
            ref method,
            ref body,
            ref code_pattern,
            timeout_secs,
            ..
        }) => Some((
            url.clone(),
            method.clone(),
            body.clone().unwrap_or_default(),
            code_pattern.clone(),
            timeout_secs,
        )),
        _ => None,
    };

    rsx! {
        h3 {
            id: "settings-otp-webhook",
            style: "margin: 24px 0 6px; font-size: 16px;",
            { crate::i18n::t("settings.otp_webhook") }
        }
        p {
            style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
            { crate::i18n::t("settings.otp_webhook_help") }
        }

        // Master switch — turns the whole automatic OTP pipeline on/off
        // without touching the saved provider configuration.
        div {
            style: "display: flex; align-items: center; justify-content: space-between; gap: 16px; margin-bottom: 12px;",
            label {
                style: "font-size: 12px; color: var(--settings-text);",
                { crate::i18n::t("settings.otp_webhook_enabled") }
            }
            input {
                r#type: "checkbox",
                checked: "{enabled()}",
                style: "cursor: pointer;",
                onchange: move |e| {
                    enabled.set(e.checked());
                    on_save_enabled.call(e.checked());
                },
            }
        }

        // Provider selector — `manual` maps to `None` on disk (the safe
        // default; OTP prompts fall back to the OneKey popup).
        div {
            style: "display: flex; align-items: center; justify-content: space-between; gap: 16px;",
            label {
                style: "font-size: 12px; color: var(--settings-text);",
                { crate::i18n::t("settings.otp_provider") }
            }
            select {
                style: "min-width: 180px; background: var(--settings-bg); color: var(--settings-text); border: 1px solid var(--settings-border-strong); border-radius: 4px; padding: 5px 8px; font-size: 12px; cursor: pointer;",
                value: kind,
                onchange: move |e| {
                    match e.value().as_str() {
                        "feishubot" => setting.set(Some(otp_default_feishubot())),
                        "http" => setting.set(Some(otp_default_http())),
                        "feishuuser" => setting.set(Some(otp_default_feishuuser())),
                        _ => setting.set(None),
                    }
                },
                option { value: "manual", selected: kind == "manual", { crate::i18n::t("settings.otp_provider_manual") } }
                option { value: "feishubot", selected: kind == "feishubot", { crate::i18n::t("settings.otp_provider_feishubot") } }
                option { value: "http", selected: kind == "http", { crate::i18n::t("settings.otp_provider_http") } }
                option { value: "feishuuser", selected: kind == "feishuuser", { crate::i18n::t("settings.otp_provider_feishuuser") } }
            }
        }

        if let Some((app_id, app_secret, chat_id, code_pattern, sender_open_id, max_age_secs)) = feishubot_fields {
            div {
                style: "display: flex; flex-direction: column; gap: 10px; margin-top: 12px; background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 12px;",
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_app_id") } }
                    input {
                        r#type: "text",
                        value: "{app_id}",
                        placeholder: "cli_xxxx",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Feishubot { app_id, .. }) = cur.as_mut() {
                                *app_id = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_app_secret") } }
                    input {
                        r#type: "password",
                        value: "{app_secret}",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Feishubot { app_secret, .. }) = cur.as_mut() {
                                *app_secret = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_chat_id") } }
                    input {
                        r#type: "text",
                        value: "{chat_id}",
                        placeholder: "oc_xxxx",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Feishubot { chat_id, .. }) = cur.as_mut() {
                                *chat_id = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_sender_open_id") } }
                    input {
                        r#type: "text",
                        value: "{sender_open_id}",
                        placeholder: "ou_xxxx",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Feishubot { sender_open_id, .. }) = cur.as_mut() {
                                let v = e.value();
                                *sender_open_id = (!v.trim().is_empty()).then_some(v);
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_code_pattern") } }
                    input {
                        r#type: "text",
                        value: "{code_pattern}",
                        placeholder: "\\b\\d{{4,8}}\\b",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Feishubot { code_pattern, .. }) = cur.as_mut() {
                                *code_pattern = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_max_age_secs") } }
                    input {
                        r#type: "number",
                        min: "1",
                        value: "{max_age_secs}",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            if let Ok(v) = e.value().trim().parse::<u64>() {
                                let mut cur = setting();
                                if let Some(OtpWebhookConfig::Feishubot { max_age_secs, .. }) = cur.as_mut() {
                                    *max_age_secs = v;
                                    setting.set(cur);
                                }
                            }
                        },
                    }
                }
            }
        }

        if let Some((app_id, app_secret, bot_open_id, code_pattern, request_text)) = feishuuser_fields {
            div {
                style: "display: flex; flex-direction: column; gap: 10px; margin-top: 12px; background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 12px;",
                p {
                    style: "margin: 0; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.otp_feishuuser_help") }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_app_id") } }
                    input {
                        r#type: "text",
                        value: "{app_id}",
                        placeholder: "cli_xxxx",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::FeishuUser { app_id, .. }) = cur.as_mut() {
                                *app_id = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_app_secret") } }
                    input {
                        r#type: "password",
                        value: "{app_secret}",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::FeishuUser { app_secret, .. }) = cur.as_mut() {
                                *app_secret = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_bot_open_id") } }
                    input {
                        r#type: "text",
                        value: "{bot_open_id}",
                        placeholder: "ou_xxxx",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::FeishuUser { bot_open_id, .. }) = cur.as_mut() {
                                *bot_open_id = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_code_pattern") } }
                    input {
                        r#type: "text",
                        value: "{code_pattern}",
                        placeholder: "\\b\\d{{4,8}}\\b",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::FeishuUser { code_pattern, .. }) = cur.as_mut() {
                                *code_pattern = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_feishu_request_text") } }
                    input {
                        r#type: "text",
                        value: "{request_text}",
                        placeholder: "动态口令",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::FeishuUser { request_text, .. }) = cur.as_mut() {
                                *request_text = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                p {
                    style: "margin: 2px 0 0; color: var(--settings-text-muted); font-size: 11px; line-height: 1.5;",
                    { crate::i18n::t("settings.otp_feishu_permissions_hint") }
                }
                div {
                    style: "display: flex; align-items: center; gap: 12px; margin-top: 4px;",
                    button {
                        r#type: "button",
                        style: "background: var(--settings-accent, #7aa2f7); color: #1a1b26; border: none; border-radius: 4px; padding: 6px 12px; font-size: 12px; font-weight: 600; cursor: pointer;",
                        onclick: move |_| {
                            // Persist the CURRENT provider draft first — the auth
                            // starter reads the saved config, so an unsaved
                            // FeishuUser selection would otherwise make this
                            // button (and the OTP-prompt auto-fill) silently do
                            // nothing until the dialog's Save button is hit
                            // (issue #130).
                            on_save.call(setting());
                            // Signal-only: app.rs polls and resets this.
                            *crate::FEISHU_AUTH_REQUESTED.write() = true;
                        },
                        { crate::i18n::t("settings.otp_feishu_reauth") }
                    }
                    span {
                        style: "font-size: 11px; color: var(--settings-text-muted);",
                        {
                            let status = crate::APP_STATE.read().feishu_token_status.clone();
                            match status {
                                Some(crate::state::FeishuTokenStatus::Connected { expires_at }) => {
                                    let ts = chrono::DateTime::from_timestamp(expires_at, 0)
                                        .map(|dt| dt.format("%H:%M").to_string())
                                        .unwrap_or_default();
                                    crate::i18n::tf("settings.otp_feishu_token_connected", &[("ts", &ts)])
                                }
                                Some(crate::state::FeishuTokenStatus::Failed { reason, .. }) => {
                                    crate::i18n::tf("settings.otp_feishu_token_failed", &[("reason", &reason)])
                                }
                                None => crate::i18n::t("settings.otp_feishu_token_missing"),
                            }
                        }
                    }
                }
            }
        }

        if let Some((url, method, body, code_pattern, timeout_secs)) = http_fields {
            div {
                style: "display: flex; flex-direction: column; gap: 10px; margin-top: 12px; background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 12px;",
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_http_url") } }
                    input {
                        r#type: "text",
                        value: "{url}",
                        placeholder: "https://totp.example.local/current",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Http { url, .. }) = cur.as_mut() {
                                *url = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_http_method") } }
                    select {
                        style: OTP_INPUT_STYLE,
                        value: method.clone(),
                        onchange: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Http { method, .. }) = cur.as_mut() {
                                *method = e.value();
                                setting.set(cur);
                            }
                        },
                        option { value: "get", selected: method == "get", "GET" }
                        option { value: "post", selected: method == "post", "POST" }
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_http_body") } }
                    input {
                        r#type: "text",
                        value: "{body}",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Http { body, .. }) = cur.as_mut() {
                                let v = e.value();
                                *body = (!v.trim().is_empty()).then_some(v);
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_code_pattern") } }
                    input {
                        r#type: "text",
                        value: "{code_pattern}",
                        placeholder: "\\b\\d{{4,8}}\\b",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            let mut cur = setting();
                            if let Some(OtpWebhookConfig::Http { code_pattern, .. }) = cur.as_mut() {
                                *code_pattern = e.value();
                                setting.set(cur);
                            }
                        },
                    }
                }
                div {
                    label { style: "font-size: 11px; color: var(--settings-text-muted); display: block; margin-bottom: 2px;", { crate::i18n::t("settings.otp_timeout_secs") } }
                    input {
                        r#type: "number",
                        min: "1",
                        value: "{timeout_secs}",
                        style: OTP_INPUT_STYLE,
                        oninput: move |e| {
                            if let Ok(v) = e.value().trim().parse::<u64>() {
                                let mut cur = setting();
                                if let Some(OtpWebhookConfig::Http { timeout_secs, .. }) = cur.as_mut() {
                                    *timeout_secs = v;
                                    setting.set(cur);
                                }
                            }
                        },
                    }
                }
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
    /// Current OTP / MFA webhook provider config (from settings.json).
    #[props(default)]
    otp_webhook: Option<OtpWebhookConfig>,
    /// Fires with the new OTP webhook config when the user clicks Save.
    #[props(default)]
    on_save_otp_webhook: EventHandler<Option<OtpWebhookConfig>>,
    /// Current OTP auto-fetch master switch (from settings.json).
    #[props(default = true)]
    otp_webhook_enabled: bool,
    /// Fires with the new master-switch state when the user clicks Save.
    #[props(default)]
    on_save_otp_webhook_enabled: EventHandler<bool>,
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
    let initial_model_download_state = model_download_status_for(&qwen_local_settings);
    let mut qwen_local = use_signal(|| qwen_local_settings.clone());
    // OTP webhook draft — edited locally, committed on Save.
    let mut otp_webhook_setting = use_signal(|| otp_webhook.clone());
    let otp_webhook_enabled_setting = use_signal(|| otp_webhook_enabled);
    let mut model_download_state = use_signal(|| initial_model_download_state);
    let model_download_task = use_signal(|| None::<(String, String)>);
    // Custom-model form state (collapsible "Add custom model" section).
    let mut show_custom_form = use_signal(|| false);
    let mut custom_name = use_signal(String::new);
    let mut custom_repo = use_signal(String::new);
    let mut custom_template = use_signal(String::new);
    let mut custom_eos = use_signal(String::new);
    let mut custom_error = use_signal(String::new);
    let mut keybinding_draft = use_signal(|| keybindings.normalized());
    let mut skin_draft = use_signal(|| skin.normalized());
    // Resolve the OS dark/light preference so `ThemeMode::System` previews
    // correctly inside the dialog. Read fresh on every render; this is cheap
    // (a tao window property read) and keeps the preview in sync if the user
    // switches OS theme while the dialog is open.
    let system_is_dark = matches!(
        dioxus::desktop::window().theme(),
        dioxus::desktop::tao::window::Theme::Dark
    );
    let skin_preview = skin_draft().palette(system_is_dark);
    // When editing the Custom skin, toggles whether the color fields below
    // edit the dark (`custom`) or light (`custom_light`) variant. Defaults to
    // whichever variant the current mode resolves to so the user lands on the
    // palette they're most likely to see.
    let mut custom_editing_light: Signal<bool> =
        use_signal(|| skin_draft().mode.resolve(system_is_dark) == ThemeMode::Light);
    let mut capturing_keybinding: Signal<Option<KeybindingAction>> = use_signal(|| None);
    let mut keybinding_error: Signal<Option<KeybindingValidationError>> = use_signal(|| None);
    let mut search_query = use_signal(String::new);
    // Subscribe explicitly so every translated label updates with the global language.
    let _active_language = crate::i18n::LANGUAGE();
    // Current language code for the <select value=...> binding.
    let language_code = match language {
        Language::Zh => "zh",
        Language::En => "en",
    };
    let search_matches = settings_search_matches(&search_query(), &qwen_local().custom_models);

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
                div {
                    style: "position:sticky;top:-24px;z-index:3;background:var(--settings-surface);padding:8px 0 10px;margin-bottom:8px;border-bottom:1px solid var(--settings-border);",
                    input {
                        r#type: "search",
                        value: "{search_query}",
                        placeholder: crate::i18n::t("settings.search_placeholder"),
                        "aria-label": crate::i18n::t("settings.search_placeholder"),
                        style: "width:100%;box-sizing:border-box;padding:8px 10px;background:var(--settings-bg);color:var(--settings-text);border:1px solid var(--settings-border-strong);border-radius:5px;font-size:12px;",
                        oninput: move |event| search_query.set(event.value()),
                    }
                    if !search_query().trim().is_empty() {
                        div {
                            style: "max-height:180px;overflow-y:auto;display:flex;flex-direction:column;gap:4px;margin-top:7px;",
                            if search_matches.is_empty() {
                                div {
                                    style: "font-size:11px;color:var(--settings-text-muted);padding:6px 8px;",
                                    { crate::i18n::t("settings.search_no_results") }
                                }
                            } else {
                                for item in search_matches.clone() {
                                    {
                                        let target = item.target;
                                        rsx! {
                                            button {
                                                key: "settings-search-{target}-{item.title}",
                                                style: "display:flex;align-items:center;justify-content:space-between;gap:10px;width:100%;padding:6px 8px;text-align:left;background:var(--settings-bg);color:var(--settings-text);border:1px solid var(--settings-border);border-radius:4px;cursor:pointer;font-size:11px;",
                                                onclick: move |_| {
                                                    if target.starts_with("settings-color-") {
                                                        skin_draft.write().kind = SkinKind::Custom;
                                                    }
                                                    spawn(async move {
                                                        tokio::task::yield_now().await;
                                                        let script = format!(
                                                            "const el=document.getElementById('{}');if(el){{el.scrollIntoView({{behavior:'smooth',block:'center'}});el.animate([{{outline:'2px solid var(--settings-accent)'}},{{outline:'2px solid transparent'}}],{{duration:1200}});}}",
                                                            target
                                                        );
                                                        let _ = dioxus::document::eval(&script).await;
                                                    });
                                                },
                                                span { "{item.title}" }
                                                span { style: "color:var(--settings-text-muted);font-size:10px;", "{item.section}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Language selector — top of the dialog since it affects how
                // every other label reads. Applied immediately on change.
                div {
                    id: "settings-language",
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

                h3 { id: "settings-appearance", style: "margin: 0 0 6px; font-size: 16px;", { crate::i18n::t("settings.appearance") } }
                p {
                    style: "margin: 0 0 20px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.appearance_help") }
                }

                div {
                    style: "display: flex; flex-direction: column; gap: 16px;",

                    div {
                        id: "settings-outline-color",
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
                        id: "settings-outline-width",
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
                        id: "settings-corner-radius",
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
                        id: "settings-appearance-preview",
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
                h3 { id: "settings-skin", style: "margin:24px 0 6px;font-size:16px;", { crate::i18n::t("settings.skin") } }
                p {
                    style: "margin:0 0 12px;color:var(--settings-text-muted);font-size:12px;line-height:1.5;",
                    { crate::i18n::t("settings.skin_help") }
                }
                // Appearance mode (Dark / Light / System). Orthogonal to the
                // skin family above: each built-in skin has a paired light
                // variant, and `System` follows the OS preference live.
                div {
                    style: "display:flex;align-items:center;gap:8px;margin-bottom:12px;",
                    span { style: "font-size:12px;color:var(--settings-text);", { crate::i18n::t("settings.theme_mode") } }
                    div {
                        style: "display:flex;flex-wrap:wrap;gap:6px;",
                        for mode in ThemeMode::ALL {
                            {
                                let selected = skin_draft().mode == mode;
                                let background = if selected { "var(--settings-accent)" } else { "var(--settings-bg)" };
                                let color = if selected { "var(--settings-bg)" } else { "var(--settings-text)" };
                                let border = if selected { "var(--settings-accent)" } else { "var(--settings-border-strong)" };
                                let key = theme_mode_key(mode);
                                let label = crate::i18n::t(key);
                                rsx! {
                                    button {
                                        key: "theme-{key}",
                                        id: match mode {
                                            ThemeMode::Dark => "settings-theme-dark",
                                            ThemeMode::Light => "settings-theme-light",
                                            ThemeMode::System => "settings-theme-system",
                                        },
                                        style: "background:{background};color:{color};border:1px solid {border};border-radius:4px;padding:5px 9px;cursor:pointer;font-size:11px;",
                                        onclick: move |_| skin_draft.write().mode = mode,
                                        "{label}"
                                    }
                                }
                            }
                        }
                    }
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
                                    id: match kind {
                                        SkinKind::TokyoNight => "settings-skin-tokyo-night",
                                        SkinKind::OneDark => "settings-skin-one-dark",
                                        SkinKind::SolarizedDark => "settings-skin-solarized-dark",
                                        SkinKind::Custom => "settings-skin-custom",
                                    },
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
                        // Variant switch: edit the dark or light custom palette.
                        // Defaults to the variant the current mode resolves to.
                        div {
                            style: "display:flex;align-items:center;gap:6px;margin-bottom:4px;",
                            span { style: "font-size:11px;color:var(--settings-text-muted);", { crate::i18n::t("settings.custom_variant") } }
                            button {
                                style: if !custom_editing_light() {
                                    "background:var(--settings-accent);color:var(--settings-bg);border:1px solid var(--settings-accent);border-radius:4px;padding:3px 8px;cursor:pointer;font-size:11px;"
                                } else {
                                    "background:var(--settings-bg);color:var(--settings-text);border:1px solid var(--settings-border-strong);border-radius:4px;padding:3px 8px;cursor:pointer;font-size:11px;"
                                },
                                onclick: move |_| custom_editing_light.set(false),
                                { crate::i18n::t("settings.theme_dark") }
                            }
                            button {
                                style: if custom_editing_light() {
                                    "background:var(--settings-accent);color:var(--settings-bg);border:1px solid var(--settings-accent);border-radius:4px;padding:3px 8px;cursor:pointer;font-size:11px;"
                                } else {
                                    "background:var(--settings-bg);color:var(--settings-text);border:1px solid var(--settings-border-strong);border-radius:4px;padding:3px 8px;cursor:pointer;font-size:11px;"
                                },
                                onclick: move |_| custom_editing_light.set(true),
                                { crate::i18n::t("settings.theme_light") }
                            }
                        }
                        if !custom_editing_light() {
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
                        } else {
                            SkinColorField { field: "light_background", label: crate::i18n::t("settings.color_background"), value: skin_draft().custom_light.background.clone(), on_change: move |value| skin_draft.write().custom_light.background = value }
                            SkinColorField { field: "light_surface", label: crate::i18n::t("settings.color_surface"), value: skin_draft().custom_light.surface.clone(), on_change: move |value| skin_draft.write().custom_light.surface = value }
                            SkinColorField { field: "light_surface_hover", label: crate::i18n::t("settings.color_surface_hover"), value: skin_draft().custom_light.surface_hover.clone(), on_change: move |value| skin_draft.write().custom_light.surface_hover = value }
                            SkinColorField { field: "light_border", label: crate::i18n::t("settings.color_border"), value: skin_draft().custom_light.border.clone(), on_change: move |value| skin_draft.write().custom_light.border = value }
                            SkinColorField { field: "light_border_strong", label: crate::i18n::t("settings.color_border_strong"), value: skin_draft().custom_light.border_strong.clone(), on_change: move |value| skin_draft.write().custom_light.border_strong = value }
                            SkinColorField { field: "light_text", label: crate::i18n::t("settings.color_text"), value: skin_draft().custom_light.text.clone(), on_change: move |value| skin_draft.write().custom_light.text = value }
                            SkinColorField { field: "light_text_muted", label: crate::i18n::t("settings.color_text_muted"), value: skin_draft().custom_light.text_muted.clone(), on_change: move |value| skin_draft.write().custom_light.text_muted = value }
                            SkinColorField { field: "light_accent", label: crate::i18n::t("settings.color_accent"), value: skin_draft().custom_light.accent.clone(), on_change: move |value| skin_draft.write().custom_light.accent = value }
                            SkinColorField { field: "light_accent_secondary", label: crate::i18n::t("settings.color_accent_secondary"), value: skin_draft().custom_light.accent_secondary.clone(), on_change: move |value| skin_draft.write().custom_light.accent_secondary = value }
                            SkinColorField { field: "light_success", label: crate::i18n::t("settings.color_success"), value: skin_draft().custom_light.success.clone(), on_change: move |value| skin_draft.write().custom_light.success = value }
                            SkinColorField { field: "light_warning", label: crate::i18n::t("settings.color_warning"), value: skin_draft().custom_light.warning.clone(), on_change: move |value| skin_draft.write().custom_light.warning = value }
                            SkinColorField { field: "light_danger", label: crate::i18n::t("settings.color_danger"), value: skin_draft().custom_light.danger.clone(), on_change: move |value| skin_draft.write().custom_light.danger = value }
                        }
                    }
                }

                // ── Suggestion preferences ──────────────────────────────────
                h3 {
                    id: "settings-suggestions",
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
                        id: "settings-enable-suggestions",
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
                        id: "settings-suggestion-count",
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
                    id: "settings-comparison",
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.comparison") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.comparison_help") }
                }
                div {
                    id: "settings-comparison-warning",
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
                    id: "settings-usage-habits",
                    style: "margin: 24px 0 6px; font-size: 16px;",
                    { crate::i18n::t("settings.usage_habits") }
                }
                p {
                    style: "margin: 0 0 12px; color: var(--settings-text-muted); font-size: 12px; line-height: 1.5;",
                    { crate::i18n::t("settings.usage_habits_help") }
                }
                div {
                    id: "settings-collect-usage",
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
                    id: "settings-collected-data",
                    style: "background: var(--settings-bg); border: 1px solid var(--settings-border); border-radius: 6px; padding: 12px; margin-top: 8px; font-size: 11px; color: var(--settings-text-muted); line-height: 1.6;",
                    div { style: "color: var(--settings-text); font-weight: 600; margin-bottom: 6px;", { crate::i18n::t("settings.what_is_collected") } }
                    div { { crate::i18n::t("settings.collected_command_category") } }
                    div { { crate::i18n::t("settings.collected_activity_counts") } }
                    div { { crate::i18n::t("settings.collected_corrections") } }
                    div { { crate::i18n::t("settings.collected_host_count") } }
                    div { id: "settings-never-collected", style: "color: var(--settings-text); font-weight: 600; margin: 10px 0 6px;", { crate::i18n::t("settings.never_collected") } }
                    div { { crate::i18n::t("settings.never_collected_credentials") } }
                    div { { crate::i18n::t("settings.never_collected_onekey") } }
                    div { { crate::i18n::t("settings.never_collected_session_data") } }
                    div { { crate::i18n::t("settings.never_collected_sensitive_arguments") } }
                    div { style: "margin-top: 10px; color: var(--settings-text-muted);", { crate::i18n::t("settings.privacy_sanitizer_help") } }
                }
                div {
                    style: "display: flex; gap: 8px; margin-top: 10px;",
                    button {
                        id: "settings-export-report",
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

                // ── OTP / MFA webhook (JumpServer 二次认证) ──────────────
                { render_otp_webhook_settings(otp_webhook_setting, otp_webhook_enabled_setting, on_save_otp_webhook, on_save_otp_webhook_enabled) }

                // ── Local AI template generation ───────────────────────
                h3 {
                    id: "settings-local-ai",
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
                    id: "settings-local-ai-mirror",
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
                    id: "settings-local-ai-model",
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
                            model_download_state.set(model_download_status_for(&s));
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
                    if qwen_local().enabled {
                        { render_model_download(
                            qwen_local(),
                            qwen_local,
                            model_download_state,
                            model_download_task,
                        ) }
                    }
                }

                // ── Custom model form (collapsible) ──────────────────────
                div {
                    id: "settings-local-ai-custom",
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
                                    model_download_state.set(model_download_status_for(&s));
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
                                        model_download_state.set(model_download_status_for(&s));
                                        qwen_local.set(s);
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }

                h3 {
                    id: "settings-keybindings",
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
                                    id: keybinding_target(action),
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
                        id: "settings-reset-default",
                        style: "background: transparent; border: 1px solid var(--settings-border-strong); color: var(--settings-text); border-radius: 4px; padding: 8px 12px; cursor: pointer; font-size: 12px;",
                        onclick: move |_| {
                            draft.set(FocusedTabAppearance::default());
                            sug_enabled.set(true);
                            sug_count.set(3);
                            comparison_warning_enabled.set(true);
                            usage_habits.set(false);
                            otp_webhook_setting.set(None);
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
                                on_save_otp_webhook.call(otp_webhook_setting());
                                on_save_otp_webhook_enabled.call(otp_webhook_enabled_setting());
                            },
                            { crate::i18n::t("common.save") }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_score_prefers_exact_and_substring_matches() {
        assert!(
            fuzzy_score("skin", "skin").unwrap() > fuzzy_score("skin", "application skin").unwrap()
        );
        assert!(fuzzy_score("MIRROR", "download mirror url").is_some());
    }

    #[test]
    fn fuzzy_score_supports_subsequence_and_chinese_queries() {
        assert!(fuzzy_score("otln clr", "outline color border colour").is_some());
        assert!(fuzzy_score("下载镜像", "HuggingFace 模型下载镜像地址").is_some());
    }

    #[test]
    fn fuzzy_score_rejects_unrelated_queries() {
        assert_eq!(fuzzy_score("zzqx", "outline color"), None);
        assert_eq!(fuzzy_score("", "outline color"), None);
    }

    #[test]
    fn settings_search_indexes_both_languages_and_every_keybinding_action() {
        let mirror = settings_search_matches("download mirror", &[]);
        assert!(
            mirror
                .iter()
                .any(|item| item.target == "settings-local-ai-mirror")
        );

        let chinese = settings_search_matches("命令建议", &[]);
        assert!(
            chinese
                .iter()
                .any(|item| item.target == "settings-suggestions")
        );

        let builtin_model = settings_search_matches("Qwen2.5-Coder-0.5B", &[]);
        assert!(
            builtin_model
                .iter()
                .any(|item| item.target == "settings-local-ai-model")
        );

        for action in KeybindingAction::ALL {
            let key = keybinding_action_key(action);
            let query = crate::i18n::t_for(key, Language::En);
            let matches = settings_search_matches(&query, &[]);
            assert!(
                matches
                    .iter()
                    .any(|item| item.target == keybinding_target(action))
            );
        }
    }

    #[test]
    fn settings_search_includes_explicit_model_download_action() {
        let english = settings_search_matches("download model", &[]);
        assert!(
            english
                .iter()
                .any(|item| item.target == "settings-local-ai-download")
        );

        let chinese = settings_search_matches("下载模型", &[]);
        assert!(
            chinese
                .iter()
                .any(|item| item.target == "settings-local-ai-download")
        );
    }

    #[test]
    fn settings_search_includes_custom_model_names_and_repositories() {
        let model = rusterm_core::config::ModelConfig {
            id: "ops-coder".to_string(),
            name: "Ops Coder".to_string(),
            repo_id: "Example/Ops-Coder".to_string(),
            architecture: "qwen2".to_string(),
            prompt_template: "{prompt}".to_string(),
            eos_token: "</s>".to_string(),
        };

        let matches = settings_search_matches("Example/Ops-Coder", &[model]);
        assert!(matches.iter().any(|item| item.title == "Ops Coder"));
    }

    #[test]
    fn settings_search_includes_otp_webhook_in_both_languages() {
        let english = settings_search_matches("otp feishu", &[]);
        assert!(
            english
                .iter()
                .any(|item| item.target == "settings-otp-webhook")
        );
        let chinese = settings_search_matches("二次认证 飞书", &[]);
        assert!(
            chinese
                .iter()
                .any(|item| item.target == "settings-otp-webhook")
        );
    }
}
