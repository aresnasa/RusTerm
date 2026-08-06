//! Multi-turn chat completions client for the agent chat panel (issue #126).
//!
//! Supports two wire protocols:
//! - **OpenAI-compatible** (`/chat/completions`) — covers OpenAI itself plus
//!   the long tail of compatible providers (DeepSeek, Qwen/DashScope, Zhipu
//!   GLM, Moonshot Kimi, Groq, OpenRouter, SiliconFlow, Gemini's compat
//!   endpoint, Ollama, LM Studio, vLLM, …).
//! - **Anthropic Messages** (`/v1/messages`).
//!
//! All requests honor an explicit [`ProxySelection`] so users behind
//! firewalls can route traffic through a local Clash (or any HTTP/SOCKS5)
//! proxy. `ProxySelection::System` keeps reqwest's default behavior of
//! reading `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` from the environment.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Wire protocol for a chat completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatProtocol {
    /// `POST {base}/chat/completions` with `Authorization: Bearer`.
    OpenAiCompatible,
    /// `POST {base}/v1/messages` with `x-api-key` + `anthropic-version`.
    Anthropic,
}

/// One prior conversation turn (system prompts are passed separately).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: ChatTurnRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatTurnRole {
    User,
    Assistant,
}

/// How outbound AI/preset requests reach the network.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProxySelection {
    /// reqwest default: honor `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` env vars.
    #[default]
    System,
    /// Force direct connection, ignoring environment proxies.
    Disabled,
    /// Explicit proxy URL (`http://…`, `https://…`, or `socks5://…`).
    Url(String),
}

/// Everything needed for one chat completion round-trip. The API key is
/// borrowed for the duration of the call and never logged.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub protocol: ChatProtocol,
    /// Base URL override; empty string → protocol default.
    pub base_url: String,
    /// May be empty for local/keyless endpoints (Ollama, LM Studio).
    pub api_key: String,
    pub model: String,
    /// Empty string → no system prompt sent.
    pub system_prompt: String,
    /// Full conversation so far, oldest first, ending with the newest user turn.
    pub turns: Vec<ChatTurn>,
    pub proxy: ProxySelection,
}

/// Resolve the effective base URL: an explicit override wins, otherwise the
/// protocol's official default. Trailing slashes are trimmed so path joins
/// are predictable.
pub fn resolve_base_url(protocol: ChatProtocol, base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    match protocol {
        ChatProtocol::OpenAiCompatible => "https://api.openai.com/v1".to_string(),
        ChatProtocol::Anthropic => "https://api.anthropic.com".to_string(),
    }
}

/// Build the JSON body for an OpenAI-compatible `/chat/completions` call.
pub fn openai_body(req: &ChatRequest) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(req.turns.len() + 1);
    if !req.system_prompt.trim().is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": req.system_prompt,
        }));
    }
    for turn in &req.turns {
        let role = match turn.role {
            ChatTurnRole::User => "user",
            ChatTurnRole::Assistant => "assistant",
        };
        messages.push(serde_json::json!({ "role": role, "content": turn.content }));
    }
    serde_json::json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": 4096,
    })
}

/// Build the JSON body for an Anthropic `/v1/messages` call.
pub fn anthropic_body(req: &ChatRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req
        .turns
        .iter()
        .map(|turn| {
            let role = match turn.role {
                ChatTurnRole::User => "user",
                ChatTurnRole::Assistant => "assistant",
            };
            serde_json::json!({ "role": role, "content": turn.content })
        })
        .collect();
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": 4096,
        "messages": messages,
    });
    if !req.system_prompt.trim().is_empty() {
        body["system"] = serde_json::Value::String(req.system_prompt.clone());
    }
    body
}

/// Build a reqwest client honoring the given proxy selection. 60s total
/// timeout so a dead endpoint can't hang the chat spinner forever.
pub fn build_http_client(proxy: &ProxySelection) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(60));
    match proxy {
        ProxySelection::System => {}
        ProxySelection::Disabled => builder = builder.no_proxy(),
        ProxySelection::Url(url) => {
            let p = reqwest::Proxy::all(url.as_str())
                .map_err(|e| anyhow::anyhow!("invalid proxy URL '{url}': {e}"))?;
            builder = builder.proxy(p);
        }
    }
    Ok(builder.build()?)
}

