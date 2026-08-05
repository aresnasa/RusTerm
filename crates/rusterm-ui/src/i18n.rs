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
    t_for(key, current_language())
}

/// Resolve a translation key for an explicit language. This is useful for
/// deterministic helpers and tests that must not mutate the global language.
pub(crate) fn t_for(key: &str, language: Language) -> String {
    translate(key, language)
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
        "settings.search_placeholder" => ("Search all settings…", "搜索所有设置…"),
        "settings.search_no_results" => ("No matching settings", "没有匹配的设置"),

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
        "restore.will_replay" => (
            "Interactive sessions (e.g. jumpserver) replay their recorded setup steps",
            "✓ 交互式会话（如堡垒机）将回放已记录的建立操作",
        ),
        "restore.no_history_run" => (
            "No history commands or scripts will be executed",
            "✗ 不会执行任何历史命令或脚本",
        ),
        "restore.skip_hint" => (
            "Choose \"Skip\" to start with blank sessions; you'll be asked again next time a snapshot exists.",
            "选择“跳过”可使用空白会话开始；下次存在会话快照时仍会询问。",
        ),
        "restore.restore" => ("Restore", "恢复"),
        "restore.skip_blank" => ("Skip (start blank)", "跳过（开始空白会话）"),
        "restore.replay_badge" => ("replay", "回放"),

        // ── cmd_status.* — command status badge ─────────────────────────
        "cmd_status.connected" => ("✓ Connected", "✓ 已连接"),
        "cmd_status.connected_tip" => (
            "Session is connected (no command result to report yet)",
            "会话已连接（尚无命令执行结果）",
        ),
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
        "suggestion.snooze" => ("Mute this session", "本次不再提示"),
        "suggestion.snooze_tooltip" => (
            "Hide suggestions for this session only (new sessions show them again)",
            "本次会话不再提示（新会话会重新显示）",
        ),
        "suggestion.disable" => ("Disable entirely", "彻底关闭"),
        "suggestion.disable_tooltip" => (
            "Turn off command suggestions (re-enable in Settings)",
            "彻底关闭命令建议（可在设置中重新开启）",
        ),
        "suggestion.history_completion_title" => ("Complete from history", "补全历史"),
        "suggestion.history_completion_hint" => (
            "Ctrl+N/P selects · Enter inserts · Esc closes",
            "Ctrl+N/P 选择 · Enter 插入 · Esc 关闭",
        ),
        "popup.drag_grip_tooltip" => (
            "Drag to move the popup (position is remembered) · double-click to restore automatic placement",
            "拖动可移动弹窗（位置会被记住）· 双击恢复自动位置",
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
            "Command mode copies a reusable rusterm shell function. Run it as `rusterm uname -a`; the API password is requested once per shell and never placed in shell history.",
            "命令模式会复制一个可复用的 rusterm shell 函数，可直接执行 `rusterm uname -a`；每个 shell 仅询问一次 API 密码，且不会写入 shell 历史。",
        ),
        "api.command" => ("Command", "命令"),
        "api.command_edit_hint" => (
            "↓ Edit the remote command here. The curl script updates automatically.",
            "↓ 在这里输入要执行的远程命令；curl 脚本会自动更新。",
        ),
        "api.elevated" => ("Run with reusable sudo authorization", "复用 sudo 授权执行"),
        "api.session" => ("Session", "会话"),
        "api.sessions" => ("Sessions", "会话"),
        "api.selected_count" => ("{count} selected", "已选择 {count} 个"),
        "api.select_all" => ("All", "全选"),
        "api.clear_selection" => ("Clear", "清空"),
        "api.follow_active" => ("Follow active", "跟随活动"),
        "api.select_session_hint" => (
            "Select at least one session to generate a curl script.",
            "请至少选择一个会话以生成 curl 脚本。",
        ),
        "api.jumpserver_hint" => (
            "Bastion session: commands run on the target node the live tab is currently on (shown by the arrow).",
            "堡垒机会话：命令在当前标签页所在的目标节点上执行（由箭头标示）。",
        ),
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
        // ── Script mode (issue 73) ───────────────────────────────────────
        "api.mode_command" => ("Command", "命令"),
        "api.mode_script" => ("Script", "脚本"),
        "api.mode_script_base64" => ("Script (base64)", "脚本 (base64)"),
        "api.templates_label" => ("Templates", "模板"),
        "api.add_template" => ("+ Add template", "+ 新增模板"),
        "api.template_name_placeholder" => ("Template name", "模板名称"),
        "api.template_body_placeholder" => ("Command or script content", "命令或脚本内容"),
        "api.template_save" => ("Save", "保存"),
        "api.template_cancel" => ("Cancel", "取消"),
        "api.template_delete" => ("Delete template", "删除模板"),
        "api.template_fill_required" => (
            "Template name and content are required",
            "模板名称和内容不能为空",
        ),
        "api.script_label" => ("Script", "脚本"),
        "api.script_edit_hint" => (
            "↓ Multi-line shell script. Each line passes the hard-floor validator; the whole script is scanned by dcg if installed.",
            "↓ 多行 shell 脚本。每行都经过硬底线校验；若已安装 dcg，整段脚本会被额外扫描。",
        ),
        "api.script_base64_label" => ("Script (base64)", "脚本 (base64)"),
        "api.script_base64_edit_hint" => (
            "↓ Paste a base64-encoded script. Decoded before validation; standard and URL-safe alphabets accepted.",
            "↓ 粘贴 base64 编码的脚本。解码后校验；支持标准和 URL-safe 字符表。",
        ),
        "api.script_too_long" => (
            "Script exceeds 64 KiB or 4096 lines.",
            "脚本超过 64 KiB 或 4096 行。",
        ),
        "api.base64_invalid" => (
            "Not valid base64 (standard or URL-safe).",
            "不是有效的 base64（标准或 URL-safe）。",
        ),
        "api.dcg_blocked" => ("dcg blocked: {reason}", "dcg 拒绝：{reason}"),
        "api.sandbox_failed" => ("sandbox failed: {reason}", "沙盒校验失败：{reason}"),
        "api.script_marker_title" => ("EDIT REMOTE SCRIPT BELOW", "在下方修改远程脚本"),
        "api.script_marker_help" => (
            "The script is forwarded verbatim to the SSH target's login shell. Multi-line constructs (heredocs, loops) are preserved.",
            "脚本会原样转发到 SSH 目标的登录 shell。多行结构（heredoc、循环）会被保留。",
        ),

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
        "session.disconnected_short" => ("Disconnected", "已断开"),
        "session.reconnect_hint" => (
            "Press Enter or right-click to reconnect",
            "按 Enter 或右键重新连接",
        ),
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
            "Found {count} private key file(s) in ~/ssh/",
            "提示：从 ~/.ssh/ 找到 {count} 个私钥文件",
        ),
        // ── connection.* — quick-entry / protocol tabs / serial fields ───
        "connection.quick_entry_label" => ("Quick entry", "快速录入"),
        "connection.quick_entry_help" => (
            "Paste user@host -p 22, host:port, or telnet://host:23 — fields below auto-fill.",
            "粘贴 user@host -p 22、host:port 或 telnet://host:23，下方字段会自动填充。",
        ),
        "connection.serial_device" => ("Device", "设备路径"),
        "connection.serial_ports_hint" => {
            ("Found {count} serial port(s)", "检测到 {count} 个可用串口")
        }
        "connection.baud_rate" => ("Baud rate", "波特率"),
        "connection.data_bits" => ("Data bits", "数据位"),
        "connection.parity" => ("Parity", "校验"),
        "connection.stop_bits" => ("Stop bits", "停止位"),
        "connection.flow_control" => ("Flow control", "流控"),
        "connection.host" => ("Host", "主机"),
        "connection.port" => ("Port", "端口"),
        "connection.name" => ("Name", "名称"),
        "connection.name_placeholder" => ("My connection", "我的连接"),
        "connection.group" => ("Group", "分组"),
        "connection.username" => ("Username", "用户名"),
        "connection.password" => ("Password", "密码"),
        "connection.key" => ("Key", "密钥"),
        "connection.agent" => ("Agent", "代理"),
        "connection.authentication" => ("Authentication", "认证方式"),
        "connection.private_key_path" => ("Private key path", "私钥路径"),
        "connection.passphrase_optional" => ("Passphrase (optional)", "口令（可选）"),
        "connection.passphrase_placeholder" => ("passphrase", "口令"),
        "connection.password_placeholder" => ("password", "密码"),
        "connection.password_keep_placeholder" => {
            ("leave blank to keep current", "留空则保留原密码")
        }
        "connection.agent_hint" => (
            "Will use keys loaded in ssh-agent (SSH_AUTH_SOCK).",
            "使用 ssh-agent（SSH_AUTH_SOCK）中已加载的密钥。",
        ),
        "connection.proxy" => ("Proxy", "代理"),
        "connection.proxy_direct" => ("Direct (no proxy)", "直连（无代理）"),
        "connection.proxy_host" => ("Proxy host", "代理主机"),
        "connection.proxy_username_optional" => ("Proxy username (optional)", "代理用户名（可选）"),
        "connection.proxy_password_optional" => ("Proxy password (optional)", "代理密码（可选）"),
        "connection.proxy_help" => (
            "HTTPS means TLS to the proxy then HTTP CONNECT to the SSH target.",
            "HTTPS 表示先与代理建立 TLS，再通过 HTTP CONNECT 到达 SSH 目标。",
        ),
        "connection.terminal_type" => ("Terminal type", "终端类型"),
        "connection.onekey_connect" => ("One-Key Connect", "一键连接"),
        "connection.onekey_hint" => (
            "Auto-fill sudo / su passwords via a popup when prompted.",
            "遇到 sudo / su 密码提示时自动通过弹窗填充。",
        ),
        "connection.login_script_label" => ("Login script (optional)", "登录脚本（可选）"),
        "connection.login_script_help" => (
            "expect/send lines to run after login. See docs for grammar.",
            "登录后执行的 expect/send 脚本，语法见文档。",
        ),
        "connection.new_title" => ("New connection", "新建连接"),
        "connection.edit_title" => ("Edit connection", "编辑连接"),
        "connection.connect" => ("Connect", "连接"),

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

        // ── ai_runtime.local.* — local LLM template generation ─────────
        "ai_runtime.local.enable" => ("Local AI Template Generation", "本地 AI 模板生成"),
        "ai_runtime.local.enable_hint" => (
            "Run a small LLM on this machine to generate scripts offline. ~1 GB RAM needed. Models download from the configured mirror.",
            "在本机运行小型 LLM 离线生成脚本，需约 1 GB 内存。模型从配置的镜像下载。",
        ),
        "ai_runtime.local.hw_warning" => ("Hardware warning: {warning}", "硬件警告：{warning}"),
        "ai_runtime.local.enable_anyway" => ("Enable anyway", "仍然启用"),
        "ai_runtime.local.downloading" => ("Downloading {file}…", "正在下载 {file}…"),
        "ai_runtime.local.quantizing" => (
            "Quantizing tensor {current}/{total}: {tensor}",
            "正在量化张量 {current}/{total}：{tensor}",
        ),
        "ai_runtime.local.preparing" => (
            "Preparing local model (first run downloads ~3 GB)…",
            "正在准备本地模型（首次运行需下载约 3 GB）…",
        ),
        "ai_runtime.local.loading" => ("Loading model into memory…", "正在加载模型到内存…"),
        "ai_runtime.local.generating" => ("Generating template…", "正在生成模板…"),
        "ai_runtime.local.generate_failed" => ("Generation failed: {error}", "生成失败：{error}"),
        "ai_runtime.local.empty_generation" => {
            ("The model returned an empty template", "模型返回了空模板")
        }
        "ai_runtime.local.ready" => ("Local model ready", "本地模型就绪"),
        "ai_runtime.local.download_model" => ("Download model", "下载模型"),
        "ai_runtime.local.download_hint" => (
            "Downloads about 3 GB, then keeps an approximately 1 GB quantized model.",
            "需下载约 3 GB，完成后保留约 1 GB 的量化模型。",
        ),
        "ai_runtime.local.download_starting" => ("Starting model download…", "正在启动模型下载…"),
        "ai_runtime.local.download_background" => (
            "{model} is still downloading in the background.",
            "{model} 仍在后台下载。",
        ),
        "ai_runtime.local.download_failed" => {
            ("Model download failed: {error}", "模型下载失败：{error}")
        }
        "ai_runtime.local.cache_unavailable" => (
            "Cannot find a writable model cache directory.",
            "无法找到可写的模型缓存目录。",
        ),
        "ai_runtime.local.model_not_ready" => (
            "The current model has not been downloaded. Open Settings → Local AI Template Generation and download it first.",
            "当前模型尚未下载，请前往设置 → 本地 AI 模板生成，先下载模型。",
        ),
        "ai_runtime.local.ai_generate" => ("✨ AI Generate", "✨ AI 生成"),
        "ai_runtime.local.ai_generate_hint" => (
            "Describe the command or script template you want",
            "描述你想要的命令或脚本模板",
        ),
        "ai_runtime.local.kind_command" => ("Command", "命令"),
        "ai_runtime.local.kind_shell" => ("Shell", "Shell"),
        "ai_runtime.local.kind_python" => ("Python", "Python"),
        "ai_runtime.local.base64_hint" => (
            "AI writes the source script; RusTerm encodes it as Base64 automatically.",
            "AI 生成原始脚本，RusTerm 会自动编码为 Base64。",
        ),
        "ai_runtime.local.manual_template" => {
            ("Or save the current content manually", "或手动保存当前内容")
        }
        "ai_runtime.local.suggestion_title" => ("AI suggestion", "AI 建议"),
        "ai_runtime.local.suggestion_apply" => ("Apply", "应用"),
        "ai_runtime.local.suggestion_saved" => ("Auto-saved", "已自动保存"),
        "ai_runtime.local.suggestion_exists" => ("Already in templates", "模板中已存在"),
        "ai_runtime.local.suggestion_save_failed" => {
            ("Auto-save failed: {error}", "自动保存失败：{error}")
        }
        "ai_runtime.local.suggestion_config_unavailable" => {
            ("Settings storage is unavailable", "设置存储不可用")
        }

        // ── ai_runtime.local.* — mirror URL + model selector + custom models ─
        "ai_runtime.local.mirror_url" => ("Download mirror", "下载镜像"),
        "ai_runtime.local.mirror_url_hint" => (
            "HuggingFace endpoint for model downloads. Use https://hf-mirror.com in China, or https://huggingface.co for direct access.",
            "HuggingFace 模型下载镜像地址。国内使用 https://hf-mirror.com，直连请用 https://huggingface.co。",
        ),
        "ai_runtime.local.model_select" => ("Active model", "当前模型"),
        "ai_runtime.local.custom_form_show" => ("+ Add custom model", "+ 添加自定义模型"),
        "ai_runtime.local.custom_form_hide" => ("Cancel custom model", "取消添加自定义模型"),
        "ai_runtime.local.custom_form_title" => ("Custom model details", "自定义模型详情"),
        "ai_runtime.local.custom_name" => ("Display name", "显示名称"),
        "ai_runtime.local.custom_repo" => ("HuggingFace repo ID", "HuggingFace 仓库 ID"),
        "ai_runtime.local.custom_template" => (
            "Prompt template (must contain {prompt})",
            "提示词模板（必须包含 {prompt}）",
        ),
        "ai_runtime.local.custom_eos" => ("EOS token", "结束符"),
        "ai_runtime.local.custom_add" => ("Add", "添加"),
        "ai_runtime.local.custom_err_empty" => ("All fields are required.", "所有字段都必须填写。"),
        "ai_runtime.local.custom_err_template" => {
            ("Template must contain {prompt}.", "模板必须包含 {prompt}。")
        }
        "ai_runtime.local.custom_err_dup" => {
            ("A model with this name already exists.", "同名模型已存在。")
        }

        // ── send.* — additional send-panel state ───────────────────────
        "send.no_target" => ("No target", "无目标"),

        // ── session.* — connection lifecycle messages ─────────────────
        "session.connecting_to" => ("Connecting to {name}…", "正在连接到 {name}…"),
        "session.connection_failed" => ("Connection failed: {error}", "连接失败：{error}"),
        "session.connection_type_not_supported" => (
            "This connection type does not support reconnecting",
            "此连接类型不支持重新连接",
        ),
        "session.press_enter_to_reconnect" => ("Press Enter to reconnect", "按 Enter 重新连接"),
        "session.reconnecting" => ("Reconnecting…", "正在重新连接…"),
        "session.replaying_ops" => (
            "Replaying {count} recorded operation(s) to restore the session state…",
            "正在回放 {count} 条已记录的操作以恢复会话状态…",
        ),
        "session.replay_skipped_unsafe" => (
            "Skipped {count} potentially dangerous operation(s) during replay",
            "回放时已跳过 {count} 条潜在危险操作",
        ),
        "session.replay_paused_credential" => (
            "Replay paused: the remote is asking for credentials — please enter them manually to continue",
            "回放已暂停：远程正在请求凭据，请手动输入密码后继续",
        ),
        "session.shell_failed" => ("Failed to start shell: {error}", "启动 shell 失败：{error}"),
        "session.starting_shell" => ("Starting local shell…", "正在启动本地终端…"),
        // Tab-bar context-menu actions (Task: disconnect/reconnect/copy session).
        "session.disconnect" => ("Disconnect", "断开连接"),
        "session.reconnect" => ("Reconnect", "重新连接"),
        "session.copy_session" => ("Copy Session", "复制会话"),
        "session.disconnected_by_user" => ("Disconnected by user", "已由用户断开"),
        "session.no_config_for_copy" => (
            "This session has no stored login config and cannot be copied",
            "此会话没有已保存的登录配置，无法复制",
        ),

        // ── sessions.* — sessions panel ────────────────────────────────
        "sessions.empty_pane" => ("Empty pane", "空白窗格"),
        "sessions.no_open_workspaces" => ("No open workspaces", "没有打开的工作区"),
        "sessions.open_count" => ("{count} open session(s)", "{count} 个打开的会话"),
        "sessions.pane_label" => ("Pane {index}", "窗格 {index}"),
        "sessions.select" => ("Select session {name}", "选择会话 {name}"),
        "sessions.status_connected" => ("Connected", "已连接"),
        "sessions.status_connecting" => ("Connecting", "正在连接"),
        "sessions.status_disconnected" => ("Disconnected", "已断开"),
        "sessions.status_failed" => ("Connection failed", "连接失败"),
        "sessions.status_reconnecting" => ("Reconnecting", "正在重新连接"),
        "sessions.title" => ("Sessions", "会话"),
        "sessions.workspace_label" => ("Workspace {index}: {label}", "工作区 {index}：{label}"),

        // ── shell.* — local-shell session names ────────────────────────
        "shell.bottom_session_name" => ("Bottom shell", "底部终端"),
        "shell.local_session_name" => ("Local shell", "本地终端"),

        // ── status.* — status bar controls ─────────────────────────────
        "status.ai" => ("AI", "AI"),
        "status.bottom" => ("Bottom", "底部"),
        "status.chat" => ("Chat", "聊天"),
        "status.left" => ("Left", "左侧"),
        "status.llm_opt_in" => ("LLM opt-in", "LLM 已选择加入"),
        "status.local" => ("Local", "本地"),
        "status.local_tooltip" => ("Open a local shell", "打开本地终端"),
        "status.logging" => ("Logging", "正在记录日志"),
        "status.onekeys" => ("OneKeys", "OneKeys"),
        "status.relay" => ("Relay", "中转"),
        "status.relay_tooltip" => ("Configure the REST API relay", "配置 REST API 中转"),
        "status.right" => ("Right", "右侧"),
        "status.sessions" => ("{count} sessions", "{count} 个会话"),
        "status.toggle_bottom_dock" => ("Toggle bottom dock", "切换底部面板"),
        "status.toggle_chat" => (
            "Toggle agent chat (Cmd+Shift+Space)",
            "切换智能体聊天（Cmd+Shift+空格）",
        ),
        "status.toggle_left_dock" => ("Toggle left dock", "切换左侧面板"),
        "status.toggle_right_dock" => ("Toggle right dock", "切换右侧面板"),
        "status.tunnels" => ("Tunnels", "隧道"),
        "status.tunnels_tooltip" => ("Manage SSH tunnels", "管理 SSH 隧道"),

        // ── transfer.* / transfers.* — SFTP transfer queue ─────────────
        "transfer.local_remote_endpoints_required" => (
            "A transfer requires one local and one remote endpoint",
            "传输必须包含一个本地端点和一个远程端点",
        ),
        "transfer.ssh_disconnected_while_opening_sftp" => (
            "SSH session disconnected while opening SFTP",
            "打开 SFTP 时 SSH 会话已断开",
        ),
        "transfer.ssh_session_not_connected" => ("SSH session is not connected", "SSH 会话未连接"),
        "transfers.clear_finished" => ("Clear finished", "清除已完成"),
        "transfers.clear_finished_hint" => ("Clear finished transfers", "清除已完成的传输"),
        "transfers.destination_title" => ("Destination: {destination}", "目标：{destination}"),
        "transfers.direction_download" => ("Download", "下载"),
        "transfers.direction_transfer" => ("Transfer", "传输"),
        "transfers.direction_upload" => ("Upload", "上传"),
        "transfers.empty_state" => ("No transfers", "没有传输任务"),
        "transfers.endpoint_local" => ("Local: {path}", "本地：{path}"),
        "transfers.endpoint_remote" => ("Remote: {path}", "远程：{path}"),
        "transfers.from" => ("From {source}", "来源：{source}"),
        "transfers.no_finished_hint" => {
            ("No finished transfers to clear", "没有可清除的已完成传输")
        }
        "transfers.source_title" => ("Source: {source}", "来源：{source}"),
        "transfers.status_cancelled" => ("Cancelled", "已取消"),
        "transfers.status_completed" => ("Completed", "已完成"),
        "transfers.status_failed" => ("Failed", "失败"),
        "transfers.status_failed_reason" => ("Failed: {reason}", "失败：{reason}"),
        "transfers.status_queued" => ("Queued", "等待中"),
        "transfers.status_running" => ("Transferring", "传输中"),
        "transfers.to" => ("To {destination}", "目标：{destination}"),
        "transfers.unnamed_file" => ("Unnamed file", "未命名文件"),

        // ── tunnels.* — SSH tunnel manager ─────────────────────────────
        "tunnels.auto_reconnect" => ("Reconnect automatically", "自动重新连接"),
        "tunnels.auto_start" => ("Start automatically", "自动启动"),
        "tunnels.check_port" => ("Check port", "检查端口"),
        "tunnels.connection_required" => ("Select an SSH connection", "请选择 SSH 连接"),
        "tunnels.edit" => ("Edit", "编辑"),
        "tunnels.edit_tunnel" => ("Edit tunnel", "编辑隧道"),
        "tunnels.empty" => ("No tunnels configured", "尚未配置隧道"),
        "tunnels.invalid_listen_address" => ("Invalid listen address", "监听地址无效"),
        "tunnels.invalid_listen_port" => ("Invalid listen port", "监听端口无效"),
        "tunnels.invalid_remote_port" => ("Invalid remote port", "远程端口无效"),
        "tunnels.listen_addr_port" => ("Listen address and port", "监听地址和端口"),
        "tunnels.listen_port_zero" => ("Listen port cannot be 0", "监听端口不能为 0"),
        "tunnels.manager_uninitialized" => {
            ("Tunnel manager is not initialized", "隧道管理器尚未初始化")
        }
        "tunnels.name" => ("Name", "名称"),
        "tunnels.name_required" => ("Tunnel name is required", "隧道名称不能为空"),
        "tunnels.new_tunnel" => ("New tunnel", "新建隧道"),
        "tunnels.new_tunnel_button" => ("New tunnel", "新建隧道"),
        "tunnels.port_in_use" => ("Port is already in use", "端口已被占用"),
        "tunnels.remote_host_port" => ("Remote host and port", "远程主机和端口"),
        "tunnels.remote_host_required" => ("Remote host is required", "远程主机不能为空"),
        "tunnels.save_and_start" => ("Save and start", "保存并启动"),
        "tunnels.ssh_connection" => ("SSH connection", "SSH 连接"),
        "tunnels.start" => ("Start", "启动"),
        "tunnels.state_active" => (
            "Active for {minutes}m {seconds}s",
            "已运行 {minutes} 分 {seconds} 秒",
        ),
        "tunnels.state_connecting" => (
            "Connecting (attempt {attempt})",
            "正在连接（第 {attempt} 次）",
        ),
        "tunnels.state_failed" => ("Failed: {error}", "失败：{error}"),
        "tunnels.state_reconnecting" => (
            "Reconnecting (attempt {attempt}, in {delay_ms} ms): {error}",
            "正在重新连接（第 {attempt} 次，{delay_ms} 毫秒后）：{error}",
        ),
        "tunnels.state_stopped" => ("Stopped", "已停止"),
        "tunnels.stop" => ("Stop", "停止"),
        "tunnels.suggest_free_ports" => ("Suggest free ports", "推荐可用端口"),
        "tunnels.title" => ("SSH tunnels", "SSH 隧道"),
        "tunnels.type" => ("Type", "类型"),
        "tunnels.type_dynamic_socks" => ("Dynamic SOCKS5 (-D)", "动态 SOCKS5（-D）"),
        "tunnels.type_local_forward" => ("Local TCP forward (-L)", "本地 TCP 转发（-L）"),
        "tunnels.unknown_kind" => ("Unknown tunnel type: {kind}", "未知隧道类型：{kind}"),

        // ── welcome.* — empty workspace ────────────────────────────────
        "welcome.create_connection" => ("Create a connection", "新建连接"),

        // ── api.* — keys reserved for the endpoint/curl UI follow-up ───
        "api.endpoints" => ("Endpoints", "端点"),
        "api.endpoint_reference" => (
            "GET  {url}/api/v1/health      # liveness, no auth\nGET  {url}/api/v1/hosts        # list hosts (BasicAuth, JSON)\nGET  {url}/r                    # list host_ids (BasicAuth, plain text)\nPOST {url}/r/{host_id}         # plain command body, plain stdout\nPOST {url}/api/v1/exec         # { host_id, command, elevated?, timeout_ms? }\nPOST {url}/api/v1/parse-curl   # parse a pasted curl into JSON",
            "GET  {url}/api/v1/health      # 存活检查，无需鉴权\nGET  {url}/api/v1/hosts        # 列出主机（BasicAuth，JSON）\nGET  {url}/r                    # 列出 host_id（BasicAuth，纯文本）\nPOST {url}/r/{host_id}         # 纯文本命令请求体，直接返回 stdout\nPOST {url}/api/v1/exec         # { host_id, command, elevated?, timeout_ms? }\nPOST {url}/api/v1/parse-curl   # 将粘贴的 curl 解析为 JSON",
        ),
        "api.password_prompt" => ("RusTerm API password: ", "RusTerm API 密码："),
        "api.function_usage" => ("Usage: rusterm <command...>", "用法：rusterm <命令...>"),
        "api.command_marker_title" => (
            "RUN NOW; THE FUNCTION REMAINS REUSABLE",
            "立即执行；此函数后续仍可复用",
        ),
        "api.command_marker_help" => (
            "Use rusterm <command...>; quote commands containing &&, pipes, or redirects. The function always targets the sessions selected when it was copied.",
            "使用 rusterm <命令...>；包含 &&、管道或重定向时请为命令加引号。此函数始终以复制时勾选的会话为目标。",
        ),
        "api.request_failed" => (
            "One or more RusTerm API requests failed (last status: %s).",
            "一个或多个 RusTerm API 请求失败（最后状态：%s）。",
        ),
        "api.no_hosts" => (
            "No hosts were selected when this function was copied.",
            "复制此函数时未选择任何主机。",
        ),
        "api.missing_config" => (
            "RUSTERM_API_URL is not set; export it first (e.g. export RUSTERM_API_URL=http://127.0.0.1:8877).",
            "RUSTERM_API_URL 未设置；请先导出（例如 export RUSTERM_API_URL=http://127.0.0.1:8877）。",
        ),
        "api.missing_user" => (
            "RUSTERM_API_USER is not set; export it first (e.g. export RUSTERM_API_USER=yourname).",
            "RUSTERM_API_USER 未设置；请先导出（例如 export RUSTERM_API_USER=你的用户名）。",
        ),
        "api.password_not_tty" => (
            "RUSTERM_API_PASSWORD is not set and stdin is not a terminal; export it first.",
            "RUSTERM_API_PASSWORD 未设置且 stdin 不是终端；请先导出。",
        ),

        // ── common / connection / sidebar additions ───────────────────
        "common.active" => ("Active", "活动"),
        "common.focused" => ("Focused", "已聚焦"),
        "connection.copy_name" => ("{name} copy", "{name} 副本"),
        "connection.delete_body" => (
            "Delete connection \"{name}\"? This cannot be undone.",
            "是否删除连接“{name}”？此操作无法撤销。",
        ),
        "connection.delete_title" => ("Delete connection", "删除连接"),
        "connections.add_group" => ("Add group", "添加分组"),
        "connections.all_hidden_hint" => (
            "All matching connections are hidden",
            "所有匹配的连接均已隐藏",
        ),
        "connections.configure_onekeys" => ("Configure OneKey", "配置 OneKey"),
        "connections.connect" => ("Connect", "连接"),
        "connections.copy" => ("Copy", "复制"),
        "connections.create" => ("Create connection", "新建连接"),
        "connections.create_group" => ("Create group", "新建分组"),
        "connections.delete" => ("Delete connection", "删除连接"),
        "connections.delete_group_hint" => ("Delete this group", "删除此分组"),
        "connections.edit" => ("Edit", "编辑"),
        "connections.empty_hint" => ("No saved connections", "没有已保存的连接"),
        "connections.group_name_placeholder" => ("Group name", "分组名称"),
        "connections.hide_from_sidebar" => ("Hide from sidebar", "从侧栏隐藏"),
        "connections.hide_hidden_again" => ("Hide hidden connections", "再次隐藏已隐藏的连接"),
        "connections.kind_serial" => ("Serial", "串口"),
        "connections.kind_shell" => ("Local shell", "本地终端"),
        "connections.kind_ssh" => ("SSH", "SSH"),
        "connections.kind_tcp" => ("TCP", "TCP"),
        "connections.kind_telnet" => ("Telnet", "Telnet"),
        "connections.move_to_group" => ("Move to group", "移动到分组"),
        "connections.no_matches" => ("No matching connections", "没有匹配的连接"),
        "connections.onekey_enabled" => ("OneKey enabled", "已启用 OneKey"),
        "connections.resize_sidebar" => ("Drag to resize the sidebar", "拖动以调整侧栏宽度"),
        "connections.search_placeholder" => ("Search connections…", "搜索连接…"),
        "connections.show_hidden" => ("Show hidden connections", "显示已隐藏的连接"),
        "connections.show_in_sidebar" => ("Show in sidebar", "在侧栏显示"),
        "connections.title" => ("Connections", "连接"),
        "connections.ungrouped" => ("Ungrouped", "未分组"),
        "connections.ungrouped_count" => ("Ungrouped ({count})", "未分组（{count}）"),
        "dock.drag_panel" => ("Drag {panel} panel", "拖动{panel}面板"),
        "dock.hide_bottom" => ("Hide bottom dock", "隐藏底部面板"),
        "dock.hide_left" => ("Hide left dock", "隐藏左侧面板"),
        "dock.hide_right" => ("Hide right dock", "隐藏右侧面板"),

        // ── history / layout additions ─────────────────────────────────
        "history.current_session_only" => ("Current session only", "仅当前会话"),
        "history.current_session_only_named" => {
            ("Current session only: {name}", "仅当前会话：{name}")
        }
        "history.cwd_meta" => ("{cwd}", "{cwd}"),
        "history.cwd_tooltip" => ("Working directory: {cwd}", "工作目录：{cwd}"),
        "history.description" => (
            "Search commands recorded from terminal sessions",
            "搜索终端会话中记录的命令",
        ),
        "history.double_click_to_run" => ("Double-click to run", "双击运行"),
        "history.host_meta" => ("{hostname}", "{hostname}"),
        "history.host_tooltip" => ("Host: {hostname}", "主机：{hostname}"),
        "history.load_error" => (
            "Failed to load history: {error}",
            "加载历史记录失败：{error}",
        ),
        "history.load_more" => ("Load more", "加载更多"),
        "history.loading" => ("Loading history…", "正在加载历史记录…"),
        "history.loading_more" => ("Loading more…", "正在加载更多…"),
        "history.no_focused_session" => ("No focused session", "没有聚焦的会话"),
        "history.no_matches" => ("No matching commands", "没有匹配的命令"),
        "history.search_placeholder" => ("Search command history…", "搜索命令历史…"),
        "history.time_tooltip" => ("Executed at {time}", "执行时间：{time}"),
        "history.title" => ("History", "历史记录"),
        "layout.compare" => ("Compare", "比对"),
        "layout.compare_tooltip" => ("Compare pane output", "比对窗格输出"),
        "layout.comparison_diff_rows" => ("{count} different rows", "{count} 行不同"),
        "layout.comparison_identical" => ("Outputs are identical", "输出完全相同"),
        "layout.comparison_on" => ("Comparison on", "比对已开启"),
        "layout.distribute" => ("Distribute", "均分"),
        "layout.distribute_tooltip" => ("Distribute panes evenly", "均匀分布窗格"),
        "layout.pane" => ("pane", "个窗格"),
        "layout.pane_count_tooltip" => ("Current pane layout", "当前窗格布局"),
        "layout.panes" => ("panes", "个窗格"),
        "layout.resize_left_right_split" => (
            "Drag to resize the left/right split",
            "拖动以调整左右分割比例",
        ),
        "layout.resize_top_bottom_split" => (
            "Drag to resize the top/bottom split",
            "拖动以调整上下分割比例",
        ),
        "layout.split" => ("Split", "分屏"),
        "layout.split_tooltip" => ("Toggle split layout", "切换分屏布局"),
        "layout.summary" => ("Layout: {count} {panes}", "布局：{count} {panes}"),
        "layout.summary_tab_tiled" => (
            "Layout: {count} {panes} (tabs)",
            "布局：{count} {panes}（标签平铺）",
        ),
        "layout.zoom_tooltip" => ("Zoom the focused pane", "缩放聚焦窗格"),
        "master_password.error" => ("Master password error: {error}", "主密码错误：{error}"),
        "master_password.invalid" => ("Invalid master password", "主密码无效"),
        "onekey.credential" => ("Credential", "凭据"),
        "onekey.credential_password" => ("Password", "密码"),
        "onekey.credential_token" => ("Token", "令牌"),
        "onekey.credential_username" => ("Username", "用户名"),
        "onekey.saved_credential" => ("Saved {credential}", "已保存的{credential}"),

        // ── relay.* — REST API relay configuration ─────────────────────
        "relay.account_required_before_start" => (
            "Add at least one account before starting the relay",
            "启动中转前请至少添加一个账号",
        ),
        "relay.add_account" => ("Add account", "添加账号"),
        "relay.add_update_account" => ("Add or update account", "添加或更新账号"),
        "relay.all_commands_validated" => ("All commands (validated)", "所有命令（需校验）"),
        "relay.allowed_commands_help" => (
            "Allowed command regexes (comma-separated)",
            "允许的命令正则表达式（逗号分隔）",
        ),
        "relay.allowed_hosts_help" => ("Allowed hosts (comma-separated)", "允许的主机（逗号分隔）"),
        "relay.audit_log" => ("Audit log: {path}", "审计日志：{path}"),
        "relay.commands" => ("Commands: {commands}", "命令：{commands}"),
        "relay.confirm_public_bind" => (
            "I understand; allow public binding",
            "我已了解，允许公开监听",
        ),
        "relay.confirm_public_bind_before_start" => (
            "Confirm public binding before starting the relay",
            "启动中转前请确认公开监听",
        ),
        "relay.hashing_failed" => ("Password hashing failed: {error}", "密码哈希失败：{error}"),
        "relay.hosts" => ("Hosts: {hosts}", "主机：{hosts}"),
        "relay.invalid_bind_addr" => ("Invalid bind address: {value}", "监听地址无效：{value}"),
        "relay.invalid_regex_indices" => (
            "Invalid command regex at index/indices {indices}",
            "以下索引处的命令正则无效：{indices}",
        ),
        "relay.password_hash_note" => ("Password (stored as a hash)", "密码（以哈希形式存储）"),
        "relay.password_required" => ("Password is required", "密码不能为空"),
        "relay.public_bind_warning" => (
            "This address exposes the relay beyond this computer",
            "此地址会将中转服务暴露到本机之外",
        ),
        "relay.readonly_help" => ("Read-only account", "只读账号"),
        "relay.save_config" => ("Save configuration", "保存配置"),
        "relay.server" => ("Server", "服务器"),
        "relay.username_required" => ("Username is required", "用户名不能为空"),

        // ── remote_files.* — remote file manager and SFTP ──────────────
        "remote_files.apply" => ("Apply", "应用"),
        "remote_files.applying_operation" => ("Applying operation…", "正在执行操作…"),
        "remote_files.choosing_download_destination" => {
            ("Choose a download destination…", "请选择下载位置…")
        }
        "remote_files.choosing_local_file" => ("Choose a local file…", "请选择本地文件…"),
        "remote_files.connect_ssh_hint" => (
            "Connect an SSH session to browse remote files",
            "连接 SSH 会话以浏览远程文件",
        ),
        "remote_files.connections" => ("Connections", "连接"),
        "remote_files.create_directory_failed" => (
            "Failed to create directory: {error}",
            "创建目录失败：{error}",
        ),
        "remote_files.created_directory" => ("Created directory {name}", "已创建目录 {name}"),
        "remote_files.delete_directory_confirmation" => (
            "Delete empty directory \"{name}\"?",
            "是否删除空目录“{name}”？",
        ),
        "remote_files.delete_empty_directory_failed" => (
            "Failed to delete empty directory: {error}",
            "删除空目录失败：{error}",
        ),
        "remote_files.delete_entry_failed" => {
            ("Failed to delete entry: {error}", "删除项目失败：{error}")
        }
        "remote_files.delete_file_confirmation" => {
            ("Delete file \"{name}\"?", "是否删除文件“{name}”？")
        }
        "remote_files.delete_symlink_confirmation" => (
            "Delete symbolic link \"{name}\"?",
            "是否删除符号链接“{name}”？",
        ),
        "remote_files.deleted" => ("Deleted {name}", "已删除 {name}"),
        "remote_files.dialog_create_title" => ("Create folder", "新建文件夹"),
        "remote_files.dialog_delete_title" => ("Delete remote entry", "删除远程项目"),
        "remote_files.dialog_rename_title" => ("Rename remote entry", "重命名远程项目"),
        "remote_files.download" => ("Download", "下载"),
        "remote_files.download_cancelled" => ("Download cancelled", "已取消下载"),
        "remote_files.download_queued" => ("Queued download: {name}", "已加入下载队列：{name}"),
        "remote_files.empty_directory" => ("This directory is empty", "此目录为空"),
        "remote_files.file_type_directory" => ("Directory", "目录"),
        "remote_files.file_type_file" => ("File", "文件"),
        "remote_files.file_type_other" => ("Other", "其他"),
        "remote_files.file_type_symlink" => ("Symbolic link", "符号链接"),
        "remote_files.go" => ("Go", "转到"),
        "remote_files.invalid_utf8_filename" => (
            "The selected filename is not valid UTF-8",
            "所选文件名不是有效的 UTF-8",
        ),
        "remote_files.list_failed" => {
            ("Failed to list directory: {error}", "列出目录失败：{error}")
        }
        "remote_files.loading" => ("Loading remote files…", "正在加载远程文件…"),
        "remote_files.name_dot" => ("Name cannot be . or ..", "名称不能是 . 或 .."),
        "remote_files.name_empty" => ("Name cannot be empty", "名称不能为空"),
        "remote_files.name_slash" => ("Name cannot contain /", "名称不能包含 /"),
        "remote_files.new_folder" => ("New folder", "新建文件夹"),
        "remote_files.no_ssh_sessions" => ("No connected SSH sessions", "没有已连接的 SSH 会话"),
        "remote_files.open_directory_hint" => ("Open directory", "打开目录"),
        "remote_files.open_local_manager" => ("Open file manager", "打开文件管理器"),
        "remote_files.open_sftp_failed" => {
            ("Failed to open SFTP: {error}", "打开 SFTP 失败：{error}")
        }
        "remote_files.parent_directory" => ("Parent directory", "上级目录"),
        "remote_files.path_absolute" => ("Path must be absolute", "路径必须是绝对路径"),
        "remote_files.read_local_metadata_failed" => (
            "Failed to read local file metadata: {error}",
            "读取本地文件元数据失败：{error}",
        ),
        "remote_files.refresh" => ("Refresh", "刷新"),
        "remote_files.rename" => ("Rename", "重命名"),
        "remote_files.rename_failed" => {
            ("Failed to rename entry: {error}", "重命名项目失败：{error}")
        }
        "remote_files.renamed_to" => ("Renamed to {name}", "已重命名为 {name}"),
        "remote_files.resize_hint" => (
            "Drag to resize the file manager",
            "拖动以调整文件管理器宽度",
        ),
        "remote_files.select_regular_file" => ("Select a regular file", "请选择普通文件"),
        "remote_files.ssh_disconnected_during_sftp" => (
            "SSH session disconnected while opening SFTP",
            "打开 SFTP 时 SSH 会话已断开",
        ),
        "remote_files.ssh_session_disconnected" => {
            ("SSH session is disconnected", "SSH 会话已断开")
        }
        "remote_files.title" => ("Remote files", "远程文件"),
        "remote_files.unsupported_delete" => {
            ("This entry type cannot be deleted", "无法删除此类型的项目")
        }
        "remote_files.upload" => ("Upload", "上传"),
        "remote_files.upload_cancelled" => ("Upload cancelled", "已取消上传"),
        "remote_files.upload_queued" => ("Queued upload: {name}", "已加入上传队列：{name}"),

        // ── common / keybindings additions ─────────────────────────────
        "common.hide" => ("Hide", "隐藏"),
        "common.show" => ("Show", "显示"),
        "keybindings.disabled" => ("Disabled", "已禁用"),

        // ── settings.* — settings dialog additions ─────────────────────
        "settings.appearance_help" => (
            "Customize the complete outline around the top tab for the focused pane.",
            "自定义聚焦窗格顶部标签页的完整轮廓。",
        ),
        "settings.outline_color" => ("Outline color", "轮廓颜色"),
        "settings.outline_width" => ("Outline width", "轮廓宽度"),
        "settings.corner_radius" => ("Corner radius", "圆角半径"),
        "settings.preview" => ("Preview", "预览"),
        "settings.focused_session" => ("Focused session", "聚焦会话"),
        "settings.skin_help" => (
            "Choose a built-in skin or tune the Custom palette. This changes application chrome only; terminal ANSI and xterm colors remain independent.",
            "选择内置皮肤或调整自定义调色板。此设置只会更改应用界面；终端 ANSI 和 xterm 颜色保持独立。",
        ),
        "settings.theme_mode" => ("Appearance mode", "外观模式"),
        "settings.theme_mode_help" => (
            "Switch between the dark and light variant of the selected skin, or follow the operating system preference.",
            "在所选皮肤的暗色与亮色变体之间切换，或跟随系统外观设置。",
        ),
        "settings.theme_dark" => ("Dark", "暗色"),
        "settings.theme_light" => ("Light", "亮色"),
        "settings.theme_system" => ("System", "随系统"),
        "settings.custom_variant" => ("Editing variant", "编辑变体"),
        "settings.skin_tokyo_night" => ("Tokyo Night", "Tokyo Night"),
        "settings.skin_one_dark" => ("One Dark", "One Dark"),
        "settings.skin_solarized_dark" => ("Solarized Dark", "Solarized Dark"),
        "settings.skin_custom" => ("Custom", "自定义"),
        "settings.skin_preview" => ("Skin preview", "皮肤预览"),
        "settings.skin_preview_connected" => ("Connected", "已连接"),
        "settings.skin_preview_action" => ("Action", "操作"),
        "settings.color_background" => ("Background", "背景"),
        "settings.color_surface" => ("Surface", "表面"),
        "settings.color_surface_hover" => ("Surface hover", "表面悬停"),
        "settings.color_border" => ("Border", "边框"),
        "settings.color_border_strong" => ("Strong border", "强调边框"),
        "settings.color_text" => ("Text", "文本"),
        "settings.color_text_muted" => ("Muted text", "弱化文本"),
        "settings.color_accent" => ("Accent", "强调色"),
        "settings.color_accent_secondary" => ("Secondary accent", "次强调色"),
        "settings.color_success" => ("Success", "成功"),
        "settings.color_warning" => ("Warning", "警告"),
        "settings.color_danger" => ("Danger", "危险"),
        "settings.suggestions_help" => (
            "Inline fish-style suggestions based on your command history.",
            "根据命令历史提供 fish 风格的行内建议。",
        ),
        "settings.on" => ("ON", "开启"),
        "settings.off" => ("OFF", "关闭"),
        "settings.suggestion_count_compact" => (
            "compact popup, minimal screen coverage",
            "紧凑弹窗，占用屏幕最少",
        ),
        "settings.suggestion_count_balanced" => {
            ("balanced view of recent commands", "均衡显示最近的命令")
        }
        "settings.suggestion_count_extensive" => {
            ("extensive history at a glance", "一览更多历史记录")
        }
        "settings.comparison_help" => (
            "Control the warning shown before highlighting a comparison where more than half of the visible rows differ.",
            "控制在高亮超过半数可见行存在差异的比对结果前是否显示警告。",
        ),
        "settings.usage_habits_help" => (
            "Opt in to local command-habit learning. Data stays on this machine in a local DuckDB file unless you explicitly export it below.",
            "选择加入本地命令习惯学习。除非你在下方明确导出，否则数据只会保存在本机的 DuckDB 文件中。",
        ),
        "settings.collected_command_category" => (
            "• Command name + first-token category (git, docker, kubectl, …)",
            "• 命令名称和首个词元类别（git、docker、kubectl 等）",
        ),
        "settings.collected_activity_counts" => (
            "• Success / failure counts and per-hour activity distribution",
            "• 成功/失败次数和每小时活动分布",
        ),
        "settings.collected_corrections" => (
            "• Typo → correction pairs that you accept (e.g. dockre → docker)",
            "• 你接受的拼写错误 → 更正配对（例如 dockre → docker）",
        ),
        "settings.collected_host_count" => (
            "• Number of distinct hosts (hostnames are NOT stored)",
            "• 不同主机的数量（不会存储主机名）",
        ),
        "settings.never_collected_credentials" => (
            "• Passwords, passphrases, private keys, API tokens, bearer tokens",
            "• 密码、口令、私钥、API 令牌、Bearer 令牌",
        ),
        "settings.never_collected_onekey" => (
            "• OneKey credential values or Expect-matched secret responses",
            "• OneKey 凭据值或由 Expect 匹配的机密响应",
        ),
        "settings.never_collected_session_data" => (
            "• Environment variable values, remote command output, session content",
            "• 环境变量值、远程命令输出、会话内容",
        ),
        "settings.never_collected_sensitive_arguments" => (
            "• Full command arguments when a credential flag is detected (the whole line is dropped or the value redacted to ***)",
            "• 检测到凭据参数时的完整命令参数（整行会被丢弃，或将值替换为 ***）",
        ),
        "settings.privacy_sanitizer_help" => (
            "Secret material is filtered by a dedicated sanitizer before anything reaches the local DuckDB store. Turning this off stops all future collection; existing local data is retained until you clear it.",
            "任何内容写入本地 DuckDB 存储前，都会由专用清理器过滤机密材料。关闭此功能将停止后续收集；现有本地数据会保留，直到你将其清除。",
        ),
        "settings.export_report_help" => (
            "Export writes an aggregated, sanitized JSON file (no raw commands, no hostnames, no timestamps beyond the report generation time) to your downloads directory. Upload to GitHub Gist or object storage is a manual next step — paste the token into your uploader of choice; RusTerm never transmits anything automatically.",
            "导出操作会将聚合并清理过的 JSON 文件写入下载目录（不含原始命令、主机名，也不含报告生成时间以外的时间戳）。上传到 GitHub Gist 或对象存储需要手动完成——请将令牌粘贴到你选择的上传工具中；RusTerm 绝不会自动传输任何内容。",
        ),
        "settings.keybindings_help" => (
            "Click a shortcut, then press a new combination. Application shortcuts require Cmd/Ctrl + Shift so standard terminal controls remain available.",
            "点击快捷键，然后按下新的组合键。应用快捷键必须包含 Cmd/Ctrl + Shift，以保留标准终端控制键。",
        ),
        "settings.keybinding_close_focused_pane" => ("Close focused pane", "关闭聚焦窗格"),
        "settings.keybinding_append_pane" => ("Add split pane", "添加分屏窗格"),
        "settings.keybinding_toggle_comparison" => ("Toggle synchronized input", "切换同步输入"),
        "settings.keybinding_toggle_pane_zoom" => ("Toggle pane zoom", "切换窗格缩放"),
        "settings.keybinding_toggle_chat" => ("Toggle agent chat", "切换智能体聊天"),
        "settings.keybinding_press_shortcut" => ("Press shortcut…", "请按快捷键…"),
        "settings.keybinding_disabled" => ("Disabled", "已禁用"),
        "settings.keybinding_error_unsafe" => (
            "Use Cmd/Ctrl + Shift plus a key to keep terminal controls safe.",
            "请使用 Cmd/Ctrl + Shift 加一个按键，以免占用终端控制键。",
        ),
        "settings.keybinding_error_conflict" => {
            ("Already used by {action}.", "已被“{action}”使用。")
        }
        "settings.keybinding_disable" => ("Disable", "禁用"),
        "settings.reset_default" => ("Reset default", "恢复默认设置"),

        // ── suggestion.* — suggestion popup additions ──────────────────
        "suggestion.remove_history_tooltip" => (
            "Remove from history (Shift+Del)",
            "从历史记录中删除（Shift+Del）",
        ),
        "suggestion.remove_history_aria" => ("Remove command from history", "从历史记录中删除命令"),

        // ── master_password.* — credential-store unlock dialog ─────────
        "master_password.create_title" => ("Create Master Password", "创建主密码"),
        "master_password.unlock_title" => ("Unlock RusTerm", "解锁 RusTerm"),
        "master_password.create_subtitle" => (
            "Set a master password to protect your connection credentials.",
            "设置主密码以保护你的连接凭据。",
        ),
        "master_password.unlock_subtitle" => (
            "Enter your master password to decrypt your connections.",
            "输入主密码以解密你的连接。",
        ),
        "master_password.label" => ("Master Password", "主密码"),
        "master_password.enter_placeholder" => ("Enter password", "输入密码"),
        "master_password.toggle_visibility" => ("Show / hide password", "显示/隐藏密码"),
        "master_password.confirm_label" => ("Confirm Password", "确认密码"),
        "master_password.confirm_placeholder" => ("Confirm password", "再次输入密码"),
        "master_password.mismatch" => ("Passwords do not match", "两次输入的密码不一致"),
        "master_password.verifying" => ("Verifying...", "正在验证..."),
        "master_password.create_and_unlock" => ("Create & Unlock", "创建并解锁"),
        "master_password.unlock" => ("Unlock", "解锁"),
        "master_password.recovery_warning" => (
            "Your master password cannot be recovered if lost.\nIt protects all saved connection credentials.",
            "主密码丢失后无法恢复。\n它用于保护所有已保存的连接凭据。",
        ),

        // ── onekey.* — OneKey manager and credential popup ─────────────
        "onekey.manager_title" => (
            "OneKeys (Expect / Send steps)",
            "OneKeys（Expect / Send 步骤）",
        ),
        "onekey.manager_description" => (
            "Each OneKey is a sequence of prompt/Send steps. Choose a built-in prompt type for common Username, Password, sudo, Git, bastion, and SSH key passphrase prompts. Send values are encrypted at rest.",
            "每个 OneKey 都由一系列提示/Send 步骤组成。常见的 Username、Password、sudo、Git、堡垒机和 SSH 密钥口令提示可选择内置提示类型。Send 值在静态存储时会被加密。",
        ),
        "onekey.custom_regex_help" => (
            "Use Custom regex only for unusual prompts. Matching is case-insensitive and runs only for connections with One-Key Connect enabled — set it in the connection's Edit dialog (checkbox right under Name).",
            "仅对特殊提示使用自定义正则表达式。匹配不区分大小写，并且只会对已启用 One-Key Connect 的连接运行——可在连接的编辑对话框中设置（名称正下方的复选框）。",
        ),
        "onekey.hide_send_values" => ("Hide Send values", "隐藏 Send 值"),
        "onekey.show_send_values" => ("Show Send values", "显示 Send 值"),
        "onekey.drag_manager" => ("Drag to move OneKey Manager", "拖动以移动 OneKey 管理器"),
        "onekey.reveal_send_values_tooltip" => (
            "Temporarily reveal Send values so they can be verified",
            "暂时显示 Send 值以便核对",
        ),
        "onekey.untitled" => ("(untitled)", "（未命名）"),
        "onekey.step_count" => ("({count} steps)", "（{count} 个步骤）"),
        "onekey.empty" => (
            "No OneKeys yet.\nClick + to add one.",
            "尚无 OneKey。\n点击 + 添加一个。",
        ),
        "onekey.add" => ("+ Add OneKey", "+ 添加 OneKey"),
        "onekey.name" => ("Name", "名称"),
        "onekey.name_placeholder" => ("ecs-user / git-inesa", "ecs-user / git-inesa"),
        "onekey.steps_label" => ("Expect / Send steps", "Expect / Send 步骤"),
        "onekey.add_step" => ("+ Step", "+ 步骤"),
        "onekey.step_label_placeholder" => ("label (Username)", "标签（用户名）"),
        "onekey.remove_step" => ("Remove step", "删除步骤"),
        "onekey.password_prompt_option" => ("Password prompt (recommended)", "密码提示（推荐）"),
        "onekey.username_prompt_option" => ("Git username prompt", "Git 用户名提示"),
        "onekey.custom_regex_option" => ("Custom regex (advanced)", "自定义正则表达式（高级）"),
        "onekey.custom_expect_placeholder" => ("Custom Expect regex", "自定义 Expect 正则表达式"),
        "onekey.password_prompt_help" => (
            "Matches Password, sudo, Git/bastion password, and SSH key passphrase prompts.",
            "匹配 Password、sudo、Git/堡垒机密码以及 SSH 密钥口令提示。",
        ),
        "onekey.username_prompt_help" => (
            "Matches Git HTTPS username prompts.",
            "匹配 Git HTTPS 用户名提示。",
        ),
        "onekey.send_placeholder" => ("Send (secret — encrypted)", "Send（机密内容 — 已加密）"),
        "onekey.delete" => ("Delete OneKey", "删除 OneKey"),
        "onekey.select_or_add" => (
            "Select a OneKey, or click + Add OneKey to create one.",
            "选择一个 OneKey，或点击 + 添加 OneKey 进行创建。",
        ),
        "onekey.shortcuts" => (
            "Esc Cancel · Ctrl/Cmd+Enter Save",
            "Esc 取消 · Ctrl/Cmd+Enter 保存",
        ),
        "onekey.validation.entry_numbered" => ("OneKey #{number}", "OneKey #{number}"),
        "onekey.validation.entry_named" => ("OneKey ‘{name}’", "OneKey“{name}”"),
        "onekey.validation.name_required" => ("{entry} needs a name.", "{entry} 需要填写名称。"),
        "onekey.validation.steps_required" => (
            "{entry} needs at least one Expect / Send step.",
            "{entry} 至少需要一个 Expect / Send 步骤。",
        ),
        "onekey.validation.step_numbered" => ("step #{number}", "步骤 #{number}"),
        "onekey.validation.step_named" => ("step ‘{label}’", "步骤“{label}”"),
        "onekey.validation.expect_required" => (
            "{entry} {step} needs an Expect regex.",
            "{entry} 的{step}需要填写 Expect 正则表达式。",
        ),
        "onekey.validation.invalid_expect" => (
            "{entry} {step} has an invalid Expect regex: {error}",
            "{entry} 的{step}包含无效的 Expect 正则表达式：{error}",
        ),
        "onekey.validation.send_required" => (
            "{entry} {step} needs a Send value.",
            "{entry} 的{step}需要填写 Send 值。",
        ),
        "onekey.popup.cancel" => ("Cancel credential popup", "取消凭据弹窗"),
        "onekey.popup.cancel_tooltip" => {
            ("Cancel credential popup (Escape)", "取消凭据弹窗（Escape）")
        }
        "onekey.popup.rejected" => (
            "Credential was sent, but the remote requested it again. Verify the saved value.",
            "凭据已发送，但远端再次请求该凭据。请核对已保存的值。",
        ),
        "onekey.popup.use_credential" => (
            "Use {name} · {label} (Enter or Tab)",
            "使用 {name} · {label}（Enter 或 Tab）",
        ),
        "onekey.popup.save" => ("Save In OneKeys", "保存到 OneKeys"),
        "onekey.submission_feedback" => (
            "Credential sent · input hidden by remote",
            "凭据已发送 · 输入已由远端隐藏",
        ),

        // ── relay / session additions ───────────────────────────────────
        "relay.sudo_authorization_unavailable" => (
            "No reusable sudo authorization is available for this host. Run sudo once in its RusTerm session with OneKey enabled, then retry.",
            "此主机没有可复用的 sudo 授权。请在其已启用 OneKey 的 RusTerm 会话中运行一次 sudo，然后重试。",
        ),
        "relay.sudo_authorization_rejected" => (
            "The reusable sudo credential was rejected or sudo policy denied this command. Re-authorize sudo in the target RusTerm session.",
            "可复用的 sudo 凭据被拒绝，或 sudo 策略拒绝了此命令。请在目标 RusTerm 会话中重新授权 sudo。",
        ),
        "relay.sudo_authorization_expired" => (
            "The reusable sudo authorization for this host has expired (the local lease elapses 30 minutes after the last sudo submission or successful API use). Run sudo once more in its RusTerm session with OneKey enabled, then retry.",
            "此主机可复用的 sudo 授权已过期（本地租约在上一次提交 sudo 或成功 API 调用后 30 分钟失效）。请在其已启用 OneKey 的 RusTerm 会话中再次运行一次 sudo，然后重试。",
        ),
        "relay.live_session_required" => (
            "This host sits behind a bastion (login script configured), so the command must run inside the exact logged-in terminal tab — only that tab knows which target node it navigated to. The requested tab is not available (closed, disconnected, or the selector is stale). Keep the tab connected and re-copy the curl command from the API panel.",
            "此主机位于堡垒机之后（已配置登录脚本），命令必须在已登录的那个终端标签页内执行——只有该标签页知道自己导航到了哪个目标节点。请求指定的标签页不可用（已关闭、已断开或选择器已过时）。请保持该标签页在线，并从 API 面板重新复制 curl 命令。",
        ),
        "relay.live_sudo_unavailable" => (
            "Non-interactive sudo (sudo -n) was refused on this bastion node, and passwords are never injected into a live terminal (its echo would leak the secret). Run any sudo command once inside that terminal tab to refresh the sudo timestamp and retry, or uncheck 'Execute with reused sudo authorization' (not needed when logged in as root).",
            "该堡垒机节点上非交互 sudo（sudo -n）被拒绝，而实时终端内绝不注入密码（回显会泄露密码）。请先在该终端标签页内运行一次任意 sudo 命令刷新 sudo 时间戳后重试，或取消勾选“复用 sudo 授权执行”（以 root 登录时无需 sudo）。",
        ),
        "relay.start_timeout_or_runtime_wedged" => (
            "Relay start timed out or runtime is wedged: {error}",
            "中转启动超时或运行时卡死：{error}",
        ),
        "session.serial_open_failed" => ("Serial open failed: {error}", "打开串口失败：{error}"),
        "session.telnet_connect_failed" => {
            ("Telnet connect failed: {error}", "Telnet 连接失败：{error}")
        }

        // ── shadow.* — shadow sandbox dialog ────────────────────────────
        "shadow.unknown" => ("Unknown", "未知"),
        "shadow.execution_title" => ("Shadow sandbox: confirm execution", "影子沙盒：确认执行"),
        "shadow.execution_description" => (
            "The following is only a model suggestion. The model cannot execute commands; only after you click “Confirm execution” will the command be written to the current login session.",
            "以下内容只是模型建议。模型不能执行命令；只有你点击“确认执行”后，命令才会写入当前登录会话。",
        ),
        "shadow.target_session" => ("Target session", "目标会话"),
        "shadow.working_directory" => ("Working directory", "工作目录"),
        "shadow.risk_warning" => ("Risk warning: ", "风险提示："),
        "shadow.execution_warning" => (
            "Verify the arguments, quoting, target host, and working directory yourself. Once confirmed, the command runs in the real session, not an OS-isolated sandbox.",
            "请自行核对参数、引号、目标主机和工作目录。确认后命令会在真实会话中执行，并非 OS 隔离沙盒。",
        ),
        "shadow.confirm_execute" => ("Confirm execution", "确认执行"),
        "shadow.exit_code_unavailable" => ("Unavailable", "未获取"),
        "shadow.output_truncated" => (
            "Output exceeds {limit}; preview and shared content have been truncated.",
            "输出超过 {limit}，预览和共享内容已截断。",
        ),
        "shadow.result_title" => (
            "Shadow sandbox: execution result awaiting authorization",
            "影子沙盒：执行结果待授权",
        ),
        "shadow.result_description" => (
            "The result is currently stored only in temporary local state and has not been added to an LLM request. Preview it, then decide whether to allow it to be sent to the model.",
            "结果目前只保存在本地临时状态，尚未加入 LLM 请求。请预览后决定是否允许发送给模型。",
        ),
        "shadow.exit_code" => ("Exit code", "退出码"),
        "shadow.command" => ("Command", "命令"),
        "shadow.do_not_share" => ("Do not share", "不分享"),
        "shadow.confirm_send_to_model" => ("Confirm send to model", "确认发送给模型"),

        // ── chat.* — agent chat box (issue #122) ───────────────────────
        "chat.title" => ("Agent Chat", "智能体聊天"),
        "chat.empty" => (
            "Ask the agent for a command, or type / to search your history.",
            "向智能体提问，或输入 / 搜索历史命令。",
        ),
        "chat.placeholder" => (
            "Message the agent…  ( / for commands, Tab for terminal )",
            "给智能体发消息…（ / 搜索命令，Tab 回到终端 ）",
        ),
        "chat.hint" => ("Tab / Esc returns to terminal", "Tab / Esc 回到终端"),
        "chat.send" => ("Send", "发送"),
        "chat.run" => ("Run", "运行"),
        "chat.thinking" => ("thinking…", "思考中…"),
        "chat.stub_reply" => (
            "(LLM round-trip not yet wired — configure an agent and API key, then this surface will call rusterm-ai.)",
            "（尚未接入 LLM——请配置智能体与 API Key，之后将调用 rusterm-ai。）",
        ),
        "chat.no_agent" => ("No agent configured", "未配置智能体"),
        "chat.agent_name" => ("Name", "名称"),
        "chat.agent_model" => ("Model", "模型"),
        "chat.agent_base_url" => ("Base URL (optional)", "Base URL（可选）"),
        "chat.agent_api_key" => ("API key (in-memory only)", "API Key（仅存内存）"),
        "chat.agent_system_prompt" => ("System prompt", "系统提示词"),
        "chat.save" => ("Save", "保存"),
        "chat.dock_tooltip" => (
            "Merge into main window (cycles: right dock → bottom dock → floating)",
            "合并到主体窗口（点击切换：右侧停靠 → 底部停靠 → 浮动）",
        ),

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
    use std::collections::BTreeSet;
    use std::path::Path;

    const DYNAMIC_SOURCE_KEYS: &[&str] = &[
        "layout.summary",
        "layout.summary_tab_tiled",
        "settings.keybinding_append_pane",
        "settings.keybinding_close_focused_pane",
        "settings.keybinding_toggle_comparison",
        "settings.keybinding_toggle_pane_zoom",
        "settings.keybinding_toggle_chat",
        "settings.skin_custom",
        "settings.skin_one_dark",
        "settings.skin_solarized_dark",
        "settings.skin_tokyo_night",
        "settings.theme_dark",
        "settings.theme_light",
        "settings.theme_system",
        "relay.account_required_before_start",
        "relay.confirm_public_bind_before_start",
        "relay.password_required",
        "relay.username_required",
        "remote_files.applying_operation",
        "remote_files.choosing_download_destination",
        "remote_files.choosing_local_file",
        "remote_files.download_cancelled",
        "remote_files.file_type_directory",
        "remote_files.file_type_file",
        "remote_files.file_type_other",
        "remote_files.file_type_symlink",
        "remote_files.invalid_utf8_filename",
        "remote_files.name_dot",
        "remote_files.name_empty",
        "remote_files.name_slash",
        "remote_files.path_absolute",
        "remote_files.select_regular_file",
        "remote_files.ssh_disconnected_during_sftp",
        "remote_files.ssh_session_disconnected",
        "remote_files.unsupported_delete",
        "remote_files.upload_cancelled",
        "tunnels.connection_required",
        "tunnels.invalid_listen_address",
        "tunnels.invalid_listen_port",
        "tunnels.invalid_remote_port",
        "tunnels.listen_port_zero",
        "tunnels.name_required",
        "tunnels.remote_host_required",
    ];

    fn catalog_keys() -> BTreeSet<String> {
        include_str!("i18n.rs")
            .lines()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix('"')?
                    .split_once("\" =>")
                    .map(|(key, _)| key.to_string())
            })
            .collect()
    }

    fn collect_rust_sources(directory: &Path, sources: &mut Vec<String>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rust_sources(&path, sources);
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && path.file_name().is_some_and(|name| name != "i18n.rs")
            {
                sources.push(std::fs::read_to_string(path).unwrap());
            }
        }
    }

    fn literal_keys_after(source: &str, marker: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut remaining = source;
        while let Some(index) = remaining.find(marker) {
            remaining = &remaining[index + marker.len()..];
            let argument = remaining.trim_start();
            if let Some(quoted) = argument.strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    keys.push(quoted[..end].to_string());
                }
            }
        }
        keys
    }

    fn source_translation_keys() -> BTreeSet<String> {
        let mut sources = Vec::new();
        collect_rust_sources(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut sources,
        );

        let mut keys = BTreeSet::new();
        for source in sources {
            keys.extend(literal_keys_after(&source, "crate::i18n::t("));
            keys.extend(literal_keys_after(&source, "crate::i18n::tf("));
            keys.extend(literal_keys_after(&source, "crate::i18n::t_for("));
        }
        keys.extend(DYNAMIC_SOURCE_KEYS.iter().map(|key| (*key).to_string()));
        keys
    }

    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut placeholders = BTreeSet::new();
        let mut remaining = text;
        while let Some(open) = remaining.find('{') {
            let after_open = &remaining[open + 1..];
            let Some(close) = after_open.find('}') else {
                break;
            };
            let name = &after_open[..close];
            if !name.is_empty()
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
            {
                placeholders.insert(name.to_string());
            }
            remaining = &after_open[close + 1..];
        }
        placeholders
    }

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
    fn every_source_key_has_non_empty_en_and_zh() {
        for key in source_translation_keys() {
            for language in [Language::En, Language::Zh] {
                let translation = translate(&key, language)
                    .unwrap_or_else(|| panic!("{language:?} translation missing for {key}"));
                assert!(
                    !translation.trim().is_empty(),
                    "{language:?} translation is empty for {key}"
                );
            }
        }
    }

    #[test]
    fn every_catalog_entry_is_bilingual_with_matching_placeholders() {
        let keys = catalog_keys();
        assert!(!keys.is_empty());

        for key in keys {
            let en = translate(&key, Language::En)
                .unwrap_or_else(|| panic!("English translation missing for {key}"));
            let zh = translate(&key, Language::Zh)
                .unwrap_or_else(|| panic!("Chinese translation missing for {key}"));
            assert!(
                !en.trim().is_empty(),
                "English translation is empty for {key}"
            );
            assert!(
                !zh.trim().is_empty(),
                "Chinese translation is empty for {key}"
            );
            assert_eq!(
                placeholders(en),
                placeholders(zh),
                "placeholder mismatch for {key}"
            );
        }
    }

    #[test]
    fn reserved_api_shell_text_meets_contract() {
        let prompt = translate("api.password_prompt", Language::En).unwrap();
        assert!(!prompt.contains('\''));

        for language in [Language::En, Language::Zh] {
            let reference = translate("api.endpoint_reference", language).unwrap();
            assert_eq!(reference.lines().count(), 6);
            assert!(reference.contains("{url}"));
            assert!(reference.contains("/r/{host_id}"));
            assert!(reference.contains("GET  {url}/r"));
            for field in ["host_id", "command", "elevated?", "timeout_ms?"] {
                assert!(reference.contains(field));
            }
        }
    }

    #[test]
    fn language_label_is_in_native_script() {
        assert_eq!(Language::Zh.label(), "中文");
        assert_eq!(Language::En.label(), "English");
    }
}
