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

        /// Inline `KEY=VAL` binding for the `env.*` template namespace
        /// (ADR 0020). Repeatable; last flag wins. Refused for names
        /// declared in the pipeline's `secrets:` block — argv leaks
        /// into shell history; use `--secrets-file` for credentials.
        #[arg(short = 'e', long = "env", value_name = "KEY=VAL")]
        env: Vec<String>,

        /// Dotenv file merged into the `env.*` template namespace.
        /// Repeatable; later files shadow earlier. A binding whose
        /// name appears in the pipeline's `secrets:` block is routed
        /// into `secrets.*` instead (ADR 0020).
        #[arg(long = "env-file", value_name = "PATH")]
        env_file: Vec<PathBuf>,

        /// Dotenv file merged into the `secrets.*` template namespace.
        /// Repeatable; later files shadow earlier (ADR 0020).
        #[arg(long = "secrets-file", value_name = "PATH")]
        secrets_file: Vec<PathBuf>,
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
