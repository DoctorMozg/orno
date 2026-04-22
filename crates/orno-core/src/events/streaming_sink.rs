//! `StreamingSink` — production-path `EventSink` for `orno run`. Each
//! `record` call serializes the envelope to NDJSON and flushes it to
//! the underlying writer immediately, so stdout stays tailable in
//! real time. `InMemorySink` is retained for tests that need to
//! snapshot the event stream.
//!
//! A single `Mutex<Inner>` covers both the `seq` counter and the
//! writer. One lock site keeps seq and wire order in lockstep — a
//! separate atomic on seq would let two tasks interleave lines in
//! the NDJSON stream with strictly-monotonic but visually reordered
//! `seq` values.

use std::io::Write;
use std::sync::Mutex;

use async_trait::async_trait;

use super::sink::EventSink;
use super::{Event, EventEnvelope};

struct Inner {
    writer: Box<dyn Write + Send>,
    next_seq: u64,
}

pub struct StreamingSink {
    inner: Mutex<Inner>,
}

impl StreamingSink {
    /// Wrap an arbitrary writer. Production wiring uses `stdout()`;
    /// tests can pass a buffer to assert on the rendered NDJSON.
    #[must_use]
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                writer,
                next_seq: 0,
            }),
        }
    }

    /// Convenience ctor for the `orno run` wiring: write NDJSON to
    /// process stdout.
    #[must_use]
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }
}

#[async_trait]
impl EventSink for StreamingSink {
    async fn record(&self, event: Event) {
        // Poison recovery matches `InMemorySink` — a panicking task on a
        // sibling node must not starve `RunFinished` on the way out.
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.next_seq += 1;
        let envelope = EventEnvelope::new(guard.next_seq, event);
        let line = match serde_json::to_string(&envelope) {
            Ok(line) => line,
            Err(e) => {
                // Serialization failure is a bug in the event shape,
                // not a runtime condition the engine can recover from
                // per-event. Log and drop — `EventSink::record` returns
                // `()` so we cannot surface it to the caller.
                tracing::warn!(error = %e, "failed to serialize event envelope");
                return;
            }
        };
        if let Err(e) = guard
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| guard.writer.write_all(b"\n"))
            .and_then(|()| guard.writer.flush())
        {
            tracing::warn!(error = %e, "failed to write event envelope to stream");
        }
    }
}