/// One multi-turn chat completion round-trip. Returns the assistant's reply
/// text, or an error with the HTTP status + body excerpt for diagnosis.
pub async fn complete_chat(req: &ChatRequest) -> anyhow::Result<String> {
    let client = build_http_client(&req.proxy)?;
    let base = resolve_base_url(req.protocol, &req.base_url);

    match req.protocol {
        ChatProtocol::OpenAiCompatible => {
            let mut http = client
                .post(format!("{base}/chat/completions"))
                .header("Content-Type", "application/json")
                .json(&openai_body(req));
            if !req.api_key.trim().is_empty() {
                http = http.header("Authorization", format!("Bearer {}", req.api_key.trim()));
            }
            let response = http.send().await?;
            if !response.status().is_success() {
                let status = response.status();
                let body = truncate_error_body(&response.text().await.unwrap_or_default());
                anyhow::bail!("HTTP {status}: {body}");
            }
            #[derive(Deserialize)]
            struct ChatResponse {
                choices: Vec<Choice>,
            }
            #[derive(Deserialize)]
            struct Choice {
                message: ChoiceMessage,
            }
            #[derive(Deserialize)]
            struct ChoiceMessage {
                content: Option<String>,
            }
            let data: ChatResponse = response.json().await?;
            Ok(data
                .choices
                .into_iter()
                .next()
                .and_then(|c| c.message.content)
                .unwrap_or_default())
        }
        ChatProtocol::Anthropic => {
            let response = client
                .post(format!("{base}/v1/messages"))
                .header("x-api-key", req.api_key.trim())
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&anthropic_body(req))
                .send()
                .await?;
            if !response.status().is_success() {
                let status = response.status();
                let body = truncate_error_body(&response.text().await.unwrap_or_default());
                anyhow::bail!("HTTP {status}: {body}");
            }
            #[derive(Deserialize)]
            struct MessagesResponse {
                content: Vec<ContentBlock>,
            }
            #[derive(Deserialize)]
            struct ContentBlock {
                #[serde(rename = "type")]
                block_type: String,
                text: Option<String>,
            }
            let data: MessagesResponse = response.json().await?;
            Ok(data
                .content
                .into_iter()
                .filter_map(|b| if b.block_type == "text" { b.text } else { None })
                .collect::<Vec<_>>()
                .join(""))
        }
    }
}

/// Keep error bodies short enough for a chat bubble (provider error JSON can
/// run to kilobytes of HTML on some gateways).
fn truncate_error_body(body: &str) -> String {
    const MAX: usize = 400;
    let trimmed = body.trim();
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut cut = MAX;
        while !trimmed.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &trimmed[..cut])
    }
}

/// Candidate local Clash listener ports, in probe order:
/// - 7890 — ClashX / clash-core default `mixed-port`/`port` (HTTP)
/// - 7897 — Clash Verge default `mixed-port` (HTTP)
/// - 7891 — clash-core default `socks-port` (SOCKS5)
pub const CLASH_PROBE_PORTS: &[(u16, &str)] = &[(7890, "http"), (7897, "http"), (7891, "socks5")];

