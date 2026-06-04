use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAISettings {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
}

impl Default for OpenAISettings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: None,
            model: "gpt-4o".to_string(),
        }
    }
}

pub struct OpenAIClient {
    http: reqwest::Client,
    settings: OpenAISettings,
    base_url: String,
}

impl OpenAIClient {
    pub fn new(settings: OpenAISettings) -> Self {
        let base_url = settings
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

        Self {
            http: reqwest::Client::new(),
            settings,
            base_url,
        }
    }

    pub async fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> anyhow::Result<String> {
        #[derive(Serialize)]
        struct ChatRequest {
            model: String,
            messages: Vec<Message>,
            max_tokens: u32,
        }

        #[derive(Serialize)]
        struct Message {
            role: String,
            content: String,
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
            content: String,
        }

        let request = ChatRequest {
            model: self.settings.model.clone(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system_prompt.to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: user_message.to_string(),
                },
            ],
            max_tokens: 4096,
        };

        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.settings.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await?;
            anyhow::bail!("OpenAI API error ({}): {}", status, body);
        }

        let data: ChatResponse = response.json().await?;
        let content = data
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        Ok(content)
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
