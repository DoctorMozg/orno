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
        tracing::warn!(error = ?e, "SIGINT handler install failed");
        return;
    }
    tracing::warn!("SIGINT received, attempting graceful shutdown");
}
