//! OTP / MFA code providers for SSH keyboard-interactive authentication.
//!
//! When a bastion such as JumpServer demands a one-time verification code as a
//! second factor, the SSH auth loop calls [`OtpProvider::fetch_code`] to obtain
//! the current code. The provider is selected by [`OtpWebhookConfig`] in the
//! application settings; see `rusterm_core::config` for the schema.
//!
//! Three providers ship out of the box:
//!
//! - [`FeishuBot`](OtpProvider::FeishuBot) reads the most recent matching
//!   message from a Feishu chat via the Open Platform API.
//! - [`Http`](OtpProvider::Http) calls a generic HTTP webhook and extracts the
//!   code with a regex.
//! - [`Manual`](OtpProvider::Manual) performs no fetch — the caller surfaces
//!   the prompt to the user.
//!
//! All network access uses `reqwest` with the same `rustls` backend as the rest
//! of RusTerm. Secrets (`app_secret`, header values) are kept in memory only
//! for the duration of a fetch and are never logged.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rusterm_core::config::OtpWebhookConfig;

/// A source of OTP / MFA verification codes.
///
/// Implementations are expected to be cheap to clone (they hold only config)
/// and safe to call from a `tokio` task. The provider never falls back to
/// manual entry on its own — a `None` return tells the caller to surface the
/// prompt to the user.
#[derive(Debug, Clone)]
pub enum OtpProvider {
    /// No automatic provider configured. [`fetch_code`](OtpProvider::fetch_code)
    /// always returns `Ok(None)` so the caller can show the manual prompt.
    Manual,
    /// Generic HTTP webhook.
    Http {
        url: String,
        method: reqwest::Method,
        body: Option<String>,
        headers: Vec<(String, String)>,
        code_re: Regex,
        timeout: Duration,
    },
    /// Feishu (Lark) bot reading the latest chat message via the Open Platform API.
    Feishubot {
        base_url: String,
        app_id: String,
        app_secret: String,
        chat_id: String,
        sender_open_id: Option<String>,
        code_re: Regex,
        max_age: Duration,
    },
}

impl OtpProvider {
    /// Build a provider from the persisted settings. Returns [`OtpProvider::Manual`]
    /// for `None` or [`OtpWebhookConfig::Manual`] so callers always have a usable
    /// value.
    pub fn from_config(cfg: Option<&OtpWebhookConfig>) -> Self {
        match cfg {
            None | Some(OtpWebhookConfig::Manual) => OtpProvider::Manual,
            Some(OtpWebhookConfig::Http {
                url,
                method,
                body,
                headers,
                code_pattern,
                timeout_secs,
            }) => {
                let method = match method.to_ascii_lowercase().as_str() {
                    "post" => reqwest::Method::POST,
                    _ => reqwest::Method::GET,
                };
                let code_re = Regex::new(code_pattern).unwrap_or_else(|_| {
                    Regex::new(r"\b\d{4,8}\b").expect("default OTP regex is valid")
                });
                OtpProvider::Http {
                    url: url.clone(),
                    method,
                    body: body.clone(),
                    headers: headers.clone(),
                    code_re,
                    timeout: Duration::from_secs(*timeout_secs),
                }
            }
            Some(OtpWebhookConfig::Feishubot {
                app_id,
                app_secret,
                chat_id,
                code_pattern,
                sender_open_id,
                max_age_secs,
                base_url,
            }) => {
                let code_re = Regex::new(code_pattern).unwrap_or_else(|_| {
                    Regex::new(r"\b\d{4,8}\b").expect("default OTP regex is valid")
                });
                OtpProvider::Feishubot {
                    base_url: base_url.clone(),
                    app_id: app_id.clone(),
                    app_secret: app_secret.clone(),
                    chat_id: chat_id.clone(),
                    sender_open_id: sender_open_id.clone(),
                    code_re,
                    max_age: Duration::from_secs(*max_age_secs),
                }
            }
        }
    }

    /// Returns `true` if this provider can fetch a code automatically.
    pub fn is_automatic(&self) -> bool {
        matches!(self, OtpProvider::Http { .. } | OtpProvider::Feishubot { .. })
    }

