use std::path::Path;

use anyhow::{Context, Result};
use orno_core::pipeline;
use orno_core::plan::plan_pipeline;

pub fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;
    let outputs = plan_pipeline(&pipeline)
        .with_context(|| format!("planning pipeline `{}`", path.display()))?;
    for output in &outputs {
        let line = serde_json::to_string(output).context("serializing plan output")?;
        println!("{line}");
    }
    Ok(())
}
