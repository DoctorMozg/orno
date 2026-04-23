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

        /// Verbose diagnostics: bumps tracing to `debug` (unless
        /// `RUST_LOG` is already set) and lifts the default
        /// `--stderr-tail-bytes` cap from 2 KB to 64 KB. Failure
        /// records always appear regardless; this flag controls how
        /// much detail they carry.
        #[arg(short = 'v', long = "verbose")]
        verbose: bool,

        /// Cap on captured stderr (and similarly bounded payloads) in
        /// failure diagnostics. Default 2048 in normal mode, 65536
        /// when `--verbose`. An explicit value here always wins.
        #[arg(long = "stderr-tail-bytes", value_name = "BYTES")]
        stderr_tail_bytes: Option<usize>,

        /// Write all LLM request/response pairs to a tape file for later
        /// replay. Creates or truncates the file. Mutually exclusive with
        /// `--replay-tape`.
        #[arg(
            long = "record-tape",
            value_name = "PATH",
            conflicts_with = "replay_tape"
        )]
        record_tape: Option<PathBuf>,

        /// Replay LLM calls from a tape file instead of hitting the live
        /// API. A tape miss returns an error without a fallback call.
        /// Mutually exclusive with `--record-tape`.
        #[arg(
            long = "replay-tape",
            value_name = "PATH",
            conflicts_with = "record_tape"
        )]
        replay_tape: Option<PathBuf>,
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
