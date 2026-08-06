//! Official provider-configuration presets for the agent chat panel
//! (issue #126).
//!
//! Ships a curated built-in catalog covering the mainstream closed-source
//! providers (OpenAI, Anthropic, Gemini, Kimi, xAI) and open-source /
//! open-weight ecosystems (DeepSeek, Qwen, GLM, Groq, SiliconFlow,
//! OpenRouter, plus keyless local runtimes: Ollama, LM Studio). The same
//! JSON schema can be fetched from the project's GitHub repository so the
//! list stays current between releases — but ONLY after the user explicitly
//! opts in to network access (`ChatSettings::allow_remote_presets`).

use serde::{Deserialize, Serialize};

use crate::chat::{ProxySelection, build_http_client};

/// Where the refreshed preset catalog is fetched from once the user grants
/// network consent. Raw JSON in the project repository — auditable and
/// versioned alongside the code that consumes it.
pub const REMOTE_PRESETS_URL: &str =
    "https://raw.githubusercontent.com/aresnasa/RusTerm/main/assets/llm-presets.json";

/// One recommended provider configuration. Non-secret by construction: the
/// preset carries everything EXCEPT the API key, which the user supplies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPreset {
    /// Stable id (used to dedup built-in vs. remote entries).
    pub id: String,
    /// Display name, e.g. "DeepSeek".
    pub name: String,
    /// Wire protocol: "openai" (OpenAI-compatible) or "anthropic".
    pub protocol: String,
    /// Chat-completions base URL.
    pub base_url: String,
    /// Recommended default model id.
    pub model: String,
    /// Whether an API key is required (local runtimes don't need one).
    #[serde(default = "default_true")]
    pub requires_key: bool,
    /// `true` for open-source / open-weight model families.
    #[serde(default)]
    pub open_source: bool,
    /// Where to obtain an API key (shown as a hint, never fetched).
    #[serde(default)]
    pub key_url: String,
}

fn default_true() -> bool {
    true
}

/// Wrapper for the remote catalog file so it can grow metadata later
/// without breaking old clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetCatalog {
    #[serde(default)]
    pub version: u32,
    pub presets: Vec<ProviderPreset>,
}

macro_rules! preset {
    ($id:expr, $name:expr, $protocol:expr, $base:expr, $model:expr, key: $key:expr, oss: $oss:expr, url: $url:expr) => {
        ProviderPreset {
            id: $id.to_string(),
            name: $name.to_string(),
            protocol: $protocol.to_string(),
            base_url: $base.to_string(),
            model: $model.to_string(),
            requires_key: $key,
            open_source: $oss,
            key_url: $url.to_string(),
        }
    };
}

/// The built-in catalog. Order matters: closed-source majors first, then
/// open-source cloud providers, then keyless local runtimes.
pub fn builtin_presets() -> Vec<ProviderPreset> {
    vec![
        preset!("openai", "OpenAI (GPT)", "openai", "https://api.openai.com/v1", "gpt-4o-mini",
            key: true, oss: false, url: "https://platform.openai.com/api-keys"),
        preset!("anthropic", "Anthropic (Claude)", "anthropic", "https://api.anthropic.com", "claude-sonnet-4-20250514",
            key: true, oss: false, url: "https://console.anthropic.com/settings/keys"),
        preset!("gemini", "Google Gemini", "openai", "https://generativelanguage.googleapis.com/v1beta/openai", "gemini-2.0-flash",
            key: true, oss: false, url: "https://aistudio.google.com/apikey"),
        preset!("kimi", "Moonshot Kimi", "openai", "https://api.moonshot.cn/v1", "moonshot-v1-8k",
            key: true, oss: false, url: "https://platform.moonshot.cn/console/api-keys"),
        preset!("xai", "xAI (Grok)", "openai", "https://api.x.ai/v1", "grok-3-mini",
            key: true, oss: false, url: "https://console.x.ai"),
        preset!("deepseek", "DeepSeek", "openai", "https://api.deepseek.com/v1", "deepseek-chat",
            key: true, oss: true, url: "https://platform.deepseek.com/api_keys"),
        preset!("qwen", "Qwen (阿里云百炼)", "openai", "https://dashscope.aliyuncs.com/compatible-mode/v1", "qwen-plus",
            key: true, oss: true, url: "https://bailian.console.aliyun.com/?apiKey=1"),
        preset!("glm", "Zhipu GLM (智谱)", "openai", "https://open.bigmodel.cn/api/paas/v4", "glm-4-flash",
            key: true, oss: true, url: "https://open.bigmodel.cn/usercenter/apikeys"),
        preset!("siliconflow", "SiliconFlow (硅基流动)", "openai", "https://api.siliconflow.cn/v1", "deepseek-ai/DeepSeek-V3",
            key: true, oss: true, url: "https://cloud.siliconflow.cn/account/ak"),
        preset!("groq", "Groq", "openai", "https://api.groq.com/openai/v1", "llama-3.3-70b-versatile",
            key: true, oss: true, url: "https://console.groq.com/keys"),
        preset!("openrouter", "OpenRouter (聚合)", "openai", "https://openrouter.ai/api/v1", "openrouter/auto",
            key: true, oss: true, url: "https://openrouter.ai/settings/keys"),
        preset!("ollama", "Ollama (本地)", "openai", "http://127.0.0.1:11434/v1", "qwen2.5-coder:7b",
            key: false, oss: true, url: "https://ollama.com/download"),
        preset!("lmstudio", "LM Studio (本地)", "openai", "http://127.0.0.1:1234/v1", "local-model",
            key: false, oss: true, url: "https://lmstudio.ai"),
    ]
}

