//! LLM transport seam — ADR 0002 and 0003.
//!
//! Callers speak to `LlmTransport`, never to a concrete SDK. The
//! production implementation (`GenAiTransport`) wraps `genai`; the
//! `DummyTransport` is kept for tests and offline CI paths. The
//! recording/replay decorators sit on the same trait and compose
//! over any concrete transport.

pub mod dummy;
pub mod genai;
pub mod recording;
pub mod replay;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;

pub use dummy::DummyTransport;
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
