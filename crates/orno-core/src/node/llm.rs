//! LLM node executor. Skeleton impl delegates to `LlmTransport`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::NodeError;
use crate::llm::{LlmRequest, LlmTransport};

use super::{NodeExecutor, NodeRequest, NodeResponse};

pub struct LlmExecutor {
    transport: Arc<dyn LlmTransport>,
}

impl LlmExecutor {
    #[must_use]
    pub fn new(transport: Arc<dyn LlmTransport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl NodeExecutor for LlmExecutor {
    async fn execute(&self, id: &str, req: NodeRequest) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Llm(req) = req else {
            return Err(NodeError::UnknownKind {
                id: id.to_string(),
                kind: "non-llm request routed to LlmExecutor".into(),
            });
        };

        let llm_req = LlmRequest {
            provider: req.provider,
            model: req.model,
            prompt: req.prompt,
            temperature: req.temperature,
            max_tokens: req.max_tokens,
        };

        let response =
            self.transport
                .complete(llm_req)
                .await
                .map_err(|source| NodeError::Execution {
                    id: id.to_string(),
                    source: Box::new(source),
                })?;

        Ok(NodeResponse {
            node_id: id.to_string(),
            output: serde_json::json!({
                "content": response.content,
                "finish_reason": response.finish_reason,
                "usage": response.usage,
            }),
        })
    }
}
