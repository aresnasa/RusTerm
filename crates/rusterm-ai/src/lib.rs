pub mod anthropic;
pub mod openai;
pub mod shadow_sandbox;
pub mod suggestion;

#[cfg(feature = "qwen-local")]
pub mod qwen_local;
#[cfg(feature = "qwen-local")]
pub mod template_gen;

pub use anthropic::AnthropicClient;
pub use openai::OpenAIClient;
pub use shadow_sandbox::{
    ApprovedExecution, ShadowExecutionRequest, ShadowExecutionResult, ShadowSandbox,
    ShadowSandboxError, ShadowSandboxPhase,
};
pub use suggestion::{AiSuggestion, SuggestionEngine};

// Re-export the most-used local-inference types for convenience.
#[cfg(feature = "qwen-local")]
pub use qwen_local::{
    HardwareCapability, QwenLocalModel, SetupProgress, detect_hardware, ensure_model,
};
#[cfg(feature = "qwen-local")]
pub use template_gen::{TemplateKind, build_prompt, parse_response};
