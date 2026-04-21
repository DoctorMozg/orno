//! LLM transport seam — ADR 0002 and 0003.
//!
//! Callers speak to `LlmTransport`, never to a concrete SDK. The production
//! implementation (to land in a later phase) wraps `genai`. The skeleton
//! ships `DummyTransport` for tests and for the first end-to-end run.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
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

/// Canned transport for skeleton and tests. Returns a deterministic
/// response without touching the network.
#[derive(Debug, Default, Clone)]
pub struct DummyTransport;

#[async_trait]
impl LlmTransport for DummyTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            content: format!(
                "[dummy] model={} prompt={:?}",
                req.model,
                req.prompt.chars().take(40).collect::<String>()
            ),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
        })
    }
}
