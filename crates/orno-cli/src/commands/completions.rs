use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::Cli;

pub fn run(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(())
}
