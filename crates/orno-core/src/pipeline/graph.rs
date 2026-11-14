//! Dependency graph over a pipeline's nodes: one place that rejects
//! duplicate ids, unknown `needs:` targets and cycles, shared by load-time
//! validation and the execution walker.

use std::collections::{BTreeMap, VecDeque};

use crate::error::PipelineError;

use super::schema::Node;

/// Index-based adjacency for `nodes`, in source order.
#[derive(Debug)]
pub struct DepGraph {
    /// Node id to its position in the source list.
    pub by_id: BTreeMap<String, usize>,
    /// `dependents[i]` lists the nodes that name node `i` in `needs:`.
    pub dependents: Vec<Vec<usize>>,
    /// Number of `needs:` entries per node.
    pub in_degree: Vec<usize>,
}

impl DepGraph {
    /// Build the graph, failing with [`PipelineError::InvalidGraph`] on a
    /// duplicate id, an unknown `needs:` target or a cycle.
    pub fn build(nodes: &[Node]) -> Result<Self, PipelineError> {
        let n = nodes.len();
        let mut by_id = BTreeMap::new();
        for (idx, node) in nodes.iter().enumerate() {
            if by_id.insert(node.id.clone(), idx).is_some() {
                return Err(PipelineError::InvalidGraph {
                    reason: format!("duplicate node id `{}`", node.id),
                });
            }
        }

        let mut dependents = vec![Vec::new(); n];
        let mut in_degree = vec![0; n];
        for (idx, node) in nodes.iter().enumerate() {
            for dep in &node.needs {
                let Some(&dep_idx) = by_id.get(dep) else {
                    return Err(PipelineError::InvalidGraph {
                        reason: format!("node `{}` depends on unknown `{dep}`", node.id),
                    });
                };
                dependents[dep_idx].push(idx);
                in_degree[idx] += 1;
            }
        }

        let graph = Self {
            by_id,
            dependents,
            in_degree,
        };
        graph.check_acyclic(nodes)?;
        Ok(graph)
    }

    /// Kahn's algorithm over a scratch copy of the in-degrees. Names the
    /// lowest-indexed node left on a cycle so the message is stable.
    fn check_acyclic(&self, nodes: &[Node]) -> Result<(), PipelineError> {
        let mut remaining = self.in_degree.clone();
        let mut queue: VecDeque<usize> = (0..nodes.len()).filter(|&i| remaining[i] == 0).collect();
        let mut popped = 0;
        while let Some(idx) = queue.pop_front() {
            popped += 1;
            for &dep in &self.dependents[idx] {
                remaining[dep] -= 1;
                if remaining[dep] == 0 {
                    queue.push_back(dep);
                }
            }
        }
        if popped < nodes.len() {
            let residual = remaining
                .iter()
                .position(|&d| d > 0)
                .expect("a short pop count leaves a node with unmet dependencies");
            return Err(PipelineError::InvalidGraph {
                reason: format!("cycle detected involving `{}`", nodes[residual].id),
            });
        }
        Ok(())
    }
}
