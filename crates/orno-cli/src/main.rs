mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use time::format_description::well_known::Rfc3339;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;

use crate::cli::{Cli, Command};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let args = Cli::parse();
    match args.command {
        Command::Run {
            pipeline,
            env,
            env_file,
            secrets_file,
        } => {
            let flags = commands::run::RunFlags {
                inline_env: env,
                env_files: env_file,
                secrets_files: secrets_file,
            };
            commands::run::run(&pipeline, flags).await
        }
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
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .with_timer(UtcTime::new(Rfc3339))
        .json()
        .init();
}
