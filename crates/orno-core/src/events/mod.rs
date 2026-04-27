//! Versioned, append-only event envelope.
//!
//! The `schema_version` on the envelope and `#[non_exhaustive]` on `Event`
//! together let us grow the enum without breaking existing replay files.
//! Every envelope carries an RFC 3339 `timestamp` so log pipelines can
//! correlate events with wall clock without reconstructing from `seq`.
//!
//! Layout: `event.rs` holds the `Event` enum; `failure.rs` holds the
//! typed failure payloads (`NodeFailure`, `LlmFailure`) plus
//! `SkipReason` / `BudgetKind`. `mod.rs` owns the `EventEnvelope` wire
//! format, the schema version constant, and the `truncate_excerpt`
//! helper shared by the failure classifier and the agent loop's
//! excerpt emission.

pub mod event;
pub mod failure;
pub mod in_memory_sink;
pub mod redactor;
pub mod sink;
pub mod streaming_sink;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use event::Event;
pub use failure::{BudgetKind, LlmFailure, NodeFailure, SkipReason};
pub use in_memory_sink::InMemorySink;
pub use redactor::Redactor;
pub use sink::EventSink;
pub use streaming_sink::StreamingSink;

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

/// Wire-format version.
///
/// **2 → 3 (PR #3, pre-opensource hardening pass).** Adds
/// `Event::NodeOutputTruncated`, emitted when a `kind: shell` node's
/// captured `stdout` or `stderr` exceeded the engine's new
/// `max_node_output_bytes` cap. The variant is purely additive — it
/// only fires on a new failure mode that previously could not occur
/// (the prior `wait_with_output` path captured everything in memory),
/// so existing replay files at version 2 remain readable as long as
/// the consumer treats unknown event types non-fatally.
///
/// **1 → 2 (PR #2).** Two additive changes shipped together so the
/// version only incremented once:
///
/// 1. `NodeStarted`, `NodeFinished`, `NodeSkipped`, and
///    `AgentIterationStarted` now carry a `node_kind: String` field
///    so downstream consumers can branch on agent-vs-shell without
///    cross-referencing the pipeline YAML.
/// 2. `NodePayloadFailure` carries an additional `signal: Option<i32>`
///    field. On Unix, a process killed by a signal now reports
///    `signal = Some(n)` and `exit_code = None` — previously
///    `exit_code` was rolled to `-1` and the signal was lost.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Keep the leading `max_bytes` of a string on a UTF-8 boundary,
/// suffixed with `"…"` when truncation occurred. HTTP error bodies put
/// the actionable signal at the *front* (status text, JSON
/// `error.message`); truncating from the head would drop exactly that
/// — opposite of stderr tails where the cause sits at the end.
///
/// The same head-retention semantics apply to prompt and response
/// excerpts on `LlmRequestStarted` / `LlmResponseReceived`:
/// a rendered prompt starts with the operator instruction, and a model
/// response starts with the direct answer — truncating either from
/// the back keeps the part a human would actually read first.
pub(crate) fn truncate_excerpt(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
