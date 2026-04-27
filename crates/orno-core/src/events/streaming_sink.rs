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
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use super::sink::EventSink;
use super::{Event, EventEnvelope};

struct Inner {
    writer: Box<dyn Write + Send>,
    next_seq: u64,
}

pub struct StreamingSink {
    inner: Mutex<Inner>,
    /// Set the first time a write to the underlying stream fails.
    /// `record` returns `()`, so a downstream EPIPE (operator closed
    /// the consumer of `orno run | jq …`) cannot reach the engine
    /// in-band — but the CLI checks this flag after `engine.run` so
    /// the process can exit non-zero instead of silently swallowing
    /// the failure. `Ordering::Relaxed` is enough because the CLI
    /// reads it strictly after `engine.run` awaits to completion,
    /// which already provides happens-before through the runtime.
    broken: AtomicBool,
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
            broken: AtomicBool::new(false),
        }
    }

    /// Convenience ctor for the `orno run` wiring: write NDJSON to
    /// process stdout.
    #[must_use]
    pub fn stdout() -> Self {
        Self::new(Box::new(std::io::stdout()))
    }

    /// True when at least one envelope failed to reach the
    /// underlying writer (most commonly EPIPE from a closed
    /// downstream consumer). Latches once set; the CLI bails on
    /// `true` so `orno run | head -n1` exits non-zero rather than
    /// pretending the run produced a complete NDJSON stream.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
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
        // Build the envelope at a tentative seq, but commit the counter
        // only after serialization succeeds. Bumping `next_seq` first and
        // then dropping on a serde failure leaves a hole in the seq
        // sequence — replay's strict-monotonic invariant treats holes as
        // dropped events and aborts the tape. Defer the commit so a
        // serializer panic on event N does not silently corrupt event
        // N+1's seq.
        let tentative_seq = guard.next_seq + 1;
        let envelope = EventEnvelope::new(tentative_seq, event);
        let line = match serde_json::to_string(&envelope) {
            Ok(line) => line,
            Err(e) => {
                // Serialization failure is a bug in the event shape,
                // not a runtime condition the engine can recover from
                // per-event. Log and drop — `EventSink::record` returns
                // `()` so we cannot surface it to the caller. The
                // tentative seq is discarded so the next call reuses it.
                tracing::warn!(
                    error = %e,
                    seq = tentative_seq,
                    "failed to serialize event envelope; seq not advanced",
                );
                return;
            },
        };
        guard.next_seq = tentative_seq;
        if let Err(e) = guard
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| guard.writer.write_all(b"\n"))
            .and_then(|()| guard.writer.flush())
        {
            tracing::warn!(error = %e, "failed to write event envelope to stream");
            // Latch the broken flag so the CLI can bail after the run
            // completes. `Relaxed` is fine: the CLI reads strictly
            // after `engine.run().await`, which carries its own
            // happens-before barrier.
            self.broken.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::ErrorKind;

    /// Writer that fails every `write_all`. Stand-in for a closed
    /// downstream pipe so the test does not depend on `dup2`/EPIPE
    /// plumbing.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(ErrorKind::BrokenPipe, "test EPIPE"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn is_broken_starts_false_and_latches_after_write_failure() {
        // The CLI relies on this latch to convert a swallowed EPIPE
        // (record returns `()`) into a non-zero exit. Without the
        // latch, a downstream `head -n1` would silently succeed even
        // though the bulk of the run never reached the consumer.
        let sink = StreamingSink::new(Box::new(FailingWriter));
        assert!(!sink.is_broken(), "fresh sink should not report broken");

        sink.record(Event::RunStarted {
            run_id: "run_test".to_string(),
        })
        .await;

        assert!(
            sink.is_broken(),
            "is_broken must latch after a failed write"
        );
    }

    /// Writer that always succeeds. Confirms the broken flag stays
    /// false on a clean run — guards against a regression where the
    /// flag is set unconditionally.
    struct OkWriter;

    impl Write for OkWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn is_broken_stays_false_when_writes_succeed() {
        let sink = StreamingSink::new(Box::new(OkWriter));
        sink.record(Event::RunStarted {
            run_id: "run_test".to_string(),
        })
        .await;
        assert!(
            !sink.is_broken(),
            "clean writes must not latch the broken flag"
        );
    }
}
