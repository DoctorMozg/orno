//! Execution engine — walker-driven sequential DAG runner. `Engine`
//! walks the pipeline's DAG, dispatches ready nodes through
//! registered `NodeExecutor`s, and emits lifecycle + skip events.

pub mod context;
pub mod scheduler;
pub mod walker;

pub use context::{Context, ContextConflict};
pub use scheduler::{Engine, EngineConfig, RunInputs};
pub use walker::{DagWalker, NodeState};

/// Generate a fresh run identifier in the form `run_<ULID>`.
///
/// The `run_` prefix makes the id self-describing in logs, replay
/// filenames, and grep output; the ULID payload is 26 Crockford
/// base32 characters whose first 48 bits are ms-since-epoch, so
/// lexicographic sort is chronological sort. That matches what the
/// record/replay infrastructure needs as a primary key without the
/// human-hostility of raw nanosecond timestamps.
///
/// Embedders that want to pin a specific id (tests, replay drivers)
/// skip this helper and pass their own string to `Engine::run`.
#[must_use]
pub fn new_run_id() -> String {
    format!("run_{}", ulid::Ulid::new())
}
