use std::path::Path;

use anyhow::{Context, Result};
use orno_core::pipeline;
use orno_core::pipeline::checks::warnings;

/// Loading runs the full semantic validation in `orno-core`, so every
/// command that loads a pipeline rejects the same inputs.
pub fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    for w in warnings(&pipeline) {
        eprintln!("warning: {w}");
    }
    println!(
        "ok: version={} nodes={} agents={} mcp_servers={}",
        pipeline.version,
        pipeline.nodes.len(),
        pipeline.agents.len(),
        pipeline.mcp_servers.len(),
    );
    Ok(())
}
