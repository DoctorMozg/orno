//! Tracing instrumentation points. The subscriber lives in `orno-cli`;
//! the library only emits spans and events.

use tracing::Level;

/// Emit a single span describing the current run. Used by the engine.
pub fn run_span(run_id: &str) -> tracing::Span {
    tracing::span!(Level::INFO, "run", run_id = %run_id)
}
