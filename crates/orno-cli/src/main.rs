#![allow(missing_docs)] // binary crate; docs target is orno-core
#![allow(unreachable_pub)] // clap derive produces pub types we don't re-export
#![allow(clippy::print_stdout)] // CLI subcommands (schema, validate) write to stdout intentionally
#![allow(clippy::print_stderr)] // main() error handler writes to stderr intentionally

mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use time::format_description::well_known::Rfc3339;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;

use crate::cli::{Cli, Command};

/// Default cap on captured stderr in failure WARNs when `--verbose`
/// is passed without an explicit `--stderr-tail-bytes`. Verbose mode
/// is opt-in for operators who already accept that tool output may
/// surface; the higher cap matches the intent.
const VERBOSE_DEFAULT_TAIL_BYTES: usize = 65_536;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Cli::parse();

    // Verbose flag is read once here so tracing init and the engine
    // config see the same value. Other subcommands ignore it.
    let verbose = matches!(&args.command, Command::Run { verbose: true, .. });
    init_tracing(verbose);

    let result = dispatch(args).await;
    if let Err(err) = &result {
        // `{:#}` walks the anyhow source chain on a single line. Without
        // it, only the top-level message reaches stderr in release builds
        // and the actual cause is invisible. The `Err` return from `main`
        // would print `Debug` form, which is verbose but unredacted —
        // this gives a tighter, deterministic line.
        eprintln!("error: {err:#}");
    }
    result
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
            record_tape,
            replay_tape,
            record_tool_tape,
            replay_tool_tape,
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
                record_tape,
                replay_tape,
                record_tool_tape,
                replay_tool_tape,
            };
            commands::run::run(&pipeline, flags).await
        },
        Command::Validate { pipeline } => commands::validate::run(&pipeline),
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
