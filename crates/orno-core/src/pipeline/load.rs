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

    // Cycle detection via Kahn's algorithm. `DagWalker::new` performs
    // the same check at execution time, but `orno validate` stops at
    // this function — without the inline pass, a cyclic pipeline would
    // pass validation and fail only at runtime. Keeping the load layer
    // decoupled from execution means inlining the algorithm here rather
    // than calling into `DagWalker`.
    let n = pipeline.nodes.len();
    let id_to_idx: std::collections::HashMap<&str, usize> = pipeline
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (idx, node) in pipeline.nodes.iter().enumerate() {
        for dep in &node.needs {
            // Safe indexing: the preceding loop already rejected any
            // `needs:` target that is not a declared id.
            let dep_idx = id_to_idx[dep.as_str()];
            in_degree[idx] += 1;
            dependents[dep_idx].push(idx);
        }
    }
    let mut queue: std::collections::VecDeque<usize> =
        (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut visited = 0usize;
    while let Some(idx) = queue.pop_front() {
        visited += 1;
        for &dep in &dependents[idx] {
            in_degree[dep] -= 1;
            if in_degree[dep] == 0 {
                queue.push_back(dep);
            }
        }
    }
    if visited < n {
        let cycle_node = pipeline
            .nodes
            .iter()
            .enumerate()
            .find(|(i, _)| in_degree[*i] > 0)
            .map_or("unknown", |(_, node)| node.id.as_str());
        return Err(PipelineError::Validation(format!(
            "cycle detected involving `{cycle_node}`"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::pipeline::schema::{Node, NodeKind, Pipeline, ShellNode};

    fn shell_node(id: &str, needs: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "true".to_string(),
                args: Vec::new(),
                stdin: None,
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            timeout: None,
        }
    }

    fn pipeline(nodes: Vec<Node>) -> Pipeline {
        Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            nodes,
        }
    }

    #[test]
    fn empty_pipeline_is_rejected() {
        let Err(PipelineError::Validation(msg)) = validate(&pipeline(Vec::new())) else {
            panic!("expected Validation error on empty pipeline");
        };
        assert!(
            msg.contains("no nodes"),
            "error message should explain the empty case: {msg}",
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let p = pipeline(vec![shell_node("a", &[]), shell_node("a", &[])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on duplicate ids");
        };
        assert!(
            msg.contains("duplicate") && msg.contains('a'),
            "error message should name the duplicate id: {msg}",
        );
    }

    #[test]
    fn unknown_needs_is_rejected() {
        let p = pipeline(vec![shell_node("b", &["nowhere"])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on unknown dep");
        };
        assert!(
            msg.contains("nowhere"),
            "error message should name the missing dep: {msg}",
        );
    }

    #[test]
    fn well_formed_pipeline_is_accepted() {
        let p = pipeline(vec![shell_node("a", &[]), shell_node("b", &["a"])]);
        assert!(validate(&p).is_ok(), "valid pipeline should pass");
    }

    #[test]
    fn cycle_is_rejected_by_validate() {
        // Two-node cycle: a -> b -> a. The existing `unknown needs`
        // check cannot catch this because both ids resolve; only the
        // Kahn pass notices the residual in-degree.
        let p = pipeline(vec![shell_node("a", &["b"]), shell_node("b", &["a"])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on cyclic pipeline");
        };
        assert!(
            msg.contains("cycle"),
            "error message should mention the cycle: {msg}",
        );
    }
}
