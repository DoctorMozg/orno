use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser, Debug)]
#[command(
    name = "orno",
    version,
    about = "CI-native multi-agent orchestrator",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Execute a pipeline YAML file.
    Run {
        /// Path to the pipeline YAML file.
        pipeline: PathBuf,
    },

    /// Load and validate a pipeline YAML without running it.
    Validate {
        /// Path to the pipeline YAML file.
        pipeline: PathBuf,
    },

    /// Print the pipeline JSON Schema to stdout.
    Schema,

    /// Generate shell completions.
    Completions {
        /// Shell to generate completions for.
        shell: Shell,
    },
}
