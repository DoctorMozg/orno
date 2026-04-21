//! Shell node executor — stub. Real subprocess handling lands with the
//! rest of the execution engine.

use async_trait::async_trait;

use crate::error::NodeError;

use super::{NodeExecutor, NodeRequest, NodeResponse};

#[derive(Debug, Default, Clone)]
pub struct ShellExecutor;

#[async_trait]
impl NodeExecutor for ShellExecutor {
    async fn execute(&self, id: &str, _req: NodeRequest) -> Result<NodeResponse, NodeError> {
        Err(NodeError::NotImplemented { id: id.to_string() })
    }
}
