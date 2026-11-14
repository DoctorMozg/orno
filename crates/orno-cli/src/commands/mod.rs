use std::sync::Arc;

use anyhow::{Result, bail};
use orno_core::events::{Event, EventSink, StreamingSink};
use orno_core::execution::scheduler::RunOutcome;

pub mod completions;
pub mod plan;
pub mod replay;
pub mod run;
pub mod schema;
pub mod validate;

/// Conventional Unix exit code for "process terminated by SIGINT"
/// (128 + signal number 2). `main` translates [`Interrupted`] into
/// this value via `ExitCode::from`.
pub const SIGINT_EXIT_CODE: u8 = 130;

/// Sentinel error returned by `orno run` / `orno replay` when the
/// process received SIGINT mid-run. `main` downcasts to this type and
/// translates it to exit code [`SIGINT_EXIT_CODE`]. A wrapping shell
/// can then distinguish operator cancellation from a normal pipeline
/// failure (which exits `1` via the anyhow surface).
///
/// Sentinel-via-error is used here instead of `std::process::exit`
/// because clippy's `disallowed-methods` policy refuses `process::exit`
/// — it skips Drop and tokio cleanup. By the time this error reaches
/// `main`, both subcommands have already emitted a sentinel
/// `RunFinished { ok: false }`, flushed any recording tapes, and (in
/// `run`) drained MCP servers under a 5s timeout.
#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted by SIGINT")
    }
}

impl std::error::Error for Interrupted {}

/// Block until the process receives SIGINT, then log a single WARN
/// line. Used inside `tokio::select!` arms so both `orno run` and
/// `orno replay` log the cancellation event with identical wording —
/// downstream log pipelines can grep for one substring instead of two.
pub async fn sigint_with_warning() {
    // `tokio::signal::ctrl_c()` returns `io::Result<()>`; on the
    // platforms orno targets the install never fails, but the
    // signature still forces us to acknowledge the result. A failed
    // install means we silently lose SIGINT handling — the WARN below
    // is suppressed only if the future itself returns an error, which
    // is the right behavior.
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %e, "SIGINT handler install failed");
        return;
    }
    tracing::warn!("SIGINT received, attempting graceful shutdown");
}

/// Sentinel in `RunFinished.failed_nodes` on SIGINT, so a stream reader
/// can tell an interrupted run from a node failure without the exit code.
const INTERRUPTED_FAILED_NODE: &str = "<interrupted>";

/// Emit the `RunFinished` that closes the stream of an interrupted run.
pub(crate) async fn emit_interrupted(sink: &Arc<dyn EventSink>, run_id: &str) {
    sink.record(Event::RunFinished {
        run_id: run_id.to_string(),
        ok: false,
        failed_nodes: vec![INTERRUPTED_FAILED_NODE.to_string()],
        skipped_nodes: Vec::new(),
    })
    .await;
}

/// Map the engine outcome and the sink's broken state to the process
/// result. `command` ("run" or "replay") names the subcommand in errors.
/// The sink swallows write errors mid-run so one EPIPE does not poison
/// every later `record`, but a broken sink still owes a non-zero exit.
pub(crate) fn finalize_run(
    streaming_sink: &StreamingSink,
    outcome: &RunOutcome,
    command: &str,
) -> Result<()> {
    if streaming_sink.is_broken() {
        bail!("event stream write failed (downstream closed?); {command} output is incomplete");
    }
    // Skipped nodes are visible as `node_skipped` envelopes, so only the
    // failed ones are listed here.
    if !outcome.ok {
        let names = if outcome.failed_nodes.is_empty() {
            "<unknown>".to_string()
        } else {
            outcome.failed_nodes.join(", ")
        };
        bail!("{command} failed: nodes [{names}] reported ok:false");
    }
    Ok(())
}
