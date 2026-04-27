#![allow(missing_docs)] // binary crate; docs target is orno-core
#![allow(unreachable_pub)] // clap derive produces pub types we don't re-export
#![allow(clippy::print_stdout)] // CLI subcommands (schema, validate) write to stdout intentionally
#![allow(clippy::print_stderr)] // main() error handler writes to stderr intentionally

mod cli;
mod commands;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;
use time::format_description::well_known::Rfc3339;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;

use crate::cli::{Cli, Command};
use crate::commands::{Interrupted, SIGINT_EXIT_CODE};

/// Default cap on captured stderr in failure WARNs when `--verbose`
/// is passed without an explicit `--stderr-tail-bytes`. Verbose mode
/// is opt-in for operators who already accept that tool output may
/// surface; the higher cap matches the intent.
const VERBOSE_DEFAULT_TAIL_BYTES: usize = 65_536;

/// Default per-stream cap on a `kind: shell` node's captured stdout
/// or stderr when the operator does not pass `--max-node-output-bytes`.
/// Mirrors `EngineConfig::default().max_node_output_bytes`. Kept here
/// so the CLI can distinguish "operator passed `0` to disable the cap"
/// (still surfaced as `0` to the engine) from "operator omitted the
/// flag" (use this default).
const DEFAULT_MAX_NODE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args = Cli::parse();

    // Verbose flag is read once here so tracing init and the engine
    // config see the same value. Other subcommands ignore it.
    let verbose = matches!(&args.command, Command::Run { verbose: true, .. });
    init_tracing(verbose);

    match dispatch(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // SIGINT path: `run` / `replay` already emitted a sentinel
            // `RunFinished { ok: false }` and (for `run`) drained MCP
            // servers. Surface exit code 130 silently — printing
            // "error: interrupted by SIGINT" would noise up CI logs
            // for a user-initiated cancellation.
            if err.downcast_ref::<Interrupted>().is_some() {
                return ExitCode::from(SIGINT_EXIT_CODE);
            }
            // `{:#}` walks the anyhow source chain on a single line.
            // Without it, only the top-level message reaches stderr in
            // release builds and the actual cause is invisible.
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        },
    }
}

async fn dispatch(args: Cli) -> Result<()> {
    match args.command {
        Command::Run {
            pipeline,
            env,
            env_file,
            secrets_file,
            verbose,
            stderr_tail_bytes,
            max_node_output_bytes,
            record_tape,
            replay_tape,
            record_tool_tape,
            replay_tool_tape,
            record_bundle,
        } => {
            let flags = commands::run::RunFlags {
                inline_env: env,
                env_files: env_file,
                secrets_files: secrets_file,
                verbose,
                max_output_bytes: stderr_tail_bytes.unwrap_or(if verbose {
                    VERBOSE_DEFAULT_TAIL_BYTES
                } else {
                    2048
                }),
                max_node_output_bytes: max_node_output_bytes
                    .unwrap_or(DEFAULT_MAX_NODE_OUTPUT_BYTES),
                record_tape,
                replay_tape,
                record_tool_tape,
                replay_tool_tape,
                record_bundle,
            };
            commands::run::run(&pipeline, flags).await
        },
        Command::Replay { bundle } => commands::replay::run(&bundle).await,
        Command::Validate { pipeline } => commands::validate::run(&pipeline),
        Command::Plan { pipeline } => commands::plan::run(&pipeline),
        Command::Schema => commands::schema::run(),
        Command::Completions { shell } => commands::completions::run(shell),
    }
}

/// Tracing goes to stderr as JSON so users can pipe CI logs straight
/// into their log pipeline. Pipeline output (event envelopes, schema)
/// goes to stdout — keeping the two streams separable by design.
/// Timestamps match the `EventEnvelope.timestamp` format (RFC 3339 UTC)
/// so a run's stdout and stderr are trivially joinable on wall clock.
///
/// `verbose` bumps the default filter from `info` to `debug` so users
/// get richer tracing without touching `RUST_LOG`. An explicit
/// `RUST_LOG` always wins — operators who pinned a level intentionally
/// are not overridden.
fn init_tracing(verbose: bool) {
    let default_level = if verbose { "debug" } else { "info" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_timer(UtcTime::new(Rfc3339))
        .json()
        .init();
}
