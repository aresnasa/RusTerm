//! # Internationalization (i18n)
//!
//! Lightweight, dependency-free Chinese/English translation layer for the UI.
//!
//! ## Design
//!
//! - [`Language`] is an enum (`En`, `Zh`), `Serialize`/`Deserialize`-able so it
//!   can live in `PersistedConfig`. The default is `Zh` (the app shipped in
//!   Chinese first).
//! - The active language is held in a Dioxus [`GlobalSignal`] named [`LANGUAGE`]
//!   so any call site — inside `rsx!`, in a helper, or deep in a component —
//!   can resolve a string with a bare [`t`] call without threading the
//!   language through every prop. The signal is initialized from
//!   `PersistedConfig::language` on app startup; the settings dialog mutates
//!   it via [`set_language`].
//! - [`t`] looks up a translation key in a flat `match`. Keys are dotted,
//!   namespaced strings (`"common.cancel"`, `"send.placeholder"`, …) so they
//!   stay stable and self-documenting even if the surrounding copy changes.
//!   Missing keys fall back to the key itself — this makes a partial rollout
//!   safe (an un-translated string shows its key rather than panicking) and
//!   surfaces gaps during development.
//!
//! ## Adding a string
//!
//! 1. Pick a key under the right namespace.
//! 2. Add a `match` arm in [`translate`] with both `En` and `Zh` arms.
//! 3. Replace the literal at the call site with `t("namespace.key")`.
//!
//! Interpolation: for strings with `{name}` placeholders, use [`tf`] (the
//! "formatted" variant) which substitutes via [`format!`]. The translation
//! strings use Rust `format!`-style `{name}` placeholders.

use std::sync::atomic::{AtomicU8, Ordering};

use dioxus::prelude::*;

// `Language` lives in `rusterm-core::config` so `PersistedConfig` can hold it
// without `rusterm-core` depending on the UI crate. We re-export it here for
// call-site convenience (`use crate::i18n::Language`).
pub use rusterm_core::config::Language;

// Language discriminants for the atomic mirror. Kept in sync with `Language`
// (0 = Zh, 1 = En). If a third language is ever added, extend both the enum
// and these constants together.
const LANG_ZH: u8 = 0;
const LANG_EN: u8 = 1;

fn lang_to_byte(lang: Language) -> u8 {
    match lang {
        Language::Zh => LANG_ZH,
        Language::En => LANG_EN,
    }
}

fn byte_to_lang(b: u8) -> Language {
    match b {
        LANG_EN => Language::En,
        _ => Language::Zh,
    }
}

/// Runtime-independent source of truth for the active language. `t()` reads
/// this so translations work outside a Dioxus runtime too (unit tests, helper
/// functions called from non-component code). The reactive [`LANGUAGE`]
/// signal below mirrors this value purely to trigger UI re-renders.
static LANG_ATOMIC: AtomicU8 = AtomicU8::new(LANG_ZH);

/// The active language, as a Dioxus global signal so components re-render
/// when it changes. `set_language` keeps this in sync with [`LANG_ATOMIC`].
/// Components that want to subscribe should read `LANGUAGE()`; everything
/// else (including [`t`]) reads the atomic — avoiding the "must be inside a
/// Dioxus runtime" requirement that `GlobalSignal` imposes.
pub static LANGUAGE: GlobalSignal<Language> = GlobalSignal::new(Language::default);

/// Read the current language without requiring a Dioxus runtime.
pub fn current_language() -> Language {
    byte_to_lang(LANG_ATOMIC.load(Ordering::Relaxed))
}

/// Initialize the global language from the persisted config. Called once on
/// app startup (after loading `settings.json`). Safe to call repeatedly.
pub fn init_language(lang: Language) {
    LANG_ATOMIC.store(lang_to_byte(lang), Ordering::Relaxed);
    if LANGUAGE() != lang {
        *LANGUAGE.write() = lang;
    }
}

