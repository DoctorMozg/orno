//! `orno-core` — library surface for the orno orchestrator.
//!
//! The trait seams that the evolution path depends on all live in this
//! crate and are exercised (with dummy implementations) from the skeleton
//! onward. See `docs/adr/0003-event-log-from-day-one.md`.

pub mod budget;
pub mod config;
pub mod error;
pub mod events;
pub mod execution;
pub mod llm;
pub mod node;
pub mod pipeline;
pub mod telemetry;

pub use error::{CoreError, LlmError, NodeError, PipelineError};
