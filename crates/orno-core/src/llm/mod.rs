//! LLM transport seam.
//!
//! Callers speak to `LlmTransport`, never to a concrete SDK. The
//! production implementation (`GenAiTransport`) wraps `genai`; the
//! `DummyTransport` is kept for tests and offline CI paths. The
//! recording/replay decorators sit on the same trait and compose
//! over any concrete transport.

pub mod bundle;
pub mod dummy;
pub mod genai;
pub mod recording;
pub mod replay;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

pub use bundle::{BundleContents, BundleEntry, BundleError, read_bundle, write_bundle};
pub use dummy::DummyTransport;
pub use dummy::ScriptedTransport;
pub use genai::GenAiTransport;
pub use recording::RecordingTransport;
pub use replay::ReplayTransport;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Multi-turn conversation history. Empty for single-turn requests.
    #[serde(default)]
    pub messages: Vec<OrnoChatMessage>,
    /// Tools the model may call on this request. Empty means no tool use.
    #[serde(default)]
    pub tools: Vec<OrnoChatTool>,
}

impl LlmRequest {
    /// Build the single-turn request shape (no message history, no tools).
    /// Use when the caller does not need multi-turn context.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor for a value type; no abstraction needed pre-v0.1"
    )]
    pub fn from_prompt(
        provider: String,
        model: String,
        prompt: String,
        system: Option<String>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            provider,
            model,
            prompt,
            system,
            temperature,
            max_tokens,
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }
}

/// A single tool call issued by the model (part of an assistant turn).
/// Orno-owned type — keeps `genai::ToolCall` off the public surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrnoChatToolCall {
    /// Provider-issued id that pairs this call with its later `ToolResult`.
    pub call_id: String,
    /// Name of the tool the model is invoking.
    pub fn_name: String,
    /// JSON arguments the model passed to the tool.
    pub fn_arguments: serde_json::Value,
}

/// A tool available for the model to call on this request.
/// Orno-owned — `genai::Tool` stays behind `GenAiTransport`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrnoChatTool {
    /// Tool name the model will reference in `OrnoChatToolCall::fn_name`.
    pub name: String,
    /// Human-readable description surfaced to the model.
    pub description: String,
    /// JSON Schema describing the tool's argument shape.
    pub schema: serde_json::Value,
}

/// A message in the conversation history.
///
/// Serializes with a `role` discriminator so each variant lands as
/// `{ "role": "user", "content": "..." }` on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrnoChatMessage {
    /// A user turn carrying plain-text content.
    User {
        /// The user's message text.
        content: String,
    },
    /// An assistant turn carrying plain-text content (no tool calls).
    Assistant {
        /// The assistant's message text.
        content: String,
    },
    /// An assistant turn whose entire content is tool calls (no text).
    ToolCalls {
        /// The tool calls the assistant issued on this turn.
        calls: Vec<OrnoChatToolCall>,
    },
    /// A tool-execution result sent back to the model.
    ToolResult {
        /// The `call_id` from the originating `OrnoChatToolCall`.
        call_id: String,
        /// The tool's output, serialized as a string.
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// Tool calls the model issued on this turn. Empty when the model replied
    /// with plain text (which lands in `content`).
    #[serde(default)]
    pub tool_calls: Vec<OrnoChatToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[async_trait]
pub trait LlmTransport: Send + Sync {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError>;
}
