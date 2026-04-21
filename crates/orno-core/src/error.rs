//! Error hierarchy. One enum per subsystem, `#[source]` for chaining,
//! `#[from]` only where the conversion is unambiguous.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Pipeline(#[from] PipelineError),

    #[error(transparent)]
    Node(#[from] NodeError),

    #[error(transparent)]
    Llm(#[from] LlmError),
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error("failed to read pipeline file {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse pipeline YAML")]
    Parse(#[source] serde_yaml_ng::Error),

    #[error("pipeline validation failed: {0}")]
    Validation(String),

    #[error("template render failed for `{name}`")]
    Template {
        name: String,
        #[source]
        source: minijinja::Error,
    },
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("node `{id}` kind `{kind}` is not registered")]
    UnknownKind { id: String, kind: String },

    #[error("node `{id}` is not implemented yet (skeleton stub)")]
    NotImplemented { id: String },

    #[error("node `{id}` failed")]
    Execution {
        id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("LLM transport is not wired yet (skeleton stub)")]
    NotImplemented,

    #[error("LLM request rejected: {0}")]
    Rejected(String),
}
