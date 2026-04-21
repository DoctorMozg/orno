//! LLM transport seam — ADR 0002 and 0003.
//!
//! Callers speak to `LlmTransport`, never to a concrete SDK. The production
//! implementation (to land in a later phase) wraps `genai`. The skeleton
//! ships `DummyTransport` (in `dummy.rs`) for tests and for the first
//! end-to-end run.

pub mod dummy;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

pub use dummy::DummyTransport;

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
