//! Versioned, append-only event envelope.
//!
//! The `schema_version` on the envelope and `#[non_exhaustive]` on `Event`
//! together let us grow the enum without breaking existing replay files.

pub mod sink;

use serde::{Deserialize, Serialize};

pub use sink::{EventSink, InMemorySink};

/// Wire envelope for every event persisted or broadcast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    pub event: Event,
}

/// Lifecycle events emitted by the execution engine. Stays append-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    RunStarted {
        run_id: String,
    },
    NodeStarted {
        run_id: String,
        node_id: String,
    },
    NodeFinished {
        run_id: String,
        node_id: String,
        ok: bool,
    },
    BudgetExceeded {
        run_id: String,
        reason: String,
    },
    RunFinished {
        run_id: String,
        ok: bool,
    },
}

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