/// Switch the active language. Updates both the atomic (so [`t`] picks it up
/// immediately, even outside a runtime) and the reactive signal (so the UI
/// re-renders). The caller persists the choice via the `ConfigManager`.
pub fn set_language(lang: Language) {
    LANG_ATOMIC.store(lang_to_byte(lang), Ordering::Relaxed);
    *LANGUAGE.write() = lang;
}

/// Resolve a translation key to the current language's string. Falls back to
/// the key itself if unknown — see module docs for why. Reads the language
/// from a plain atomic so this works from any thread / test / helper without
/// a Dioxus runtime.
pub fn t(key: &str) -> String {
    translate(key, current_language())
        .map(str::to_owned)
        .unwrap_or_else(|| key.to_owned())
}

/// Resolve a translation key and interpolate named placeholders. The
/// translation string uses Rust `format!`-style `{name}` placeholders; pass
/// them as a slice of `("name", value)` pairs.
///
/// Example: `tf("session.disconnected", &[("reason", &reason)])`
pub fn tf(key: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let template = translate(key, current_language())
        .map(str::to_owned)
        .unwrap_or_else(|| key.to_owned());
    interpolate(&template, args)
}

/// Core catalog. Returns `None` for unknown keys so the caller can fall back
/// to the key text (which makes a partial migration visibly safe).
///
/// Organized by namespace with section comments. Each key has exactly two
/// arms (`En` / `Zh`); keep them adjacent so reviewers can verify parity.
fn translate<'a>(key: &str, lang: Language) -> Option<&'a str> {
    let (en, zh): (&'a str, &'a str) = match key {
        // ── common.* — shared buttons & labels ──────────────────────────
        "common.cancel" => ("Cancel", "取消"),
        "common.confirm" => ("Confirm", "确认"),
        "common.ok" => ("OK", "确定"),
        "common.close" => ("Close", "关闭"),
        "common.save" => ("Save", "保存"),
        "common.delete" => ("Delete", "删除"),
        "common.retry" => ("Retry", "重试"),
        "common.continue" => ("Continue", "继续"),
        "common.skip" => ("Skip", "跳过"),
        "common.dont_ask_again" => ("Don't ask again", "不再询问"),
        "common.dont_show_again" => ("Don't show again", "不再提示"),

        // ── settings.* — settings dialog ────────────────────────────────
        "settings.language" => ("Language", "语言"),
        "settings.language_help" => (
            "Interface language (requires no restart)",
            "界面语言（无需重启）",
        ),
        "settings.title" => ("Settings", "设置"),
        "settings.appearance" => ("Appearance", "外观"),
        "settings.focused_tab_outline" => ("Focused tab outline", "聚焦标签页轮廓"),
        "settings.suggestions" => ("Suggestions", "命令建议"),
        "settings.enable_suggestions" => ("Enable inline suggestions", "启用行内建议"),
        "settings.suggestion_count" => ("Suggestion count", "建议数量"),
        "settings.comparison" => ("Comparison", "比对模式"),
        "settings.comparison_diff_warning" => {
            ("Warn before highlighting large diffs", "高亮大量差异前警告")
        }
        "settings.keybindings" => ("Keyboard shortcuts", "键盘快捷键"),
        "settings.skin" => ("Skin", "皮肤"),
        "settings.usage_habits" => ("Usage habits & privacy", "使用习惯与隐私"),
        "settings.collect_usage_habits" => ("Collect usage habits", "收集使用习惯"),
        "settings.what_is_collected" => ("What is collected", "收集内容"),
        "settings.never_collected" => ("Never collected", "绝不收集"),
        "settings.export_report" => ("Export privacy-safe report", "导出隐私安全报告"),

        // ── close_confirm.* — close confirmation dialog ─────────────────
        "close_confirm.title" => ("Close confirmation", "关闭确认"),
        "close_confirm.closing_last_window" => {
            ("About to close the last window", "即将关闭最后一个窗口")
        }
        "close_confirm.body" => (
            "Are you sure you want to quit the app?",
            "是否确实要关闭本软件？",
        ),
        "close_confirm.confirm_close" => ("Confirm close", "确认关闭"),
        "close_confirm.dont_ask_again" => ("Don't ask on next close", "下次关闭时不再询问"),

        // ── restore.* — restore session dialog ──────────────────────────
        "restore.title" => ("Restore last session", "恢复上次会话"),
        "restore.detected" => (
            "Found {session_count} session(s) from last time (saved {saved_at})",
            "检测到 {session_count} 个上次会话（保存于 {saved_at}）",
        ),
        "restore.will_cd" => (
            "After restoring, each session will cd into its last working directory",
            "✓ 恢复后会自动 cd 到上次的工作目录",
        ),
        "restore.no_history_run" => (
            "No history commands or scripts will be executed",
            "✗ 不会执行任何历史命令或脚本",
        ),
        "restore.skip_hint" => (
            "Choose \"Skip\" to start with blank sessions; \"Don't ask again\" disables this permanently.",
            "选择“跳过”可使用空白会话开始；选择“不再询问”将永久禁用此功能。",
        ),
        "restore.restore" => ("Restore", "恢复"),
        "restore.skip_blank" => ("Skip (start blank)", "跳过（开始空白会话）"),

        // ── cmd_status.* — command status badge ─────────────────────────
        "cmd_status.success" => ("✓ Success", "✓ 成功"),
        "cmd_status.success_tip" => (
            "Last command succeeded (exit 0)",
            "上一条命令执行成功（exit 0）",
        ),
        "cmd_status.failed" => ("✗ Failed (exit {exit_code})", "✗ 失败 (exit {exit_code})"),
        "cmd_status.failed_tip" => (
            "Last command failed (exit {exit_code})",
            "上一条命令执行失败（exit {exit_code}）",
        ),
        "cmd_status.disconnected" => ("⚠ Disconnected", "⚠ 断开"),
        "cmd_status.disconnected_tip" => (
            "Session disconnected: {reason}. Press Enter or right-click to reconnect",
            "会话已断开：{reason}。按 Enter 或右键重新连接",
        ),

        // ── danger.* — dangerous command dialog ──────────────────────────
        "danger.title" => ("⚠ Dangerous command confirmation", "⚠ 高危命令确认"),
        "danger.body" => (
            "This command may cause irreversible damage. Please confirm to continue.",
            "此命令可能造成不可逆破坏，请确认后继续",
        ),
        "danger.continue_anyway" => ("Continue anyway", "仍然继续"),

        // ── suggestion.* — suggestion popup ─────────────────────────────
        "suggestion.correction_prefix" => ("Fix ·", "纠正 ·"),
        "suggestion.correction_hint" => (
            "Fixes: Tab replaces only (no run) · History: × removes",
            "纠正项：Tab 仅替换，不执行 · 历史项：× 删除",
        ),
        "suggestion.history_hint" => ("Shift+Del or click × to remove", "Shift+Del 或点击 × 删除"),
        "suggestion.history_completion_title" => ("Complete from history", "补全历史"),
        "suggestion.history_completion_hint" => (
            "Ctrl+N/P selects · Enter inserts · Esc closes",
            "Ctrl+N/P 选择 · Enter 插入 · Esc 关闭",
        ),

        // ── send.* — Send panel ──────────────────────────────────────────
        "send.placeholder" => (
            "Command to send (Ctrl/Cmd+Enter to run, Tab to complete)...",
            "要发送的命令（Ctrl/Cmd+Enter 运行，Tab 补全）...",
        ),
        "send.run" => ("Send ↵", "发送 ↵"),
        "send.tab_title" => ("Send", "发送"),
        "send.targets" => ("Send targets", "发送目标"),
        "send.target" => ("Target: {label} ▾", "目标：{label} ▾"),
        "send.choose_targets" => ("Choose connected sessions", "选择已连接的会话"),
        "send.all" => ("All", "全选"),
        "send.invert" => ("Invert", "反选"),
        "send.no_sessions" => ("No connected sessions", "没有已连接的会话"),
        "send.selected_count" => ("{selected}/{total} selected", "已选 {selected}/{total}"),
        "send.n_sessions" => ("{n} sessions", "{n} 个会话"),
        "send.connected_session" => ("Connected session", "已连接的会话"),

        // ── shell.* — Shell panel ────────────────────────────────────────
        "shell.tab_title" => ("Shell", "本地终端"),
        "shell.start" => ("Start local shell", "启动本地终端"),
        "shell.start_hint" => (
            "Start a local shell embedded in this bottom panel.",
            "在此底部面板中启动一个嵌入式本地终端。",
        ),
        "shell.terminate" => ("Terminate", "终止"),

        // ── transfers.* — Transfers panel ────────────────────────────────
        "transfers.tab_title" => ("Transfers", "传输"),

        // ── api.* — REST API relay panel (bottom dock) ────────────────────
        "api.tab_title" => ("API", "API"),
        "api.title" => ("REST API relay", "REST API 中转"),
        "api.status_running" => ("Running at {url}", "运行中：{url}"),
        "api.status_stopped" => ("Stopped", "已停止"),
        "api.enable_on_startup" => ("Enable relay on startup", "启动时开启中转"),
        "api.start" => ("Start", "启动"),
        "api.stop" => ("Stop", "停止"),
        "api.bind_addr" => ("Bind addr", "监听地址"),
        "api.port" => ("Port", "端口"),
        "api.base_url" => ("Base URL", "基础 URL"),
        "api.no_account" => (
            "No accounts configured. Add one below to authenticate curl requests.",
            "尚未配置账号。在下方添加一个以通过 curl 鉴权。",
        ),
        "api.accounts" => ("Accounts (BasicAuth)", "账号（BasicAuth）"),
        "api.username" => ("Username", "用户名"),
        "api.password" => ("Password", "密码"),
        "api.add_account" => ("Add account", "添加账号"),
        "api.remove" => ("Remove", "删除"),
        "api.readonly" => ("Read-only", "只读"),
        "api.curl_examples" => (
            "curl examples — run commands on your sessions",
            "curl 示例 — 在你的会话上执行命令",
        ),
        "api.curl_hint" => (
            "Pick a connected session and a command, then copy the curl. Replace USER:PASS with an account above.",
            "选择一个已连接的会话和命令，然后复制 curl。将 USER:PASS 替换为上方账号。",
        ),
        "api.command" => ("Command", "命令"),
        "api.session" => ("Session", "会话"),
        "api.copy" => ("Copy", "复制"),
        "api.copied" => ("Copied!", "已复制！"),
        "api.no_sessions" => (
            "No connected SSH sessions. Connect to a host first.",
            "没有已连接的 SSH 会话。请先连接到主机。",
        ),
        "api.invalid_port" => ("Invalid port: {value}", "端口无效：{value}"),
        "api.saved" => ("Saved to relay.json", "已保存到 relay.json"),
        "api.account_exists" => ("Account \"{name}\" already exists", "账号“{name}”已存在"),
        "api.fill_user_pass" => ("Username and password are required", "用户名和密码不能为空"),

        // ── ai.* — AI panel ──────────────────────────────────────────────
        "ai.title" => ("AI suggestions · shadow sandbox", "AI 建议 · 影子沙盒"),
        "ai.review" => ("Review execution", "审查执行"),
        "ai.shared_results" => (
            "Local results shared with the model: {shared_result_count}",
            "已授权给模型的本机结果：{shared_result_count}",
        ),
        "ai.empty" => (
            "No model suggestions yet.\nSet OPENAI_API_KEY or ANTHROPIC_API_KEY and reopen the AI panel.",
            "暂无模型建议。\n设置 OPENAI_API_KEY 或 ANTHROPIC_API_KEY 后重新打开 AI 面板。",
        ),
        "ai.disclaimer" => (
            "The model can only suggest. Each choice still needs confirmation in a separate dialog; the model cannot write to the terminal directly.",
            "模型只能提出建议。选择后仍需在独立弹窗中确认，模型无权直接写入终端。",
        ),

        // ── session.* — session/disconnect messages ─────────────────────
        "session.disconnected" => ("Session disconnected: {reason}", "会话已断开：{reason}"),
        "session.command_not_sent" => (
            "Target session unavailable, command not sent",
            "目标会话不可用，命令未发送",
        ),

        // ── connection.* — connection dialog hints ───────────────────────
        "connection.ssh_hosts_hint" => (
            "Found {count} host config(s) in {path}",
            "提示：从 {path} 读取到 {count} 个主机配置",
        ),
        "connection.identity_files_hint" => (
            "Found {count} private key file(s) in ~/.ssh/",
            "提示：从 ~/.ssh/ 找到 {count} 个私钥文件",
        ),

        // ── terminal_search.* — terminal find/selection tools ────────────
        "terminal_search.find" => ("Find", "查找"),
        "terminal_search.placeholder" => ("Search visible terminal text…", "搜索当前终端文本…"),
        "terminal_search.no_matches" => ("No matches", "无匹配"),
        "terminal_search.match_count" => ("{current}/{total}", "{current}/{total}"),
        "terminal_search.previous" => ("Previous match (Shift+Enter)", "上一个匹配（Shift+Enter）"),
        "terminal_search.next" => ("Next match (Enter)", "下一个匹配（Enter）"),
        "terminal_search.find_selection" => {
            ("Find the selected terminal text", "查找当前选中的终端文本")
        }
        "terminal_search.selection" => ("Selection", "选择"),
        "terminal_search.online" => ("Online", "在线"),
        "terminal_search.online_search_tip" => (
            "Search the selected text online (only the selection is sent)",
            "在线搜索选中文本（仅发送所选内容）",
        ),
        "terminal_search.highlight" => ("Keep highlights after closing", "关闭后保留高亮"),
        "terminal_search.highlight_on" => ("Persistent highlights enabled", "已启用持续高亮"),
        "terminal_search.highlight_label" => ("Highlight", "高亮"),
        "terminal_search.close" => ("Close search (Esc)", "关闭查找（Esc）"),

        // ── layout.* — pane/tab layout UI ────────────────────────────────
        "layout.empty_pane" => ("Empty pane", "空白窗格"),
        "layout.empty_pane_clone_hint" => (
            "Click the ⧉ in the title bar to clone the focused session",
            "点击标题栏 ⧉ 复制焦点会话",
        ),
        "layout.close_pane" => (
            "Close this pane (remove from layout)",
            "关闭此窗格（从布局中移除）",
        ),
        "layout.close_pane_hint" => (
            "Close this pane (also Cmd+W / Ctrl+Shift+W)",
            "关闭此窗格（从布局中移除，Cmd+W / Ctrl+Shift+W 亦可）",
        ),
        "layout.clone_focused" => (
            "Clone focused session: {source_name}",
            "复制当前焦点会话：{source_name}",
        ),
        "layout.no_focused_to_clone" => ("No focused session to clone", "没有可复制的焦点会话"),
        "layout.drop_new_session" => (
            "Drag a left-sidebar session here to open a new session",
            "拖动左侧会话到此处新建会话",
        ),
        "layout.drop_session_compare" => (
            "Drag a left session = open new session for comparison",
            "拖动左侧会话 = 新开会话对比",
        ),
        "layout.drop_middle_replace" => ("Drop in the middle = replace", "拖到中间 = 替换"),
        "layout.drop_custom_conn" => (
            "Open the sidebar and drag a custom connection into this pane",
            "打开侧栏，将自定义连接拖入此窗格",
        ),
        "layout.or_drag_tab" => (
            "…or drag a tab/session title here",
            "或拖动标签页/会话标题到此处",
        ),
        "layout.move_float_hint" => (
            "Click to start moving the floating pane; click again or press Esc to stop",
            "单击开始移动小窗口，再次按下左键或按 Esc 停止",
        ),
        "layout.drag_to_repane" => (
            "Drag a session title to move it to another pane; ⠿ click to start/stop moving the floating pane",
            "拖动会话标题可移动到其他窗格；⠿ 单击开始/停止移动浮动窗",
        ),
        "layout.diff_too_large" => ("⚠ Large output diff detected", "⚠ 检测到大量输出差异"),
        "layout.diff_warning_body" => (
            "Comparison found {diff_rows} / {total_rows} rows ({pct}%) differ.\nThe output differs too much; highlighting every diff line may hurt readability.\nShow diff highlighting anyway?",
            "比对模式发现 {diff_rows} / {total_rows} 行（{pct}%）存在差异。\n输出的内容差异过大，高亮所有差异行可能会影响阅读。\n是否仍然显示差异高亮？",
        ),
        "layout.keep_showing" => ("Keep showing", "继续显示"),

        // ── ai_runtime.* — AI request status (app.rs) ────────────────────
        "ai_runtime.not_configured" => (
            "Model not configured: set OPENAI_API_KEY or ANTHROPIC_API_KEY in the launch environment.",
            "未配置模型：请在启动环境中设置 OPENAI_API_KEY 或 ANTHROPIC_API_KEY。",
        ),
        "ai_runtime.requesting" => (
            "Requesting model suggestions; sending only authorized execution results…",
            "正在请求模型建议；仅发送已授权的执行结果…",
        ),
        "ai_runtime.request_failed" => ("Model request failed: {error}", "模型请求失败：{error}"),
        "ai_runtime.returned" => (
            "Model returned {count} suggestion(s). They won't auto-run — review each one.",
            "模型返回 {count} 条建议。建议不会自动执行，请逐条审查。",
        ),
        "ai_runtime.wont_autorun" => (
            "Model suggestions won't auto-execute.",
            "模型建议不会自动执行。",
        ),
        "ai_runtime.no_active_session" => (
            "No active session to run suggestions against.",
            "没有可执行建议的活动会话。",
        ),
        "ai_runtime.approval_invalid" => (
            "Execution approval expired: {error}",
            "执行审批已失效：{error}",
        ),
        "ai_runtime.cannot_start_approval" => {
            ("Cannot start approval: {error}", "无法开始审批：{error}")
        }
        "ai_runtime.result_auth_invalid" => (
            "Result authorization expired: {error}",
            "结果授权已失效：{error}",
        ),

        // ── shadow.* — shadow sandbox dialog ─────────────────────────────
        // (Keys filled in by the shadow_sandbox_dialog conversion.)

        // Fallback: unknown key.
        _ => return None,
    };
    match lang {
        Language::En => Some(en),
        Language::Zh => Some(zh),
    }
}