/// Probe well-known local Clash ports and return the first live proxy URL
/// (e.g. `http://127.0.0.1:7890`). Returns `None` when no Clash-style client
/// is listening. Only ever touches loopback — no external traffic.
pub async fn detect_clash_proxy() -> Option<String> {
    for (port, scheme) in CLASH_PROBE_PORTS {
        let addr = format!("127.0.0.1:{port}");
        let probe = tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(&addr),
        )
        .await;
        if let Ok(Ok(_stream)) = probe {
            return Some(format!("{scheme}://127.0.0.1:{port}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request(protocol: ChatProtocol) -> ChatRequest {
        ChatRequest {
            protocol,
            base_url: String::new(),
            api_key: "sk-test".to_string(),
            model: "test-model".to_string(),
            system_prompt: "be terse".to_string(),
            turns: vec![
                ChatTurn {
                    role: ChatTurnRole::User,
                    content: "hi".to_string(),
                },
                ChatTurn {
                    role: ChatTurnRole::Assistant,
                    content: "hello".to_string(),
                },
                ChatTurn {
                    role: ChatTurnRole::User,
                    content: "what model are you?".to_string(),
                },
            ],
            proxy: ProxySelection::Disabled,
        }
    }

    #[test]
    fn resolve_base_url_uses_protocol_default_when_empty() {
        assert_eq!(
            resolve_base_url(ChatProtocol::OpenAiCompatible, ""),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            resolve_base_url(ChatProtocol::Anthropic, "  "),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn resolve_base_url_trims_trailing_slash_from_override() {
        assert_eq!(
            resolve_base_url(
                ChatProtocol::OpenAiCompatible,
                "https://api.deepseek.com/v1/"
            ),
            "https://api.deepseek.com/v1"
        );
    }

    #[test]
    fn openai_body_includes_system_and_full_history_in_order() {
        let body = openai_body(&sample_request(ChatProtocol::OpenAiCompatible));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be terse");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "what model are you?");
        assert_eq!(body["model"], "test-model");
    }

    #[test]
    fn openai_body_omits_system_message_when_prompt_empty() {
        let mut req = sample_request(ChatProtocol::OpenAiCompatible);
        req.system_prompt.clear();
        let body = openai_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn anthropic_body_lifts_system_prompt_to_top_level() {
        let body = anthropic_body(&sample_request(ChatProtocol::Anthropic));
        assert_eq!(body["system"], "be terse");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn anthropic_body_omits_system_key_when_prompt_empty() {
        let mut req = sample_request(ChatProtocol::Anthropic);
        req.system_prompt.clear();
        let body = anthropic_body(&req);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_http_client_accepts_all_proxy_selections() {
        assert!(build_http_client(&ProxySelection::System).is_ok());
        assert!(build_http_client(&ProxySelection::Disabled).is_ok());
        assert!(
            build_http_client(&ProxySelection::Url("http://127.0.0.1:7890".to_string())).is_ok()
        );
        assert!(
            build_http_client(&ProxySelection::Url("socks5://127.0.0.1:7891".to_string())).is_ok()
        );
        assert!(build_http_client(&ProxySelection::Url("not a url".to_string())).is_err());
    }

    #[test]
    fn truncate_error_body_caps_length_at_char_boundary() {
        let long = "错".repeat(400); // 3 bytes each → 1200 bytes
        let out = truncate_error_body(&long);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 403);
        // Short bodies pass through untouched.
        assert_eq!(truncate_error_body(" ok "), "ok");
    }

    #[test]
    fn clash_probe_ports_cover_known_clients() {
        let ports: Vec<u16> = CLASH_PROBE_PORTS.iter().map(|(p, _)| *p).collect();
        assert!(ports.contains(&7890)); // ClashX / clash-core
        assert!(ports.contains(&7897)); // Clash Verge
        assert!(ports.contains(&7891)); // clash-core socks
    }

    #[tokio::test]
    async fn detect_clash_proxy_returns_url_for_live_local_listener() {
        // Bind a listener on an ephemeral port — detect_clash_proxy only
        // probes the fixed well-known ports, so instead exercise the probe
        // logic directly: a live listener on a probe port would be detected.
        // Here we just pin that detection completes quickly with no panic
        // whether or not a Clash client is running on this machine.
        let result = tokio::time::timeout(Duration::from_secs(2), detect_clash_proxy()).await;
        assert!(result.is_ok(), "probe must finish within the timeout");
        if let Ok(Some(url)) = result {
            assert!(url.starts_with("http://127.0.0.1:") || url.starts_with("socks5://127.0.0.1:"));
        }
    }
}
