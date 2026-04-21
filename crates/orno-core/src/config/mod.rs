//! Provider configuration with explicit capability flags.
//!
//! The research (§2) is emphatic: "Don't detect, declare." Every provider is
//! a named backend with an explicit feature set. Sniffing capabilities at
//! runtime is what turns "OpenAI-compatible" into a bug farm.

use serde::{Deserialize, Serialize};

/// A single LLM provider endpoint plus its declared capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// SSE dialect — normal `data: ... [DONE]` framing, or Ollama-native NDJSON.
    #[serde(default)]
    pub dialect: SseDialect,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub tool_calling: bool,
    #[serde(default)]
    pub structured_outputs: bool,
    #[serde(default)]
    pub reasoning_content: Option<ReasoningField>,
    #[serde(default)]
    pub uses_max_completion_tokens: bool,
    #[serde(default)]
    pub clamps_temperature: bool,
    #[serde(default)]
    pub supports_stream_usage: bool,
}

/// Which field name the provider uses for reasoning tokens, if any.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasoningField {
    ReasoningContent,
    Reasoning,
    InlineThinkTag,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SseDialect {
    #[default]
    OpenAi,
    OllamaNdjson,
}
