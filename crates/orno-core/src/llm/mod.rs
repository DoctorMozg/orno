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

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

pub use bundle::{
    BundleContents, BundleEntry, BundleError, CURRENT_BUNDLE_VERSION, read_bundle,
    scrub_pipeline_secrets, write_bundle,
};
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
    pub messages: Arc<Vec<OrnoChatMessage>>,
    /// Tools the model may call on this request. Empty means no tool use.
    /// `Arc`-wrapped so the agent loop can hand the same definitions
    /// to every iteration's request without re-cloning the underlying
    /// `Vec`. Serializes identically to `Vec<OrnoChatTool>` thanks to
    /// serde's blanket `Arc<T>` impl, so the wire format is unchanged.
    #[serde(default)]
    pub tools: Arc<Vec<OrnoChatTool>>,
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
            messages: Arc::new(Vec::new()),
            tools: Arc::new(Vec::new()),
        }
    }

    /// Replace the request's tool definitions. The vector is wrapped in
    /// an `Arc` internally so the same definitions can be cheaply shared
    /// across iterations of an agent loop.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<OrnoChatTool>) -> Self {
        self.tools = Arc::new(tools);
        self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_prompt_initializes_tools_to_empty_arc_vec() {
        // Default constructor invariant: a `from_prompt` request carries
        // an empty `Arc<Vec<OrnoChatTool>>`, never a `None` placeholder
        // or a stack-borrowed slice. Subsequent `Arc::clone(&tools)` in
        // the agent loop relies on this shape to cheaply hand the same
        // (empty) definitions to every iteration.
        let req = LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hi".into(),
            None,
            None,
            None,
        );
        assert!(
            req.tools.is_empty(),
            "from_prompt must yield an empty tools list",
        );
        // A second clone of the request must share the same underlying
        // allocation — proves the field is genuinely `Arc<Vec<...>>` and
        // not, say, an inner `Vec` rebuilt on each clone.
        let cloned = req.clone();
        assert!(
            Arc::ptr_eq(&req.tools, &cloned.tools),
            "from_prompt's empty tools Arc must be shared by Clone",
        );
    }

    #[test]
    fn with_tools_wraps_in_arc_and_replaces_existing() {
        // Audit fix: `LlmRequest.tools` was promoted to `Arc<Vec<...>>`
        // so the agent loop hands the same definitions to every
        // iteration without re-cloning the schema vector. `with_tools`
        // takes an owned `Vec` and must wrap it in a fresh `Arc`,
        // overwriting any prior tools.
        let tool = OrnoChatTool {
            name: "Bash".into(),
            description: "run a shell command".into(),
            schema: serde_json::json!({"type": "object"}),
        };
        let req = LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .with_tools(vec![tool.clone()]);

        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "Bash");

        // Replacement semantics: a second call must drop the prior
        // contents, not append. Without this, a caller that re-uses an
        // `LlmRequest` builder would see stale tool defs leak into the
        // next request.
        let replaced = req.with_tools(vec![]);
        assert!(
            replaced.tools.is_empty(),
            "with_tools must replace the existing list, not append",
        );
    }

    #[test]
    fn with_tools_arc_is_shared_across_clones() {
        // The `Arc` shape is load-bearing for the agent loop's
        // hot-path: every iteration clones the entire `LlmRequest` to
        // hand it to the transport, and a deep `Vec` clone on each
        // iteration would be O(n) over the schema list. `Arc::ptr_eq`
        // proves the bytes are not duplicated on `Clone`.
        let tools = vec![
            OrnoChatTool {
                name: "Bash".into(),
                description: "run".into(),
                schema: serde_json::json!({}),
            },
            OrnoChatTool {
                name: "Read".into(),
                description: "read".into(),
                schema: serde_json::json!({}),
            },
        ];
        let req = LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .with_tools(tools);

        let cloned = req.clone();
        assert!(
            Arc::ptr_eq(&req.tools, &cloned.tools),
            "Clone must share the tools allocation; otherwise every \
             agent-loop iteration deep-clones the schema list",
        );
    }
}
