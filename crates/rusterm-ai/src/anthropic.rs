use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicSettings {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
}

impl Default for AnthropicSettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: None,
            model: "claude-sonnet-4-20250514".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
    system: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

pub struct AnthropicClient {
    http: HttpClient,
    settings: AnthropicSettings,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(settings: AnthropicSettings) -> Self {
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());

        Self {
            http: HttpClient::new(),
            settings,
            base_url,
        }
    }

    pub async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        let request = MessagesRequest {
            model: self.settings.model.clone(),
            max_tokens: 4096,
            messages: vec![Message {
                role: "user".to_string(),
                content: user_message.to_string(),
            }],
            system: Some(system_prompt.to_string()),
        };

        let response = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.settings.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await?;
            anyhow::bail!("Anthropic API error ({}): {}", status, body);
        }

        let data: MessagesResponse = response.json().await?;
        let text = data
            .content
            .iter()
            .filter_map(|b| {
                if b.block_type == "text" {
                    b.text.clone()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");

        Ok(text)
    }

    pub async fn suggest_command(
        &self,
        context: &str,
        partial: &str,
    ) -> anyhow::Result<Vec<String>> {
        let system = "You are a terminal command assistant. Given the context and partial input, suggest likely command completions. Return only the commands, one per line, no explanations.";
        let user = format!("Context: {}\nPartial input: {}\nSuggest commands:", context, partial);

        let response = self.complete(system, &user).await?;
        let suggestions: Vec<String> = response
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        Ok(suggestions)
    }
}
