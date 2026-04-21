//! Pipeline schema, loading, and template rendering.

pub mod load;
pub mod schema;
pub mod template;

pub use schema::{
    AgentConfig, AgentNode, AgentPolicy, McpAuthConfig, McpHttpConfig, McpServerConfig,
    McpStdioConfig, Node, NodeKind, OnParseError, Pipeline, ShellNode,
};
