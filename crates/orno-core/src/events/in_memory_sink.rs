//! In-memory `EventSink` used by the skeleton CLI and by tests. A
//! feature-gated `SqliteSink` plugs in alongside this impl without
//! touching the scheduler or the trait.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::EventEnvelope;
use super::sink::EventSink;

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
