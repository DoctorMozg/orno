//! Generator-style DAG walker. Keeps node-state internally; the
//! caller pulls "what's ready next?" rather than receiving a
//! pre-built ordered list. Branch failure skips every transitive
//! descendant and the walker surfaces those skips in causal order.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};

use crate::error::PipelineError;
use crate::events::SkipReason;
use crate::pipeline::graph::DepGraph;
use crate::pipeline::{Node, Pipeline};

/// Per-node lifecycle state tracked by the walker.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped { reason: SkipReason },
}

/// Generator-style DAG walker.
///
/// Construction validates the graph (duplicate ids, cycles, unknown `needs:` targets)
/// and fails fast with [`PipelineError::InvalidGraph`]. After that, the
/// walker answers [`DagWalker::next_ready`] until the DAG drains. Every
/// [`DagWalker::complete`] call may cascade skips through transitive
/// descendants; those are returned so the caller can emit `NodeSkipped`
/// events in causal order before resuming.
pub struct DagWalker<'p> {
    nodes: Vec<&'p Node>,
    by_id: BTreeMap<String, usize>,
    state: Vec<NodeState>,
    // Reverse adjacency: dependents[i] holds indices of nodes that
    // list nodes[i] in their `needs:`. Forward adjacency is not
    // stored — the cascade and ready-promotion both walk downstream.
    dependents: Vec<Vec<usize>>,
    // Remaining unmet dependency count per node; decremented as
    // each upstream succeeds.
    waiting_on: Vec<usize>,
    // Min-heap on source index, so ties among ready nodes run in YAML
    // source order even when a completion promotes an earlier node.
    ready_queue: BinaryHeap<Reverse<usize>>,
}

impl<'p> DagWalker<'p> {
    /// Build a walker over the pipeline. Validates the DAG; returns
    /// [`PipelineError::InvalidGraph`] on duplicate ids, unknown
    /// `needs:` targets or cycles.
    #[must_use = "walker errors must be propagated"]
    pub fn new(pipeline: &'p Pipeline) -> Result<Self, PipelineError> {
        let graph = DepGraph::build(&pipeline.nodes)?;
        let nodes: Vec<&Node> = pipeline.nodes.iter().collect();
        let n = nodes.len();
        let DepGraph {
            by_id,
            dependents,
            in_degree: waiting_on,
        } = graph;

        let mut state: Vec<NodeState> = vec![NodeState::Pending; n];
        let mut ready_queue = BinaryHeap::new();
        for idx in 0..n {
            if waiting_on[idx] == 0 {
                state[idx] = NodeState::Ready;
                ready_queue.push(Reverse(idx));
            }
        }

        Ok(Self {
            nodes,
            by_id,
            state,
            dependents,
            waiting_on,
            ready_queue,
        })
    }