/// Replace `{name}` placeholders in `template` with the supplied values.
/// A missing placeholder in the template is harmless; an unknown `{token}` is
/// left as-is so translators spot the typo.
fn interpolate(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = template.to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_key_returns_translation_for_each_language() {
        assert_eq!(translate("common.cancel", Language::En), Some("Cancel"));
        assert_eq!(translate("common.cancel", Language::Zh), Some("取消"));
    }

    #[test]
    fn unknown_key_returns_none_so_caller_falls_back() {
        assert_eq!(translate("nope.does.not.exist", Language::En), None);
    }

    #[test]
    fn interpolate_substitutes_named_placeholders() {
        let template = translate("cmd_status.failed", Language::En).unwrap();
        let s = interpolate(template, &[("exit_code", &127)]);
        assert_eq!(s, "✗ Failed (exit 127)");
    }

    #[test]
    fn every_key_has_non_empty_en_and_zh() {
        // Smoke-test a representative sample. A full parity check would need
        // a key list; the per-arm structure (two adjacent arms) makes a
        // mismatch a review-time concern.
        for key in [
            "common.cancel",
            "settings.language",
            "send.placeholder",
            "layout.empty_pane",
        ] {
            assert!(
                translate(key, Language::En).is_some(),
                "en missing for {key}"
            );
            assert!(
                translate(key, Language::Zh).is_some(),
                "zh missing for {key}"
            );
        }
    }

    #[test]
    fn language_label_is_in_native_script() {
        assert_eq!(Language::Zh.label(), "中文");
        assert_eq!(Language::En.label(), "English");
    }
}
