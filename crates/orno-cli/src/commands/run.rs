//! `orno run <pipeline.yaml>` — skeleton dispatcher.
//!
//! Loads the pipeline, builds an `Engine` over an `InMemorySink`, and
//! prints every recorded `EventEnvelope` to stdout as NDJSON. Node
//! execution itself is a no-op until the scheduler is wired to
//! executors in a later phase.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use orno_core::events::InMemorySink;
use orno_core::execution::Engine;
use orno_core::pipeline;

pub async fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    let sink = Arc::new(InMemorySink::new());
    let engine = Engine::new(sink.clone());
    let run_id = new_run_id();

    engine.run(&run_id, &pipeline).await?;

    for envelope in sink.snapshot() {
        let line = serde_json::to_string(&envelope)?;
        println!("{line}");
    }
    Ok(())
}

/// Minimal unique id for a run. Real impl will switch to ULID once the
/// scheduler needs durable ordering guarantees.
fn new_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("run-{nanos}")
}
