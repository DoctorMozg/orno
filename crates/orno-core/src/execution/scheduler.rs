//! Execution engine — walker-driven sequential DAG runner.
//!
//! `Engine::run` owns a `DagWalker` over the pipeline and a `Context`
//! seeded with `vars:` from the pipeline plus `env` and `secrets`
//! resolved by the caller (ADR 0020). It dispatches each ready node
//! through a registered `NodeExecutor`. Shell outputs are captured
//! into `Context` for downstream template rendering. Node failures
//! cascade through `DagWalker::complete` which returns the newly-
//! skipped descendants; the engine emits their `NodeSkipped`
//! events in causal order before pulling the next ready node.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;
use tracing::instrument;

use crate::error::CoreError;
use crate::events::{Event, EventSink};
use crate::execution::context::Context;
use crate::execution::walker::DagWalker;
use crate::node::{NodeRegistry, NodeResponse, kind_str, render_request};
use crate::pipeline::Pipeline;
use crate::pipeline::schema::NodeKind;
use crate::pipeline::template::TemplateEngine;

/// Inputs resolved from CLI flags + `.env` files before a run begins
/// (ADR 0020). The engine trusts these as the final values for the
/// `env.*` and `secrets.*` template namespaces; resolution
/// (precedence, dotenv parsing, classification) happens in the CLI
/// before `Engine::run` is called.
#[derive(Debug, Clone, Default)]
pub struct RunInputs {
    /// Backs the `env.*` template namespace. Not auto-inherited from
    /// the process environment; the CLI decides what to expose.
    pub env: BTreeMap<String, String>,
    /// Backs the `secrets.*` template namespace. Disjoint from `env`
    /// so a pipeline cannot resolve a secret through `env.*` and
    /// sidestep redaction.
    pub secrets: BTreeMap<String, String>,
}

/// Walker-driven DAG runner. Dispatches ready nodes through the
/// registered [`NodeExecutor`]s and emits lifecycle events via the
/// [`EventSink`]. Parallelism is deferred (ADR 0021); a single
/// `Engine` dispatches serially in YAML source order among ties.
pub struct Engine {
    sink: Arc<dyn EventSink>,
    registry: Arc<NodeRegistry>,
    templates: Arc<TemplateEngine>,
}

impl Engine {
    #[must_use]
    pub fn new(
        sink: Arc<dyn EventSink>,
        registry: Arc<NodeRegistry>,
        templates: Arc<TemplateEngine>,
    ) -> Self {
        Self {
            sink,
            registry,
            templates,
        }
    }

    /// Drive the pipeline to completion, emitting `RunStarted` →
    /// `NodeStarted` → `NodeFinished` → `NodeSkipped…` → `RunFinished`
    /// envelopes through the sink.
    ///
    /// Returns `Ok(())` even when nodes fail — **per-run success is a
    /// stream-level signal** carried by `RunFinished.ok`, never a
    /// process-level error. `Err(CoreError)` is reserved for setup
    /// failures that prevent the run from starting at all (invalid
    /// graph, walker construction error).
    #[instrument(skip(self, pipeline, inputs), fields(pipeline.run_id = %run_id))]
    pub async fn run(
        &self,
        run_id: &str,
        pipeline: &Pipeline,
        inputs: RunInputs,
    ) -> Result<(), CoreError> {
        self.sink
            .record(Event::RunStarted {
                run_id: run_id.to_string(),
            })
            .await;

        let mut walker = DagWalker::new(pipeline)?;
        let mut context = Context::new(pipeline.vars.clone(), inputs.env, inputs.secrets);
        let mut run_ok = true;

        while let Some(node) = walker.next_ready() {
            self.sink
                .record(Event::NodeStarted {
                    run_id: run_id.to_string(),
                    node_id: node.id.clone(),
                })
                .await;

            let (node_ok, node_output) = self
                .dispatch_node(run_id, node, &context, &pipeline.agents)
                .await;

            if let Some(output) = node_output {
                context.record_node_output(&node.id, output);
            }
            if !node_ok {
                run_ok = false;
            }

            let node_id = node.id.clone();
            let newly_skipped = walker.complete(&node_id, node_ok);

            self.sink
                .record(Event::NodeFinished {
                    run_id: run_id.to_string(),
                    node_id: node_id.clone(),
                    ok: node_ok,
                })
                .await;

            for (skipped_id, reason) in newly_skipped {
                self.sink
                    .record(Event::NodeSkipped {
                        run_id: run_id.to_string(),
                        node_id: skipped_id,
                        reason,
                    })
                    .await;
            }
        }

        self.sink
            .record(Event::RunFinished {
                run_id: run_id.to_string(),
                ok: run_ok,
            })
            .await;

        Ok(())
    }

    /// Dispatch a single ready node. Returns `(ok, output_on_success)`.
    /// Every failure mode (missing executor, template render error,
    /// executor error, non-zero shell exit) collapses to `ok = false`
    /// with diagnostic context on stderr via `tracing::warn!`. Only
    /// successful responses carry output back for downstream templating.
    async fn dispatch_node(
        &self,
        run_id: &str,
        node: &crate::pipeline::Node,
        context: &Context,
        agents: &BTreeMap<String, crate::pipeline::AgentConfig>,
    ) -> (bool, Option<Value>) {
        let kind = kind_str(&node.kind);
        let Some(exec) = self.registry.get(kind) else {
            tracing::warn!(
                node.id = %node.id,
                node.kind = kind,
                "no executor registered for kind",
            );
            return (false, None);
        };

        let req = match render_request(&node.kind, &self.templates, context, agents) {
            Ok(req) => req,
            Err(err) => {
                tracing::warn!(
                    node.id = %node.id,
                    node.kind = kind,
                    error = %err,
                    "render_request failed",
                );
                return (false, None);
            }
        };

        match exec.execute(run_id, &node.id, req).await {
            Ok(resp) => {
                let ok = node_response_ok(&node.kind, &resp);
                (ok, ok.then_some(resp.output))
            }
            Err(err) => {
                tracing::warn!(
                    node.id = %node.id,
                    node.kind = kind,
                    error = %err,
                    "node execution failed",
                );
                (false, None)
            }
        }
    }
}

