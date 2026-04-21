//! YAML → validated `Pipeline`.

use std::path::Path;

use crate::error::PipelineError;

use super::schema::Pipeline;

/// Load a pipeline from an on-disk YAML file and validate it.
pub fn load_from_path(path: &Path) -> Result<Pipeline, PipelineError> {
    let bytes = std::fs::read(path).map_err(|source| PipelineError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let pipeline: Pipeline = serde_yaml_ng::from_slice(&bytes).map_err(PipelineError::Parse)?;
    validate(&pipeline)?;
    Ok(pipeline)
}

/// Validate semantic constraints that serde alone cannot enforce.
pub fn validate(pipeline: &Pipeline) -> Result<(), PipelineError> {
    if pipeline.nodes.is_empty() {
        return Err(PipelineError::Validation("pipeline has no nodes".into()));
    }

    let mut ids = std::collections::HashSet::new();
    for node in &pipeline.nodes {
        if !ids.insert(node.id.as_str()) {
            return Err(PipelineError::Validation(format!(
                "duplicate node id `{}`",
                node.id
            )));
        }
    }

    for node in &pipeline.nodes {
        for dep in &node.needs {
            if !ids.contains(dep.as_str()) {
                return Err(PipelineError::Validation(format!(
                    "node `{}` depends on unknown `{}`",
                    node.id, dep
                )));
            }
        }
    }

    Ok(())
}
