//! Versioned, append-only event envelope.
//!
//! The `schema_version` on the envelope and `#[non_exhaustive]` on `Event`
//! together let us grow the enum without breaking existing replay files.
//! Every envelope carries an RFC 3339 `timestamp` so log pipelines can
//! correlate events with wall clock without reconstructing from `seq`.

pub mod in_memory_sink;
pub mod sink;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use in_memory_sink::InMemorySink;
pub use sink::EventSink;

/// Re-export of `llm::Usage` so `LlmResponseReceived` can carry it
/// without cross-module coupling at the event-consumer layer.
pub use crate::llm::Usage;

/// Wire envelope for every event persisted or broadcast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    /// UTC wall-clock instant the event was emitted, serialized as RFC
    /// 3339 (`"2026-04-21T15:30:00.123456789Z"`). Distinct from `seq`:
    /// `seq` is a strictly-monotonic emission order, `timestamp` is a
    /// human-readable correlator that makes event streams legible in
    /// logs without a tool to decode them.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub event: Event,
}

impl EventEnvelope {
    /// Build an envelope with the current schema version and a freshly
    /// captured UTC timestamp. The caller owns `seq` because the
    /// scheduler is the only component authoritative for emission
    /// order.
    #[must_use]
    pub fn new(seq: u64, event: Event) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            seq,
            timestamp: OffsetDateTime::now_utc(),
            event,
        }
    }
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
    NodeSkipped {
        run_id: String,
        node_id: String,
        reason: SkipReason,
    },
    BudgetExceeded {
        run_id: String,
        reason: String,
    },
    /// Emitted immediately before the transport is called. Carries
    /// provider + model identifiers but never the prompt — prompt
    /// bodies may contain rendered `secrets.*` values (ADR 0020) and
    /// must stay out of the event log. The response content is
    /// propagated through `NodeResponse.output`, not through events.
    LlmRequestStarted {
        run_id: String,
        node_id: String,
        provider: String,
        model: String,
    },
    /// Emitted immediately after a successful transport call. Carries
    /// the normalized `finish_reason` and token usage so downstream
    /// budget accounting has a numeric signal without parsing
    /// provider-specific payloads.
    LlmResponseReceived {
        run_id: String,
        node_id: String,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    },
    RunFinished {
        run_id: String,
        ok: bool,
    },
}

/// Why a node never ran. Expanded as new skip cases appear
/// (branch failure, explicit `when:` gate, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkipReason {
    /// An upstream node this node transitively depended on
    /// finished with `ok: false`.
    DependencyFailed { upstream: String },
}

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
