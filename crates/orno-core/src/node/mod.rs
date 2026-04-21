//! Node executor trait and shared wire types.
//!
//! `NodeRequest` and `NodeResponse` are the serde-tagged contracts that
//! every node kind flows through. Built-in executors implement the same
//! trait; the subprocess plugin transport (deferred past v0.1 per ADR
//! 0017) will reuse the same wire format without reintroducing a
//! sibling kind.

pub mod agent;
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
    Agent(AgentNodeRequest),
    Shell(ShellNodeRequest),
}

/// Runtime payload for a `kind: agent` node. Agents are referenced by
/// name in YAML; the scheduler resolves the name against
/// `Pipeline.agents` before dispatch and materializes the policy + tool
/// allowlist here. Templated fields (`initial_prompt`, `system`) are
/// already rendered by the time they reach this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNodeRequest {
    pub agent: String,
    pub initial_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellNodeRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
