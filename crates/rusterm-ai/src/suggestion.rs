use serde::{Deserialize, Serialize};

use crate::openai::OpenAIClient;
use crate::anthropic::AnthropicClient;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AiProvider {
    OpenAI,
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiSuggestion {
    pub command: String,
    pub confidence: f32,
    pub source: AiProvider,
}

pub struct SuggestionEngine {
    openai: Option<OpenAIClient>,
    anthropic: Option<AnthropicClient>,
}

impl SuggestionEngine {
    pub fn new() -> Self {
        Self {
            openai: None,
            anthropic: None,
        }
    }

    pub fn with_openai(&mut self, client: OpenAIClient) {
        self.openai = Some(client);
    }

    pub fn with_anthropic(&mut self, client: AnthropicClient) {
        self.anthropic = Some(client);
    }

    pub async fn suggest(
        &self,
        context: &str,
        partial: &str,
        provider: AiProvider,
    ) -> anyhow::Result<Vec<AiSuggestion>> {
        let suggestions = match provider {
            AiProvider::OpenAI => {
                if let Some(ref client) = self.openai {
                    let cmds = client.suggest_command(context, partial).await?;
                    cmds.into_iter()
                        .enumerate()
                        .map(|(i, cmd)| AiSuggestion {
                            command: cmd,
                            confidence: 1.0 - (i as f32 * 0.1),
                            source: AiProvider::OpenAI,
                        })
                        .collect()
                } else {
                    vec![]
                }
            }
            AiProvider::Anthropic => {
                if let Some(ref client) = self.anthropic {
                    let cmds = client.suggest_command(context, partial).await?;
                    cmds.into_iter()
                        .enumerate()
                        .map(|(i, cmd)| AiSuggestion {
                            command: cmd,
                            confidence: 1.0 - (i as f32 * 0.1),
                            source: AiProvider::Anthropic,
                        })
                        .collect()
                } else {
                    vec![]
                }
            }
        };

        Ok(suggestions)
    }

    pub async fn suggest_all(
        &self,
        context: &str,
        partial: &str,
    ) -> anyhow::Result<Vec<AiSuggestion>> {
        let mut results = Vec::new();

        if let Some(ref client) = self.openai {
            if let Ok(cmds) = client.suggest_command(context, partial).await {
                results.extend(cmds.into_iter().enumerate().map(|(i, cmd)| AiSuggestion {
                    command: cmd,
                    confidence: 1.0 - (i as f32 * 0.1),
                    source: AiProvider::OpenAI,
                }));
            }
        }

        if let Some(ref client) = self.anthropic {
            if let Ok(cmds) = client.suggest_command(context, partial).await {
                results.extend(cmds.into_iter().enumerate().map(|(i, cmd)| AiSuggestion {
                    command: cmd,
                    confidence: 1.0 - (i as f32 * 0.1),
                    source: AiProvider::Anthropic,
                }));
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggestion_creation() {
        let s = AiSuggestion {
            command: "kubectl get pods -n default".to_string(),
            confidence: 0.9,
            source: AiProvider::OpenAI,
        };

        assert_eq!(s.command, "kubectl get pods -n default");
        assert!((s.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(s.source, AiProvider::OpenAI);
    }

    #[test]
    fn test_suggestion_equality() {
        let s1 = AiSuggestion {
            command: "ls -la".to_string(),
            confidence: 1.0,
            source: AiProvider::Anthropic,
        };
        let s2 = AiSuggestion {
            command: "ls -la".to_string(),
            confidence: 1.0,
            source: AiProvider::Anthropic,
        };
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_ai_provider_equality() {
        assert_eq!(AiProvider::OpenAI, AiProvider::OpenAI);
        assert_ne!(AiProvider::OpenAI, AiProvider::Anthropic);
    }

    #[test]
    fn test_suggestion_engine_new() {
        let engine = SuggestionEngine::new();
        // No providers configured, should return empty
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(engine.suggest("ctx", "ls", AiProvider::OpenAI)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_suggestion_serialization() {
        let s = AiSuggestion {
            command: "git status".to_string(),
            confidence: 0.8,
            source: AiProvider::OpenAI,
        };

        let json = serde_json::to_string(&s).unwrap();
        let de: AiSuggestion = serde_json::from_str(&json).unwrap();
        assert_eq!(s, de);
    }
}
