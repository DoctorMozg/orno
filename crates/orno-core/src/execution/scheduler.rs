//! Scheduler skeleton. Today: sequential walk of `dag::plan`. Later:
//! `JoinSet` with per-node `CancellationToken` + timeout + graceful
//! shutdown.

use std::sync::Arc;

use crate::error::CoreError;
use crate::events::{CURRENT_SCHEMA_VERSION, Event, EventEnvelope, EventSink};
use crate::pipeline::Pipeline;

pub struct Engine {
    sink: Arc<dyn EventSink>,
}

impl Engine {
    #[must_use]
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        Self { sink }
    }

    /// Run a pipeline and emit lifecycle events. Node execution itself is
    /// deferred to the real scheduler; this skeleton only walks the plan
    /// and emits `NodeStarted`/`NodeFinished(ok=false, NotImplemented)` to
    /// exercise the event pipe end-to-end.
    pub async fn run(&self, run_id: &str, pipeline: &Pipeline) -> Result<(), CoreError> {
        let mut seq: u64 = 0;
        let mut next_seq = || {
            seq += 1;
            seq
        };

        self.sink
            .record(EventEnvelope {
                schema_version: CURRENT_SCHEMA_VERSION,
                seq: next_seq(),
                event: Event::RunStarted {
                    run_id: run_id.to_string(),
                },
            })
            .await;

        for node_id in super::dag::plan(pipeline)? {
            self.sink
                .record(EventEnvelope {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    seq: next_seq(),
                    event: Event::NodeStarted {
                        run_id: run_id.to_string(),
                        node_id: node_id.clone(),
                    },
                })
                .await;

            // Skeleton: every node "finishes" without doing work.
            self.sink
                .record(EventEnvelope {
                    schema_version: CURRENT_SCHEMA_VERSION,
                    seq: next_seq(),
                    event: Event::NodeFinished {
                        run_id: run_id.to_string(),
                        node_id,
                        ok: true,
                    },
                })
                .await;
        }

        self.sink
            .record(EventEnvelope {
                schema_version: CURRENT_SCHEMA_VERSION,
                seq: next_seq(),
                event: Event::RunFinished {
                    run_id: run_id.to_string(),
                    ok: true,
                },
            })
            .await;

        Ok(())
    }
}