/// Classify a successful `NodeResponse` against the originating node kind.
/// Only shell nodes treat their payload's `exit_code` as a failure signal;
/// agent and any future kinds default to success when `execute` returns `Ok`
/// so an agent payload that happens to carry an `exit_code` key is never
/// misread as a shell exit.
fn node_response_ok(kind: &NodeKind, resp: &NodeResponse) -> bool {
    match kind {
        NodeKind::Shell(_) => resp
            .output
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_none_or(|code| code == 0),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::InMemorySink;
    use crate::pipeline::schema::{AgentNode, Node, Pipeline, ShellNode};
    use serde_json::json;

    fn shell_kind() -> NodeKind {
        NodeKind::Shell(ShellNode {
            command: "echo".to_string(),
            args: Vec::new(),
        })
    }

    fn agent_kind() -> NodeKind {
        NodeKind::Agent(AgentNode {
            agent: "dummy".to_string(),
            initial_prompt: String::new(),
        })
    }

    fn resp(output: Value) -> NodeResponse {
        NodeResponse {
            node_id: "n".to_string(),
            output,
        }
    }

    /// Build a `Pipeline` carrying the given nodes and no vars/env/secrets.
    fn pipeline_of(nodes: Vec<Node>) -> Pipeline {
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

    fn shell_node(id: &str, command: &str, needs: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: command.to_string(),
                args: Vec::new(),
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn shell_zero_exit_is_ok() {
        assert!(node_response_ok(
            &shell_kind(),
            &resp(json!({"exit_code": 0, "stdout": "", "stderr": ""})),
        ));
    }

    #[test]
    fn shell_nonzero_exit_is_fail() {
        assert!(!node_response_ok(
            &shell_kind(),
            &resp(json!({"exit_code": 2, "stdout": "", "stderr": ""})),
        ));
    }

    #[test]
    fn shell_missing_exit_code_is_ok() {
        // Defensive: if a future ShellExecutor variant omits exit_code,
        // fall back to success rather than flagging the node failed.
        assert!(node_response_ok(&shell_kind(), &resp(json!({}))));
    }

    #[test]
    fn agent_payload_with_exit_code_is_still_ok() {
        // Regression guard: an agent response whose JSON payload happens
        // to contain an `exit_code` field must not be misread as failed.
        assert!(node_response_ok(
            &agent_kind(),
            &resp(json!({"exit_code": 1, "assistant": "done"})),
        ));
    }

    #[tokio::test]
    async fn missing_executor_surfaces_failure() {
        // Engine must treat a kind with no registered executor as a node
        // failure — never panic, never silently succeed. An empty
        // registry over a shell node is the shortest path to that branch.
        let pipeline = pipeline_of(vec![shell_node("lonely", "echo", &[])]);
        let sink = Arc::new(InMemorySink::new());
        let registry = Arc::new(NodeRegistry::new());
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates);

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run itself returns Ok even on node failure");

        let envelopes = sink.snapshot();
        let node_ok = envelopes.iter().find_map(|e| match &e.event {
            Event::NodeFinished { node_id, ok, .. } if node_id == "lonely" => Some(*ok),
            _ => None,
        });
        assert_eq!(
            node_ok,
            Some(false),
            "missing executor must emit NodeFinished ok:false",
        );
        let run_ok = envelopes.iter().find_map(|e| match &e.event {
            Event::RunFinished { ok, .. } => Some(*ok),
            _ => None,
        });
        assert_eq!(
            run_ok,
            Some(false),
            "aggregate run must surface ok:false when any node fails",
        );
    }

    #[tokio::test]
    async fn independent_success_does_not_rescue_run_ok() {
        // Two independent nodes, only one of which fails. The succeeding
        // sibling must still run (sibling_failure_does_not_skip_independent
        // covers the walker half of this invariant); the aggregate run
        // must report ok:false because `run_ok` is latched by any failure.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![
            shell_node(
                "fail",
                "definitely-not-a-real-program-for-tests-xyz-12345",
                &[],
            ),
            shell_node("ok", "true", &[]),
        ]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates);

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run returns Ok even on partial failure");

        let envelopes = sink.snapshot();
        let mut fail_ok = None;
        let mut ok_ok = None;
        let mut run_ok = None;
        for e in &envelopes {
            match &e.event {
                Event::NodeFinished { node_id, ok, .. } if node_id == "fail" => fail_ok = Some(*ok),
                Event::NodeFinished { node_id, ok, .. } if node_id == "ok" => ok_ok = Some(*ok),
                Event::RunFinished { ok, .. } => run_ok = Some(*ok),
                _ => {}
            }
        }
        assert_eq!(fail_ok, Some(false), "failing node must report ok:false");
        assert_eq!(
            ok_ok,
            Some(true),
            "independent `ok` node must still execute and report ok:true",
        );
        assert_eq!(
            run_ok,
            Some(false),
            "run must report ok:false when any node fails, even with a succeeding sibling",
        );
    }
}
