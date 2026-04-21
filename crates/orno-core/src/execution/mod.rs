//! Execution engine — skeleton. The real scheduler, DAG planner, and
//! cancellation plumbing land in a later phase.

pub mod dag;
pub mod scheduler;

pub use scheduler::Engine;
