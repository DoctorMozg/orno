//! Node executor trait and shared wire types.
//!
//! `NodeRequest` and `NodeResponse` are the serde-tagged contracts that the
//! future subprocess plugin protocol will cross (ADR 0004). Built-in
//! executors implement the same trait.

pub mod llm;
pub mod registry;
pub mod shell;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::NodeError;

/// Input to a node. `#[non_exhaustive]` so we can grow the variant list
/// without breaking on-disk replays.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeRequest {
    Llm(LlmNodeRequest),
    Shell(ShellNodeRequest),
    External(ExternalNodeRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmNodeRequest {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellNodeRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalNodeRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResponse {
    pub node_id: String,
    pub output: serde_json::Value,
}

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    async fn execute(&self, id: &str, req: NodeRequest) -> Result<NodeResponse, NodeError>;
}