    /// Return the next node the scheduler should execute, or `None`
    /// when no ready nodes remain. Marks the returned node as
    /// `Running`.
    ///
    /// A `None` return does not imply the DAG is drained: a future
    /// parallel scheduler may still have nodes `Running`.
    pub fn next_ready(&mut self) -> Option<&'p Node> {
        let Reverse(idx) = self.ready_queue.pop()?;
        self.state[idx] = NodeState::Running;
        Some(self.nodes[idx])
    }

    /// Record the outcome of a running node. On success, promotes
    /// every downstream node whose unmet-dep count falls to zero to
    /// `Ready`. On failure, marks every transitive downstream node
    /// as `Skipped { reason: DependencyFailed { upstream: id } }`
    /// and returns the newly-skipped ids with their reasons in
    /// causal (BFS) order — the caller emits those as `NodeSkipped`
    /// events before pulling the next ready node.
    ///
    /// The `upstream` on every skip reason names the originating
    /// failure (this `id`), not the skipped node's immediate parent;
    /// the single root cause is what the user needs to see.
    ///
    /// Returns an empty `Vec` on success.
    ///
    /// # Panics
    /// Panics if `id` is unknown or is not in `Running` state — both
    /// are scheduler bugs the walker surfaces loudly rather than
    /// silently absorbs.
    pub fn complete(&mut self, id: &str, ok: bool) -> Vec<(String, SkipReason)> {
        let idx = *self
            .by_id
            .get(id)
            .unwrap_or_else(|| panic!("complete called with unknown id `{id}`"));
        assert!(
            matches!(self.state[idx], NodeState::Running),
            "complete called on node `{id}` which is not Running",
        );

        if ok {
            self.state[idx] = NodeState::Succeeded;
            // Clone the dependents list — the borrow checker needs
            // `self.state` mutably while iterating, and the list is
            // small.
            let deps = self.dependents[idx].clone();
            for dep in deps {
                self.waiting_on[dep] -= 1;
                if self.waiting_on[dep] == 0 && matches!(self.state[dep], NodeState::Pending) {
                    self.state[dep] = NodeState::Ready;
                    self.ready_queue.push(Reverse(dep));
                }
            }
            return Vec::new();
        }

        self.state[idx] = NodeState::Failed;

        let mut skipped: Vec<(String, SkipReason)> = Vec::new();
        let mut bfs: VecDeque<usize> = VecDeque::new();
        bfs.extend(self.dependents[idx].iter().copied());
        while let Some(cur) = bfs.pop_front() {
            if !matches!(self.state[cur], NodeState::Pending | NodeState::Ready) {
                continue;
            }
            let reason = SkipReason::DependencyFailed {
                upstream: id.to_string(),
            };
            self.state[cur] = NodeState::Skipped {
                reason: reason.clone(),
            };
            skipped.push((self.nodes[cur].id.clone(), reason));
            for &next in &self.dependents[cur] {
                bfs.push_back(next);
            }
        }

        // Purge any skipped indices that were previously queued ready.
        // Rebuilding the queue once after the cascade is cheaper than
        // scanning it per-skip.
        if !skipped.is_empty() {
            self.ready_queue
                .retain(|&Reverse(i)| !matches!(self.state[i], NodeState::Skipped { .. }));
        }

        skipped
    }

    /// True when every node has reached a terminal state
    /// (`Succeeded`, `Failed`, or `Skipped`). A `Ready` or `Running`
    /// node keeps the walker live.
    #[cfg(test)]
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state.iter().all(|s| {
            matches!(
                s,
                NodeState::Succeeded | NodeState::Failed | NodeState::Skipped { .. }
            )
        })
    }

    /// Snapshot per-node state keyed by id.
    #[cfg(test)]
    #[must_use]
    pub fn statuses(&self) -> BTreeMap<String, NodeState> {
        self.by_id
            .iter()
            .map(|(id, idx)| (id.clone(), self.state[*idx].clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::error::PipelineError;
    use crate::events::SkipReason;
    use crate::pipeline::schema::{Node, NodeKind, Pipeline, ShellNode};

    /// Build a pipeline of shell nodes. Each tuple is `(id, needs_ids)`.
    /// The shell command is `"true"` for every node — the walker never
    /// invokes them, so the command value is irrelevant.
    fn pipeline_with_shell_nodes(spec: &[(&str, &[&str])]) -> Pipeline {
        let nodes = spec
            .iter()
            .map(|(id, needs)| Node {
                id: (*id).to_string(),
                kind: NodeKind::Shell(ShellNode {
                    command: "true".to_string(),
                    args: Vec::new(),
                    stdin: None,
                    env: BTreeMap::new(),
                }),
                needs: needs.iter().map(|s| (*s).to_string()).collect(),
                timeout: None,
            })
            .collect();
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
    fn ties_run_in_source_order_even_when_a_completion_unblocks_an_earlier_node() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &[]), ("c", &["a"])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let mut order = Vec::new();
        while let Some(node) = walker.next_ready() {
            order.push(node.id.clone());
            walker.complete(&node.id, true);
        }
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn linear_chain_ready_order() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is ready");
        assert_eq!(first.id, "a");
        assert!(walker.complete("a", true).is_empty());

        let second = walker.next_ready().expect("b is ready");
        assert_eq!(second.id, "b");
        assert!(walker.complete("b", true).is_empty());

        let third = walker.next_ready().expect("c is ready");
        assert_eq!(third.id, "c");
        assert!(walker.complete("c", true).is_empty());

        assert!(walker.next_ready().is_none());
        assert!(walker.is_done());
    }

    #[test]
    fn diamond_converges() {
        let pipeline = pipeline_with_shell_nodes(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &["b", "c"]),
        ]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is ready");
        assert_eq!(first.id, "a");
        assert!(walker.complete("a", true).is_empty());

        let statuses = walker.statuses();
        assert!(matches!(statuses["b"], NodeState::Ready));
        assert!(matches!(statuses["c"], NodeState::Ready));

        let next_one = walker.next_ready().expect("b or c is ready").id.clone();
        let next_two = walker.next_ready().expect("b or c is ready").id.clone();
        let mut observed = [next_one, next_two];
        observed.sort();
        assert_eq!(observed, ["b".to_string(), "c".to_string()]);

        assert!(walker.complete(&observed[0], true).is_empty());
        assert!(walker.complete(&observed[1], true).is_empty());

        let fourth = walker.next_ready().expect("d is ready");
        assert_eq!(fourth.id, "d");
        assert!(walker.complete("d", true).is_empty());

        assert!(walker.next_ready().is_none());
        assert!(walker.is_done());
    }

    #[test]
    fn disjoint_nodes_both_ready() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &[])]);
        let walker = DagWalker::new(&pipeline).expect("valid DAG");

        let statuses = walker.statuses();
        assert!(matches!(statuses["a"], NodeState::Ready));
        assert!(matches!(statuses["b"], NodeState::Ready));
    }

    #[test]
    fn cycle_rejected() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &["b"]), ("b", &["a"])]);
        let Err(PipelineError::InvalidGraph { reason }) = DagWalker::new(&pipeline) else {
            panic!("expected InvalidGraph on cyclic graph");
        };
        assert!(
            reason.contains('a') || reason.contains('b'),
            "reason should name an involved node id; got {reason:?}",
        );
    }

    #[test]
    fn unknown_needs_rejected() {
        let pipeline = pipeline_with_shell_nodes(&[("b", &["nowhere"])]);
        let Err(PipelineError::InvalidGraph { reason }) = DagWalker::new(&pipeline) else {
            panic!("expected InvalidGraph on unknown needs target");
        };
        assert!(
            reason.contains("nowhere"),
            "reason should mention `nowhere`; got {reason:?}",
        );
    }

    #[test]
    fn single_level_failure_skips_direct_dependent() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &["a"])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is ready");
        assert_eq!(first.id, "a");

        let skipped = walker.complete("a", false);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "b");
        let SkipReason::DependencyFailed { upstream } = &skipped[0].1;
        assert_eq!(upstream, "a");

        assert!(walker.next_ready().is_none());
    }

    #[test]
    fn transitive_failure_skips_chain() {
        let pipeline =
            pipeline_with_shell_nodes(&[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("d", &["c"])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is ready");
        assert_eq!(first.id, "a");

        let skipped = walker.complete("a", false);
        let ids: Vec<&str> = skipped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "d"]);
        for (_, reason) in &skipped {
            let SkipReason::DependencyFailed { upstream } = reason;
            assert_eq!(upstream, "a");
        }
    }

    #[test]
    fn diamond_failure_skips_both_branches() {
        let pipeline = pipeline_with_shell_nodes(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a"]),
            ("d", &["b", "c"]),
        ]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is ready");
        assert_eq!(first.id, "a");

        let skipped = walker.complete("a", false);
        assert_eq!(skipped.len(), 3);
        let mut ids: Vec<&str> = skipped.iter().map(|(id, _)| id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["b", "c", "d"]);
        for (_, reason) in &skipped {
            let SkipReason::DependencyFailed { upstream } = reason;
            assert_eq!(upstream, "a");
        }
    }

    #[test]
    fn sibling_failure_does_not_skip_independent() {
        // `a` and `c` are independent roots; only `b` depends on `a`.
        // Failing `a` must skip `b` but leave `c` untouched. Source-order
        // tie-breaking puts `a` first (verified by
        // `source_order_preserved_among_ready`), so the drain-and-check
        // pattern here reads linearly without branching on queue order.
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &["a"]), ("c", &[])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        let first = walker.next_ready().expect("a is first ready");
        assert_eq!(first.id, "a");

        let skipped = walker.complete("a", false);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "b");
        let SkipReason::DependencyFailed { upstream } = &skipped[0].1;
        assert_eq!(upstream, "a");

        let statuses = walker.statuses();
        assert!(
            matches!(statuses["c"], NodeState::Ready),
            "independent sibling `c` must remain Ready, got {:?}",
            statuses["c"],
        );
        assert_eq!(
            walker.next_ready().expect("c is next ready").id,
            "c",
            "c must still be reachable as the next Ready node",
        );
    }

    #[test]
    fn source_order_preserved_among_ready() {
        let pipeline = pipeline_with_shell_nodes(&[("a", &[]), ("b", &[]), ("c", &[])]);
        let mut walker = DagWalker::new(&pipeline).expect("valid DAG");

        assert_eq!(walker.next_ready().expect("a").id, "a");
        assert_eq!(walker.next_ready().expect("b").id, "b");
        assert_eq!(walker.next_ready().expect("c").id, "c");
        assert!(walker.next_ready().is_none());
    }
}
