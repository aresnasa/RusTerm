pub mod openai;
pub mod anthropic;
pub mod suggestion;

pub use openai::OpenAIClient;
pub use anthropic::AnthropicClient;
pub use suggestion::{AiSuggestion, SuggestionEngine};
