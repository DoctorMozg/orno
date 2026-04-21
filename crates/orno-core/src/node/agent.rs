//! Agent node executor — stub. The real impl owns the loop from ADR
//! 0005 and composes an `LlmTransport` (ADR 0002) plus tool handlers
//! (ADR 0008); both land with the execution engine in a later phase.

use async_trait::async_trait;

use crate::error::NodeError;

use super::{NodeExecutor, NodeRequest, NodeResponse};

#[derive(Debug, Default, Clone)]
pub struct AgentExecutor;

#[async_trait]
impl NodeExecutor for AgentExecutor {
    async fn execute(&self, id: &str, _req: NodeRequest) -> Result<NodeResponse, NodeError> {
        Err(NodeError::NotImplemented { id: id.to_string() })
    }
}
