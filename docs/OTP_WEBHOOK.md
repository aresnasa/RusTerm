# OTP / MFA Webhook 自动填充

JumpServer（及其他堡垒机）开启二次验证后，SSH 登录会在 keyboard-interactive
认证阶段要求输入一次性验证码（OTP / MFA）。RusTerm 可以通过可配置的 webhook
自动获取该验证码，避免手动输入。

## 工作流程

1. RusTerm 与堡垒机建立 SSH 连接，先用密码完成第一因素认证。
2. 当服务端通过 keyboard-interactive 下发 OTP 提示（匹配 `otp`、`mfa`、
   `验证码`、`动态密码` 等关键字，详见 `looks_like_otp_prompt`），认证循环
   调用已配置的 [`OtpProvider::fetch_code`]。
3. Provider 返回验证码 → 自动提交；Provider 返回 `None`（未配置或取码失败）→
   回退到旧逻辑：把密码当作答案发出让服务端显式拒绝，同时由 UI 的 OneKey
   凭据弹窗接手，让用户手动输入。

## settings.json 配置

`otp_webhook` 是 `PersistedConfig` 中的可选字段。`null` / 缺省 / `Manual`
都表示“不自动取码”，完全等价于本功能上线前的行为。

### Feishu 机器人（推荐）

从飞书开放平台自建应用读取指定会话里最新一条匹配消息：

```json
{
  "otp_webhook": {
    "kind": "feishubot",
    "app_id": "cli_xxxxxxxxxx",
    "app_secret": "yyyyyyyyyyyyyyyy",
    "chat_id": "oc_zzzzzzzzzzzz",
    "code_pattern": "\\b\\d{4,8}\\b",
    "sender_open_id": null,
    "max_age_secs": 120,
    "base_url": "https://open.feishu.cn"
  }
}
```

字段说明：

| 字段              | 默认值                       | 说明                                                            |
| ----------------- | ---------------------------- | --------------------------------------------------------------- |
| `app_id`          | —                            | 飞书自建应用 App ID（`cli_` 开头）                              |
| `app_secret`      | —                            | 飞书自建应用 App Secret                                         |
| `chat_id`         | —                            | 接收 MFA 推送消息的会话 ID（`oc_` 开头），机器人需被加入该会话   |
| `code_pattern`    | `\b\d{4,8}\b`                | 从消息文本中提取验证码的正则，可带一个捕获组                     |
| `sender_open_id`  | `null`                       | 可选：仅接受来自该 open_id（`ou_` 开头）的消息，过滤无关消息     |
| `max_age_secs`    | `120`                        | 验证码最大年龄（秒），超过则忽略，避免复用上次登录的旧码          |
| `base_url`        | `https://open.feishu.cn`     | 飞书 API 域名，国际版 Lark 改为 `https://open.larksuite.com`    |

获取流程：调 `/auth/v3/tenant_access_token/internal` 拿 tenant_access_token
→ 调 `/im/v1/messages?container_id={chat_id}` 拉取最近 20 条消息（按时间倒序）
→ 跳过超过 `max_age_secs` 的、`sender_open_id` 不符的 → 用 `code_pattern`
提取第一个匹配。

### 通用 HTTP webhook

适合自建 TOTP 中转、内部 API 等任意能返回当前验证码的 HTTP 端点：

```json
{
  "otp_webhook": {
    "kind": "http",
    "url": "https://totp.internal.example/current",
    "method": "get",
    "body": null,
    "headers": [["Authorization", "Bearer abc123"]],
    "code_pattern": "\\b\\d{6}\\b",
    "timeout_secs": 10
  }
}
```

响应体按纯文本处理，用 `code_pattern` 提取第一个匹配。响应可以是纯文本
（如 `123456`）也可以是 JSON（正则会从整个响应字符串里抓取）。

### 手动模式（默认）

```json
{ "otp_webhook": { "kind": "manual" } }
```

或直接省略 `otp_webhook` 字段。此时 OTP 提示仍会触发 OneKey 凭据弹窗，
用户可以手动输入验证码。

## 安全说明

- `app_secret` 与 HTTP webhook 的 `headers` 当前以**明文**存于 settings.json。
  `PersistedConfig` 整体由主密码 + AEAD 加密落盘，磁盘上并非明文；但运行期
  内存中持有这些凭据。后续可像 `PersistedSshAuth::Password` 一样改用
  `EncryptedValue` 字段级加密。
- 所有 webhook 请求复用项目 `reqwest` + `rustls` 栈，使用系统根证书校验。
- 验证码仅在认证循环内存活，不会被写入日志或会话回放记录。

## 运行期 API

- `rusterm_core::config::OtpWebhookConfig` — 配置 schema（tagged enum）。
- `rusterm_core::ConfigManager::load_otp_webhook` / `save_otp_webhook` —
  读写持久化字段。
- `rusterm_ssh::OtpProvider::from_config` — 把 `Option<&OtpWebhookConfig>`
  转成运行期 provider。
- `rusterm_ssh::SshClient::with_otp_provider` / `set_otp_provider` —
  把 provider 注入 SSH 客户端；`start_ssh_connection` 会在每次连接前从
  `ConfigManager` 读取最新配置并注入。

## 扩展新 Provider

在 `OtpWebhookConfig` 增加一个 tagged 变体，并在 `OtpProvider::from_config`
与 `OtpProvider::fetch_code` 中各加一个 match 分支即可。Provider 应当：

- 仅持有配置（cheap to clone）；
- 网络调用使用 `http_client` helper，复用 `rustls` 栈；
- 返回 `Ok(None)` 表示“这次没取到”，由认证循环回退到密码或手动输入。
