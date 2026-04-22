//! Event sink trait. Concrete impls live in sibling files
//! (`in_memory_sink.rs` today; feature-gated `SqliteSink` or OTLP
//! forwarders in later phases) so the trait file stays a contract page.
//!
//! The sink owns `seq` and `timestamp` assignment. Callers pass bare
//! `Event` values; the sink wraps them into `EventEnvelope` with a
//! monotonic seq so a run's events are totally ordered regardless of
//! which subsystem (engine, executor) emits them.

use async_trait::async_trait;

use super::Event;

#[async_trait]
pub trait EventSink: Send + Sync {
    /// Wrap `event` in an envelope with a freshly-minted seq and the
    /// current UTC timestamp, then persist it. Engine and executors
    /// both call this — the sink is the single authority on
    /// cross-emitter ordering.
    async fn record(&self, event: Event);
}