    /// Fetch the current OTP code. Returns `Ok(None)` when no code could be
    /// obtained automatically (provider is `Manual`, no matching message
    /// found, network error swallowed, etc.). The caller must then fall back
    /// to manual entry.
    pub async fn fetch_code(&self) -> Result<Option<String>> {
        match self {
            OtpProvider::Manual => Ok(None),
            OtpProvider::Http {
                url,
                method,
                body,
                headers,
                code_re,
                timeout,
            } => fetch_http(url, method.clone(), body.as_deref(), headers, code_re, *timeout)
                .await
                .map(Some),
            OtpProvider::Feishubot {
                base_url,
                app_id,
                app_secret,
                chat_id,
                sender_open_id,
                code_re,
                max_age,
            } => {
                fetch_feishu(
                    base_url,
                    app_id,
                    app_secret,
                    chat_id,
                    sender_open_id.as_deref(),
                    code_re,
                    *max_age,
                )
                .await
            }
        }
    }
}

/// Build a `reqwest::Client` with the same `rustls` TLS backend as the rest of
/// RusTerm, accepting the system root store (`webpki-roots`). The client
/// follows redirects (some webhook providers 302 to the real payload) and
/// never sends a default `User-Agent` that identifies RusTerm — webhooks are
/// user-configured endpoints, not third-party APIs.
fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(true)
        .timeout(timeout)
        .build()
        .context("failed to build reqwest client for OTP webhook")
}

async fn fetch_http(
    url: &str,
    method: reqwest::Method,
    body: Option<&str>,
    headers: &[(String, String)],
    code_re: &Regex,
    timeout: Duration,
) -> Result<String> {
    let client = http_client(timeout)?;
    let mut req = client.request(method, url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    if let Some(body) = body {
        req = req.body(body.to_string());
    }
    let resp = req.send().await.context("OTP webhook request failed")?;
    let status = resp.status();
    let text = resp.text().await.context("OTP webhook response decode failed")?;
    if !status.is_success() {
        tracing::warn!(
            "[OTP] webhook {} returned status {} body_len={}",
            url,
            status,
            text.len()
        );
        return Err(anyhow!("OTP webhook returned HTTP {}", status));
    }
    extract_code(&text, code_re).ok_or_else(|| {
        tracing::warn!(
            "[OTP] webhook {} response matched no code (body_len={})",
            url,
            text.len()
        );
        anyhow!("OTP webhook response contained no matching code")
    })
}

/// Extract the first regex match from `text`. The regex is expected to have
/// either no capture group (whole match is the code) or one capture group
/// (group 1 is the code).
fn extract_code(text: &str, code_re: &Regex) -> Option<String> {
    let caps = code_re.captures(text)?;
    if code_re.capture_names().count() > 1 {
        // If group 1 exists, prefer it; otherwise fall back to the whole match.
        Some(caps.get(1).or_else(|| caps.get(0))?.as_str().to_string())
    } else {
        Some(caps.get(0)?.as_str().to_string())
    }
}

// ── Feishu Open Platform API ──────────────────────────────────────────

#[derive(serde::Deserialize)]
struct FeishuTokenResp {
    code: i64,
    msg: String,
    tenant_access_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct FeishuMessageItem {
    message_id: String,
    #[serde(default)]
    body: Option<FeishuMessageBody>,
    #[serde(default)]
    create_time: String,
    #[serde(default)]
    sender: Option<FeishuSender>,
}

#[derive(serde::Deserialize)]
struct FeishuMessageBody {
    content: String,
}

#[derive(serde::Deserialize)]
struct FeishuSender {
    #[serde(default)]
    id: Option<String>,
}

#[derive(serde::Deserialize)]
struct FeishuMessageListResp {
    code: i64,
    msg: String,
    #[serde(default)]
    data: Option<FeishuMessageListData>,
}

#[derive(serde::Deserialize)]
struct FeishuMessageListData {
    #[serde(default)]
    items: Vec<FeishuMessageItem>,
}

/// Fetch a `tenant_access_token` from the Feishu Open Platform.
async fn feishu_tenant_token(base_url: &str, app_id: &str, app_secret: &str) -> Result<String> {
    let client = http_client(Duration::from_secs(10))?;
    let resp: FeishuTokenResp = client
        .post(format!("{}/open-apis/auth/v3/tenant_access_token/internal", base_url))
        .json(&serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret,
        }))
        .send()
        .await
        .context("Feishu token request failed")?
        .json()
        .await
        .context("Feishu token response decode failed")?;
    if resp.code != 0 {
        return Err(anyhow!(
            "Feishu token API error {}: {}",
            resp.code,
            resp.msg
        ));
    }
    resp.tenant_access_token.ok_or_else(|| anyhow!("Feishu token API returned no token"))
}

