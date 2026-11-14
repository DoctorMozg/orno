//! Pipeline schema, loading, and template rendering.

pub mod checks;
pub mod graph;
pub mod load;
pub mod schema;
pub mod template;

pub use schema::{
    AgentConfig, AgentNode, AgentPolicy, McpAuthConfig, McpHttpConfig, McpServerConfig,
    McpStdioConfig, Node, NodeKind, OnParseError, Pipeline, ShellNode,
};
