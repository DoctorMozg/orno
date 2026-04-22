//! `orno-core` — library surface for the orno orchestrator.
//!
//! The trait seams that the evolution path depends on all live in this
//! crate and are exercised (with dummy implementations) from the skeleton
//! onward. See `docs/adr/0003-event-log-from-day-one.md`.

pub mod agent;
pub mod budget;
pub mod config;
pub mod error;
pub mod events;
pub mod execution;
pub mod llm;
pub mod node;
pub mod pipeline;
pub mod telemetry;
pub mod tool;

pub use error::{AgentError, CoreError, LlmError, NodeError, PipelineError, ToolError};

/// Render the pipeline JSON Schema as a pretty-printed string. Used by the
/// `orno schema` subcommand; IDEs can reference the committed file via a
/// `# yaml-language-server: $schema=...` comment.
pub fn pipeline_json_schema_string() -> Result<String, serde_json::Error> {
    let schema = schemars::schema_for!(pipeline::Pipeline);
    serde_json::to_string_pretty(&schema)
}
