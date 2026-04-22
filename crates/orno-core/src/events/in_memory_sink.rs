//! In-memory `EventSink` used by the skeleton CLI and by tests. A
//! feature-gated `SqliteSink` plugs in alongside this impl without
//! touching the scheduler or the trait.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::sink::EventSink;
use super::{Event, EventEnvelope};

/// Shared state behind the sink. The mutex protects both `events`
/// and the strictly-monotonic `seq` counter — they are mutated
/// together on every `record` call.
#[derive(Default)]
struct Inner {
    events: Vec<EventEnvelope>,
    next_seq: u64,
}

#[derive(Default, Clone)]
pub struct InMemorySink {
    inner: Arc<Mutex<Inner>>,
}

impl InMemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the events recorded so far. Primarily for tests.
    #[must_use]
    pub fn snapshot(&self) -> Vec<EventEnvelope> {
        // Recover from a poisoned mutex: a panicking async task must not
        // silently starve subsequent `record` calls (notably `RunFinished`).
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .clone()
    }
}

#[async_trait]
impl EventSink for InMemorySink {
    async fn record(&self, event: Event) {
        // See `snapshot` — poison recovery keeps the terminal events flowing
        // even after a mid-run panic on another task.
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.next_seq += 1;
        let envelope = EventEnvelope::new(guard.next_seq, event);
        guard.events.push(envelope);
    }
}
