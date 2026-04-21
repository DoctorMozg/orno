//! Event sink trait and in-memory default. Future SQLite/OTLP sinks plug in
//! here without touching the scheduler.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::EventEnvelope;

#[async_trait]
pub trait EventSink: Send + Sync {
    async fn record(&self, envelope: EventEnvelope);
}

#[derive(Default, Clone)]
pub struct InMemorySink {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

impl InMemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the events recorded so far. Primarily for tests.
    #[must_use]
    pub fn snapshot(&self) -> Vec<EventEnvelope> {
        self.events
            .lock()
            .expect("event sink mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl EventSink for InMemorySink {
    async fn record(&self, envelope: EventEnvelope) {
        self.events
            .lock()
            .expect("event sink mutex poisoned")
            .push(envelope);
    }
}
