//! `orno-core` — library surface for the orno orchestrator.
//!
//! The trait seams that the evolution path depends on all live in this
//! crate and are exercised (with dummy implementations) from the skeleton
//! onward.

// Crate-level allows: these lints are correct for specific design decisions
// in orno-core and are allowed here rather than inline to keep the lint
// suppression centralized and documented.
#![allow(clippy::struct_excessive_bools)] // AgentPolicy uses bool flags for the strictness contract
#![allow(clippy::struct_field_names)] // wire-format types mirror provider API naming
#![allow(clippy::unnecessary_literal_bound)] // dynamic ToolHandler::name() lifetime bound

pub mod agent;
pub mod budget;
pub mod config;
pub mod error;
pub mod events;
pub mod execution;
pub mod llm;
pub mod mcp;
pub mod node;
pub mod pipeline;
pub mod plan;
pub mod telemetry;
pub mod tool;

pub use error::{AgentError, CoreError, LlmError, McpError, NodeError, PipelineError, ToolError};

/// Render the pipeline JSON Schema as a pretty-printed string. Used by the
/// `orno schema` subcommand; IDEs can reference the committed file via a
/// `# yaml-language-server: $schema=...` comment.
pub fn pipeline_json_schema_string() -> Result<String, serde_json::Error> {
    let schema = schemars::schema_for!(pipeline::Pipeline);
    serde_json::to_string_pretty(&schema)
}
