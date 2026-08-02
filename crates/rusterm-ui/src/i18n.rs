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
            "The copied script exports the configured username and asks for the API password once per shell. It never places the password in shell history.",
            "复制的脚本会导出已配置的用户名，并在每个 shell 中仅询问一次 API 密码；密码不会写入 shell 历史。",
        ),
        "api.command" => ("Command", "命令"),
        "api.command_edit_hint" => (
            "↓ Edit the remote command here. The curl script updates automatically.",
            "↓ 在这里输入要执行的远程命令；curl 脚本会自动更新。",
        ),
        "api.elevated" => ("Run with reusable sudo authorization", "复用 sudo 授权执行"),
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
        "session.shell_failed" => ("Failed to start shell: {error}", "启动 shell 失败：{error}"),
        "session.starting_shell" => ("Starting local shell…", "正在启动本地终端…"),

        // ── sessions.* — sessions panel ────────────────────────────────
        "sessions.empty_pane" => ("Empty pane", "空白窗格"),
        "sessions.no_open_workspaces" => ("No open workspaces", "没有打开的工作区"),
        "sessions.open_count" => ("{count} open session(s)", "{count} 个打开的会话"),
        "sessions.pane_label" => ("Pane {index}", "窗格 {index}"),
        "sessions.select" => ("Select session {name}", "选择会话 {name}"),
        "sessions.status_connected" => ("Connected", "已连接"),
        "sessions.status_disconnected" => ("Disconnected", "已断开"),
        "sessions.status_reconnecting" => ("Reconnecting", "正在重新连接"),
        "sessions.title" => ("Sessions", "会话"),
        "sessions.workspace_label" => ("Workspace {index}: {label}", "工作区 {index}：{label}"),

        // ── shell.* — local-shell session names ────────────────────────
        "shell.bottom_session_name" => ("Bottom shell", "底部终端"),
        "shell.local_session_name" => ("Local shell", "本地终端"),

        // ── status.* — status bar controls ─────────────────────────────
        "status.ai" => ("AI", "AI"),
        "status.bottom" => ("Bottom", "底部"),
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
            "GET  {url}/api/v1/health      # liveness, no auth\nGET  {url}/api/v1/hosts        # list hosts (BasicAuth)\nPOST {url}/api/v1/exec         # { host_id, command, elevated?, timeout_ms? }\nPOST {url}/api/v1/parse-curl   # parse a pasted curl into JSON",
            "GET  {url}/api/v1/health      # 存活检查，无需鉴权\nGET  {url}/api/v1/hosts        # 列出主机（BasicAuth）\nPOST {url}/api/v1/exec         # { host_id, command, elevated?, timeout_ms? }\nPOST {url}/api/v1/parse-curl   # 将粘贴的 curl 解析为 JSON",
        ),
        "api.password_prompt" => ("printf \"API password: \"", "printf \"API 密码：\""),
        "api.command_marker_title" => ("Remote command", "远程命令"),
        "api.command_marker_help" => (
            "Edit the command between the markers; the curl script updates automatically.",
            "编辑标记之间的命令；curl 脚本会自动更新。",
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
    use std::collections::BTreeSet;
    use std::path::Path;

    const DYNAMIC_SOURCE_KEYS: &[&str] = &[
        "layout.summary",
        "layout.summary_tab_tiled",
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
            if let Some(quoted) = argument.strip_prefix('"')
                && let Some(end) = quoted.find('"')
            {
                keys.push(quoted[..end].to_string());
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
            assert_eq!(reference.lines().count(), 4);
            assert!(reference.contains("{url}"));
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
