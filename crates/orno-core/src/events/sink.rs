//! Event sink trait. Concrete impls live in sibling files
//! (`in_memory_sink.rs` today; feature-gated `SqliteSink` or OTLP
//! forwarders in later phases) so the trait file stays a contract page.

use async_trait::async_trait;

use super::EventEnvelope;

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn record(&self, envelope: EventEnvelope);
}