/// Merge a remote catalog over the built-ins: remote entries with a known id
/// replace the built-in entry in place (so ordering stays stable); new ids
/// are appended. Malformed entries were already rejected by serde.
pub fn merge_presets(
    builtin: Vec<ProviderPreset>,
    remote: Vec<ProviderPreset>,
) -> Vec<ProviderPreset> {
    let mut merged = builtin;
    for preset in remote {
        if let Some(existing) = merged.iter_mut().find(|p| p.id == preset.id) {
            *existing = preset;
        } else {
            merged.push(preset);
        }
    }
    merged
}

/// Fetch the remote preset catalog. Callers MUST have obtained explicit
/// user consent before invoking this — it is the only chat-related code
/// path that talks to the network outside an LLM request.
pub async fn fetch_remote_presets(
    url: &str,
    proxy: &ProxySelection,
) -> anyhow::Result<Vec<ProviderPreset>> {
    let client = build_http_client(proxy)?;
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {} fetching presets", response.status());
    }
    let catalog: PresetCatalog = response.json().await?;
    Ok(catalog.presets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_presets_cover_open_and_closed_source() {
        let presets = builtin_presets();
        assert!(presets.iter().any(|p| p.open_source));
        assert!(presets.iter().any(|p| !p.open_source));
        // Local keyless runtimes are present for offline use.
        assert!(presets.iter().any(|p| !p.requires_key));
    }

    #[test]
    fn builtin_presets_have_unique_ids_and_valid_protocols() {
        let presets = builtin_presets();
        let mut ids = std::collections::HashSet::new();
        for p in &presets {
            assert!(ids.insert(p.id.clone()), "duplicate preset id: {}", p.id);
            assert!(
                p.protocol == "openai" || p.protocol == "anthropic",
                "unknown protocol '{}' in preset {}",
                p.protocol,
                p.id
            );
            assert!(!p.base_url.is_empty());
            assert!(!p.model.is_empty());
        }
    }

    #[test]
    fn merge_presets_replaces_by_id_and_appends_new() {
        let builtin = builtin_presets();
        let count = builtin.len();
        let remote = vec![
            ProviderPreset {
                model: "gpt-5".to_string(),
                ..builtin[0].clone()
            },
            preset!("newprov", "New Provider", "openai", "https://api.new.example/v1", "new-1",
                key: true, oss: true, url: ""),
        ];
        let merged = merge_presets(builtin, remote);
        assert_eq!(merged.len(), count + 1);
        assert_eq!(merged[0].model, "gpt-5"); // replaced in place
        assert_eq!(merged.last().unwrap().id, "newprov");
    }

    #[test]
    fn preset_catalog_json_roundtrip_matches_remote_schema() {
        let catalog = PresetCatalog {
            version: 1,
            presets: builtin_presets(),
        };
        let json = serde_json::to_string_pretty(&catalog).unwrap();
        let parsed: PresetCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.presets, builtin_presets());
    }

    #[test]
    fn preset_deserializes_with_defaults_for_optional_fields() {
        let json =
            r#"{"id":"x","name":"X","protocol":"openai","base_url":"https://x/v1","model":"m"}"#;
        let p: ProviderPreset = serde_json::from_str(json).unwrap();
        assert!(p.requires_key); // defaults to true — safe direction
        assert!(!p.open_source);
        assert!(p.key_url.is_empty());
    }

    #[test]
    fn shipped_remote_catalog_file_matches_builtin_presets() {
        // assets/llm-presets.json is what REMOTE_PRESETS_URL serves once the
        // repo is pushed — it must stay in sync with the built-in catalog so
        // a fresh "online refresh" is a no-op rather than a surprise.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/llm-presets.json");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let catalog: PresetCatalog = serde_json::from_str(&raw).unwrap();
        assert_eq!(catalog.presets, builtin_presets());
    }
}
