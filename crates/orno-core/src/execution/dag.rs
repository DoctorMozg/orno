//! DAG planning — skeleton. Topological sort and ready-set maintenance
//! land with the real scheduler.

use crate::error::PipelineError;
use crate::pipeline::Pipeline;

/// Placeholder planner. Returns node ids in source order.
pub fn plan(pipeline: &Pipeline) -> Result<Vec<String>, PipelineError> {
    Ok(pipeline.nodes.iter().map(|n| n.id.clone()).collect())
}
