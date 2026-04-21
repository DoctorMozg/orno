use std::path::Path;

use anyhow::{Context, Result};
use orno_core::pipeline;

pub fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    println!(
        "ok: version={} nodes={}",
        pipeline.version,
        pipeline.nodes.len()
    );
    Ok(())
}
