//! User-facing pipeline schema. Keep every variant explicit; no untagged
//! enums over bool-ish strings (the Norway problem).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Pipeline {
    /// Schema version. Incremented whenever the schema changes in a
    /// backwards-incompatible way.
    #[serde(default = "default_schema_version")]
    pub version: u32,
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
    pub nodes: Vec<Node>,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
    #[serde(default)]
    pub needs: Vec<String>,
}

/// Built-in node kinds plus the stub `External` variant reserved for the
/// future subprocess plugin protocol (ADR 0004).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeKind {
    Llm(LlmNode),
    Shell(ShellNode),
    External(ExternalNode),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmNode {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShellNode {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExternalNode {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}
