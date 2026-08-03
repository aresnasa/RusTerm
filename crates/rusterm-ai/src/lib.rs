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
// ModelConfig / builtin_models / resolve_model live in rusterm_core::config
// (they're persistence structs, not AI-layer concerns) — import them from
// there directly. This avoids a re-export that would need feature-gating.
#[cfg(feature = "qwen-local")]
pub use qwen_local::{
    HardwareCapability, QwenLocalModel, SetupProgress, detect_hardware, ensure_model,
};
#[cfg(feature = "qwen-local")]
pub use template_gen::{TemplateKind, build_prompt, parse_generated_response, parse_response};