/// Read the latest messages from `chat_id`, returning the most recent one
/// (after sender + age filtering) that matches `code_re`.
async fn fetch_feishu(
    base_url: &str,
    app_id: &str,
    app_secret: &str,
    chat_id: &str,
    sender_open_id: Option<&str>,
    code_re: &Regex,
    max_age: Duration,
) -> Result<Option<String>> {
    let token = feishu_tenant_token(base_url, app_id, app_secret).await?;
    let client = http_client(Duration::from_secs(10))?;
    // Page size 20 is more than enough — we only want the freshest code and
    // MFA pushes are typically the most recent message in a dedicated bot chat.
    let url = format!(
        "{}/open-apis/im/v1/messages?container_id_type=chat&container_id={}&page_size=20&sort_type=by_create_time_desc",
        base_url, chat_id,
    );
    let resp: FeishuMessageListResp = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .context("Feishu message list request failed")?
        .json()
        .await
        .context("Feishu message list decode failed")?;
    if resp.code != 0 {
        return Err(anyhow!(
            "Feishu message list API error {}: {}",
            resp.code,
            resp.msg
        ));
    }
    let items = resp.data.map(|d| d.items).unwrap_or_default();
    let now = chrono::Utc::now();
    let max_age_secs = max_age.as_secs() as i64;
    for item in items {
        // Sender filter — skip messages from other users when configured.
        if let Some(want) = sender_open_id {
            let got = item.sender.as_ref().and_then(|s| s.id.as_deref());
            if got != Some(want) {
                continue;
            }
        }
        // Age filter — parse `create_time` (millisecond epoch string from Feishu).
        let created = item
            .create_time
            .parse::<i64>()
            .ok()
            .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms));
        if let Some(ts) = created {
            let age = now.signed_duration_since(ts);
            if age.num_seconds() > max_age_secs {
                // Sorted descending, so everything after is older — stop.
                tracing::debug!(
                    "[OTP] feishu message {} is {}s old (> {}s); stopping",
                    item.message_id,
                    age.num_seconds(),
                    max_age_secs
                );
                break;
            }
        }
        // Message body is a JSON string like `{"text":"Your code: 123456"}`.
        let content = item.body.map(|b| b.content).unwrap_or_default();
        if let Some(code) = extract_code(&content, code_re) {
            tracing::info!(
                "[OTP] feishu matched code from message {} (len={})",
                item.message_id,
                content.len()
            );
            return Ok(Some(code));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_code_with_no_group_uses_whole_match() {
        let re = Regex::new(r"\b\d{6}\b").unwrap();
        assert_eq!(extract_code("Your code: 123456 expires soon", &re).as_deref(), Some("123456"));
    }

    #[test]
    fn extract_code_with_capture_group_uses_group_1() {
        let re = Regex::new(r"code[:\s]+(\d{6})").unwrap();
        assert_eq!(extract_code("MFA code: 654321 ok", &re).as_deref(), Some("654321"));
    }

    #[test]
    fn extract_code_returns_none_when_no_match() {
        let re = Regex::new(r"\d{6}").unwrap();
        assert!(extract_code("no digits here", &re).is_none());
    }

    #[test]
    fn provider_from_config_manual_returns_manual() {
        assert!(matches!(OtpProvider::from_config(None), OtpProvider::Manual));
        assert!(matches!(
            OtpProvider::from_config(Some(&OtpWebhookConfig::Manual)),
            OtpProvider::Manual
        ));
    }

    #[test]
    fn provider_from_config_http_parses_method_and_regex() {
        let cfg = OtpWebhookConfig::Http {
            url: "https://example.local/code".to_string(),
            method: "POST".to_string(),
            body: Some("{}".to_string()),
            headers: vec![("X-Test".to_string(), "1".to_string())],
            code_pattern: r"(\d{4})".to_string(),
            timeout_secs: 5,
        };
        match OtpProvider::from_config(Some(&cfg)) {
            OtpProvider::Http { method, timeout, .. } => {
                assert_eq!(method, reqwest::Method::POST);
                assert_eq!(timeout, Duration::from_secs(5));
            }
            other => panic!("expected Http, got {:?}", other),
        }
    }

    #[test]
    fn provider_is_automatic_flag() {
        assert!(!OtpProvider::Manual.is_automatic());
        let http = OtpProvider::Http {
            url: "x".into(),
            method: reqwest::Method::GET,
            body: None,
            headers: vec![],
            code_re: Regex::new(r"\d+").unwrap(),
            timeout: Duration::from_secs(1),
        };
        assert!(http.is_automatic());
    }

    #[tokio::test]
    async fn manual_fetch_returns_none() {
        assert!(OtpProvider::Manual.fetch_code().await.unwrap().is_none());
    }
}
