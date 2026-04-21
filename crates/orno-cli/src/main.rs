mod cli;
mod commands;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let args = Cli::parse();
    match args.command {
        Command::Run { pipeline } => commands::run::run(&pipeline).await,
        Command::Validate { pipeline } => commands::validate::run(&pipeline),
        Command::Schema => commands::schema::run(),
        Command::Completions { shell } => commands::completions::run(shell),
    }
}

/// Tracing goes to stderr as JSON so users can pipe CI logs straight into
/// their log pipeline. Pipeline output (event envelopes, schema) goes to
/// stdout — keeping the two streams separable by design.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .json()
        .init();
}
