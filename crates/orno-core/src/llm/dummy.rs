//! Canned `LlmTransport` for the skeleton and for tests — returns a
//! deterministic response without touching the network.

use async_trait::async_trait;

use crate::error::LlmError;

use super::{LlmRequest, LlmResponse, LlmTransport, Usage};

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
