# OTP / MFA webhook 二次认证配置面（任务 #129, 2026-08-05）

## 背景
- #121 已经实现 SSH 键盘交互中检测 OTP / MFA 提示并通过
  `OtpWebhookConfig` / `OtpProvider`（`crates/rusterm-core/src/config.rs`、
  `crates/rusterm-ssh/src/otp.rs`）自动取码并提交。
- 落地 sidecar：`config_manager.rs` 已有 `load_otp_webhook` /
  `save_otp_webhook(Map)`（写整个 `PersistedConfig`）。SSH 连接入口
  (`start_ssh_connection`) 已在连接前读取并调用
  `SshClient::with_otp_provider`。Provider 支持 `manual` / `feishubot` /
  `http` 三种形式；其中 `feishubot` 通过 Feishu Open Platform IM API
  轮询 chat_id 最新消息并用regex 取 OTP。
- `OtpWebhookConfig` 的变化：`Manual` 变体现在是写盘时转成 `None` ——配置面板默认
  折叠为「手动（仅提示）」即不落盘；安全默认。

## 需要对接但后端没接到 UI 的缺 nenne 漏
- Settings 对话框只有 suggestion / comparison / usage_habits / local-ai
  / keybindings / skin，没有 OTP webhook 一节。用户报任务 #129：「需要一个
  webhook 访问飞书聊天工具，基于社区文章 article/7271149634339422210，
  复用已有的——从聊天消息中截取 OTP」。

## 补上的
- `crates/rusterm-ui/src/components/settings_dialog.rs`
  - 新添 props `otp_webhook: Option<OtpWebhookConfig>` +
    `on_save_otp_webhook: EventHandler<Option<OtpWebhookConfig>>`（其他
    props 类型都要 `Default`/`Option`）+ 本地 signal `otp_webhook_setting`。
  - 新添纯函数 `render_otp_webhook_settings(Signal<Option<OtpWebhookConfig>>) -> Element`
    格式化了 provider 选择（ manual → None 、 feishubot / http 默认表单）。手
    动时什么都不存； Mehr 智能钱不暗示。
  - `otp_default_feishubot()` / `otp_default_http()` 辅助函数，
    `OtpWebhookConfig::Http` / `Feishubot` 各字段的 `oninput`/`onchange` 修改器；
    `max_age_secs` / `timeout_secs` 用 `r#type: "number"` + `parse::<u64>`、未通过不更新。
  - 占位 `placeholder: "\\\\b\\\\d{{4,8}}\\\\b"`——Dioxus rsx 把 `{...}` 当格式化，
    `{{` 转义为 `{`。
  - **Reset default** 清空 OTP webhook。
  - **Save** 按钮除原有回调外额外 `on_save_otp_webhook.call(otp_webhook_setting())`。
- `render_otp_webhook_settings` 调用位置：Usage habits 之后 / Local AI 之前。
- `crates/rusterm-ui/src/app.rs` （SettingsDialog 使用处）
  - 导入 `OtpWebhookConfig`；
  - 添加 props：`otp_webhook: state.read().config_manager … .and_then(|cm| cm.load_otp_webhook())`
  - On save handler：`cm.save_otp_webhook(cfg.as_ref())` + info 级
    `[OTP] webhook provider set to {:?}` 只 log 类型判别器，不 log 值（凭据）。
- `crates/rusterm-ui/src/i18n.rs`：新增
  - `settings.otp_webhook`、`settings.otp_webhook_help`（引用社区文章 URL）、
    `settings.otp_provider*`、`settings.otp_feishu_*`、`settings.otp_http_*`、
    `settings.otp_code_pattern`、`settings.otp_max_age_secs`、`settings.otp_timeout_secs`。
  - 包括搜索定义条目：`("settings-otp-webhook", "settings.otp_webhook", "settings.otp_webhook",
      Some("settings.otp_webhook_help"), "otp mfa 2fa … feishu webhook …")`。

## 测试
- `cargo check / test -p rusterm-ui`：789/789 绿。
- `cargo test -p rusterm-ssh --lib`：133/133（沙箱外、transport tests 绑 loopback
  正常）。
- `cargo test -p rusterm-core --lib`：229/229（沙箱内 `~/Library/Application Support/rusterm/session_logs`
  写权限）。

## 尚未接线（可选）
- Settings 面板仍未提供 **test fetch**（点击后调用 provider 取码看返回是否成功）。
  需要把这种「试取」逻辑加到 `otp.rs`，作为可选后续。
- 用飞书「自定义机器人」（`bot/v2/hook`）的 send-only API 无法实现「读
  chat 消息」，只能去 open platform / bot 会话接口——现有
  `Feishubot` provider已经在做这一点。社区 article 讲的是一个「发消息」
  webhook bot；端到端用该 bot 做 OTP 投递方向与人实现的方向相反，请防混。
