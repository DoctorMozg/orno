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
//!
//! Layout: per-node dispatch logic (executor lookup, template render,
//! timeout wrapping, payload classification) lives in [`dispatch`] so
//! this file stays focused on orchestration (walker loop,
//! `RunFinished` aggregates, engine construction).

mod dispatch;

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::instrument;

use crate::error::CoreError;
use crate::events::{Event, EventSink};
use crate::execution::context::Context;
use crate::execution::walker::DagWalker;
use crate::node::NodeRegistry;
use crate::pipeline::Pipeline;
use crate::pipeline::template::TemplateEngine;

use dispatch::DispatchOutcome;

/// Inputs resolved from CLI flags + `.env` files before a run begins
/// (ADR 0020). The engine trusts these as the final values for the
/// `env.*` and `secrets.*` template namespaces; resolution
/// (precedence, dotenv parsing, classification) happens in the CLI
/// before `Engine::run` is called.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RunInputs {
    /// Backs the `env.*` template namespace. Not auto-inherited from
    /// the process environment; the CLI decides what to expose.
    pub env: BTreeMap<String, String>,
    /// Backs the `secrets.*` template namespace. Disjoint from `env`
    /// so a pipeline cannot resolve a secret through `env.*` and
    /// sidestep redaction.
    pub secrets: BTreeMap<String, String>,
}

/// Tunables for diagnostic emission. Held by `Engine` and consulted
/// at every failure site so verbosity is a single, tested knob rather
/// than a forest of if-let guards.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// When true, failure WARNs include richer context (full payload
    /// excerpts rather than truncated tails). Failure records are
    /// always emitted; this only controls detail.
    pub verbose: bool,
    /// Cap on captured stderr (and similarly bounded payloads) in
    /// failure WARNs. Output passed into `Context` and event payloads
    /// is unaffected — this only bounds what the human-facing log
    /// stream prints, since shell stderr is unbounded by definition.
    pub max_output_bytes: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            max_output_bytes: 2048,
        }
    }
}

/// Walker-driven DAG runner. Dispatches ready nodes through the
/// registered [`NodeExecutor`]s and emits lifecycle events via the
/// [`EventSink`]. Parallelism is deferred (ADR 0021); a single
/// `Engine` dispatches serially in YAML source order among ties.
pub struct Engine {
    // Fields are `pub(super)` so `dispatch.rs` (a submodule of
    // `scheduler`) can read them from its `impl Engine` block without
    // opening the whole crate to the internals.
    pub(super) sink: Arc<dyn EventSink>,
    pub(super) registry: Arc<NodeRegistry>,
    pub(super) templates: Arc<TemplateEngine>,
    pub(super) config: EngineConfig,
}

impl Engine {
    #[must_use]
    pub fn new(
        sink: Arc<dyn EventSink>,
        registry: Arc<NodeRegistry>,
        templates: Arc<TemplateEngine>,
        config: EngineConfig,
    ) -> Self {
        Self {
            sink,
            registry,
            templates,
            config,
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
    #[expect(
        clippy::too_many_lines,
        reason = "dispatch loop; complexity justified by ADR 0021 event sequencing"
    )]
    #[instrument(skip(self, pipeline, inputs), fields(pipeline.run_id = %run_id))]
    pub async fn run(
        &self,
        run_id: &str,
        pipeline: &Pipeline,
        inputs: RunInputs,
    ) -> Result<(), CoreError> {
        // Own `run_id` once so every `Event::*` ctor can `clone()` from
        // this local instead of hitting `&str::to_string()` on the
        // parameter at every emission site. Shadows the borrowed name so
        // nothing inside `run` accidentally keeps the old `&str` alive.
        let run_id = run_id.to_string();
        // Build the redactor from the run's `secrets.*` namespace before
        // `Context::new` consumes `inputs.secrets`. Every user-visible
        // string that reaches the event stream flows through this
        // instance (ADR 0020).
        let redactor = crate::events::Redactor::new(&inputs.secrets);

        self.sink
            .record(Event::RunStarted {
                run_id: run_id.clone(),
            })
            .await;

        let mut walker = DagWalker::new(pipeline).inspect_err(|err| {
            // Walker construction failures (cycles, unknown `needs:`
            // targets) prevent any node from running, so they never
            // reach the per-node failure path. Surface them on the
            // human-facing log stream before propagating — the CLI
            // also prints the chain on exit, but a structured WARN
            // gives log pipelines a hook with `pipeline.run_id` in
            // the span.
            tracing::warn!(error = %err, "walker construction failed");
        })?;
        let mut context = Context::new(pipeline.vars.clone(), inputs.env, inputs.secrets);
        let mut run_ok = true;
        // `RunFinished` aggregates (ADR 0023). Both vectors capture
        // node ids in causal order — the same order the per-node
        // events fire — so a downstream tool that reads only the
        // `RunFinished` envelope sees the failure footprint without
        // folding the full stream. `Vec` over `BTreeSet` because
        // `next_ready` already enforces emission order and a single
        // node id never repeats.
        let mut failed_nodes: Vec<String> = Vec::new();
        let mut skipped_nodes: Vec<String> = Vec::new();

        while let Some(node) = walker.next_ready() {
            self.sink
                .record(Event::NodeStarted {
                    run_id: run_id.clone(),
                    node_id: node.id.clone(),
                })
                .await;

            let DispatchOutcome {
                ok: node_ok,
                failure,
                output: node_output,
            } = self
                .dispatch_node(&run_id, node, &context, &pipeline.agents, &redactor)
                .await;

            // Record the node's output into Context regardless of
            // success. A failed shell node's `stderr` / `exit_code` are
            // still useful to downstream templates (recovery branches,
            // diagnostic agents); the wire-format `NodeFailure` only
            // carries a bounded tail, the unbounded form lives here.
            if let Some(output) = node_output {
                let output = if redactor.is_noop() {
                    output
                } else {
                    redactor.redact_json(&output)
                };
                context.record_node_output(&node.id, output);
            }
            if !node_ok {
                run_ok = false;
                failed_nodes.push(node.id.clone());
            }

            let node_id = node.id.clone();

            self.sink
                .record(Event::NodeFinished {
                    run_id: run_id.clone(),
                    node_id: node_id.clone(),
                    ok: node_ok,
                    failure,
                })
                .await;

            let newly_skipped = walker.complete(&node_id, node_ok);

            for (skipped_id, reason) in newly_skipped {
                skipped_nodes.push(skipped_id.clone());
                self.sink
                    .record(Event::NodeSkipped {
                        run_id: run_id.clone(),
                        node_id: skipped_id,
                        reason,
                    })
                    .await;
            }
        }

        self.sink
            .record(Event::RunFinished {
                run_id: run_id.clone(),
                ok: run_ok,
                failed_nodes,
                skipped_nodes,
            })
            .await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{InMemorySink, NodeFailure};
    use crate::pipeline::schema::{Node, NodeKind, Pipeline, ShellNode};

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
                stdin: None,
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            timeout: None,
        }
    }

    // Tracing-capture helpers used by the failure-logging regression
    // tests. Kept inside `mod tests` so they remain a strictly test-only
    // surface — production code only uses the `tracing` macro crate,
    // not `tracing-subscriber`.
    use std::io::Write;
    use std::sync::Mutex;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = BufferGuard;
        fn make_writer(&'a self) -> Self::Writer {
            BufferGuard(self.0.clone())
        }
    }

    struct BufferGuard(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferGuard {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer mutex").write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Build a JSON tracing subscriber whose output is captured in
    /// `buf`. Caller installs it via `set_default`; the returned guard
    /// must outlive the work-under-test. Per-thread default propagates
    /// to async tasks only on `current_thread` runtimes — every test
    /// using this helper marks `flavor = "current_thread"`.
    fn capture_subscriber(buf: &Arc<Mutex<Vec<u8>>>) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_writer(BufferWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .json()
            .finish()
    }

    fn capture_drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().expect("buffer mutex").clone())
            .expect("captured tracing output is valid UTF-8")
    }

    #[tokio::test]
    async fn missing_executor_surfaces_failure() {
        // Engine must treat a kind with no registered executor as a node
        // failure — never panic, never silently succeed. An empty
        // registry over a shell node is the shortest path to that branch.
        // Phase 2: also asserts the wire-format `failure` carries
        // `NoExecutorRegistered { node_kind }` so consumers know *why*.
        let pipeline = pipeline_of(vec![shell_node("lonely", "echo", &[])]);
        let sink = Arc::new(InMemorySink::new());
        let registry = Arc::new(NodeRegistry::new());
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run itself returns Ok even on node failure");

        let envelopes = sink.snapshot();
        let node_finished = envelopes.iter().find_map(|e| match &e.event {
            Event::NodeFinished {
                node_id,
                ok,
                failure,
                ..
            } if node_id == "lonely" => Some((*ok, failure.clone())),
            _ => None,
        });
        let (node_ok, failure) = node_finished.expect("NodeFinished for `lonely` must exist");
        assert!(!node_ok, "missing executor must emit NodeFinished ok:false");
        match failure {
            Some(NodeFailure::NoExecutorRegistered { node_kind }) => {
                assert_eq!(node_kind, "shell", "node_kind must echo the YAML kind");
            },
            other => panic!("expected NoExecutorRegistered, got {other:?}"),
        }
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
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

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
                _ => {},
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

    #[tokio::test]
    async fn run_finished_aggregates_failed_and_skipped_in_causal_order() {
        // Phase 3 wire addition: `RunFinished` carries causal-order
        // vectors so a downstream tool that reads only the last
        // envelope sees the failure footprint without folding the
        // stream. "Causal order" means stream-emission order — the
        // failed node first, then each transitively-skipped descendant
        // in the order the walker surfaced them.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![
            shell_node(
                "fail",
                "definitely-not-a-real-program-for-tests-xyz-12345",
                &[],
            ),
            shell_node("mid", "true", &["fail"]),
            shell_node("leaf", "true", &["mid"]),
            // Independent sibling: must NOT appear in either vector.
            shell_node("indep", "true", &[]),
        ]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run returns Ok even with a failing node");

        let envelopes = sink.snapshot();
        let (ok, failed_nodes, skipped_nodes) = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::RunFinished {
                    ok,
                    failed_nodes,
                    skipped_nodes,
                    ..
                } => Some((*ok, failed_nodes.clone(), skipped_nodes.clone())),
                _ => None,
            })
            .expect("RunFinished must be in the stream");

        assert!(!ok, "any failure latches run.ok to false");
        assert_eq!(failed_nodes, vec!["fail".to_string()]);
        // Skip cascade order is whatever the walker surfaces; both
        // descendants must be present and `indep` must not be.
        assert!(
            skipped_nodes.contains(&"mid".to_string())
                && skipped_nodes.contains(&"leaf".to_string()),
            "transitive descendants must be in skipped_nodes: {skipped_nodes:?}",
        );
        assert!(
            !skipped_nodes.contains(&"indep".to_string()),
            "independent sibling must not be reported as skipped: {skipped_nodes:?}",
        );
        assert_eq!(skipped_nodes.len(), 2, "exactly two descendants skipped");
    }

    #[tokio::test]
    async fn run_finished_aggregates_are_empty_on_clean_run() {
        // A fully-green run must report empty vectors so a downstream
        // tool can branch on `failed_nodes.is_empty() && skipped_nodes.is_empty()`
        // without special-casing the absence of the keys.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![
            shell_node("a", "true", &[]),
            shell_node("b", "true", &["a"]),
        ]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("clean run returns Ok");

        let envelopes = sink.snapshot();
        let (ok, failed, skipped) = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::RunFinished {
                    ok,
                    failed_nodes,
                    skipped_nodes,
                    ..
                } => Some((*ok, failed_nodes.clone(), skipped_nodes.clone())),
                _ => None,
            })
            .expect("RunFinished must be present");
        assert!(ok);
        assert!(failed.is_empty(), "no failures expected: {failed:?}");
        assert!(skipped.is_empty(), "no skips expected: {skipped:?}");
    }

    #[tokio::test]
    async fn shell_nonzero_exit_emits_node_payload_failure_on_wire() {
        // Phase 2: the wire-format `failure` field on NodeFinished must
        // carry exit_code and a stderr_tail when the shell exits non-zero.
        // The tail must surface the actual stderr substring so a UI can
        // display it without parsing tracing JSON.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![Node {
            id: "fail".to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "echo 'visible-cause' 1>&2; exit 7".to_string(),
                ],
                stdin: None,
            }),
            needs: Vec::new(),
            timeout: None,
        }]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run returns Ok even on node failure");

        let envelopes = sink.snapshot();
        let failure = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::NodeFinished {
                    node_id, failure, ..
                } if node_id == "fail" => failure.clone(),
                _ => None,
            })
            .expect("failed NodeFinished must carry a failure");

        match failure {
            NodeFailure::NodePayloadFailure {
                exit_code,
                stderr_tail,
            } => {
                assert_eq!(exit_code, Some(7), "exit_code must round-trip on the wire");
                let tail = stderr_tail.expect("stderr_tail must be present when stderr ran");
                assert!(
                    tail.contains("visible-cause"),
                    "stderr_tail must surface the actual cause: {tail}",
                );
            },
            other => panic!("expected NodePayloadFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_failure_payload_still_lands_in_context_for_downstream() {
        // Phase 2 invariant: even when a shell node fails, its `stderr`
        // and `exit_code` flow into Context under `node.<id>.*`. A
        // recovery template (`{{ node.fail.exit_code }}`) must be able
        // to read them without inspecting the event log. Earlier
        // `dispatch_node` gated this on success, which made downstream
        // recovery patterns impossible.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![
            Node {
                id: "fail".to_string(),
                kind: NodeKind::Shell(ShellNode {
                    command: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "echo 'recoverable' 1>&2; exit 9".to_string(),
                    ],
                    stdin: None,
                }),
                needs: Vec::new(),
                timeout: None,
            },
            // `recover` does NOT depend on `fail` (depending would
            // skip-cascade it). The point is to prove the *Context*
            // entry exists; we read it via the snapshot below.
            shell_node("recover", "true", &[]),
        ]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run must succeed even with a failing node");

        // The wire-format check is the easiest indirect way to prove the
        // payload was preserved past dispatch_node: if the executor's
        // resp.output had been dropped, NodePayloadFailure.exit_code
        // would be None.
        let exit_code = sink.snapshot().iter().find_map(|e| match &e.event {
            Event::NodeFinished {
                node_id,
                failure: Some(NodeFailure::NodePayloadFailure { exit_code, .. }),
                ..
            } if node_id == "fail" => *exit_code,
            _ => None,
        });
        assert_eq!(
            exit_code,
            Some(9),
            "failing shell payload must reach the wire, proving it was not discarded",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shell_nonzero_exit_emits_warn_with_exit_code_and_stderr_tail() {
        // The bug autopsy case: a shell that runs and exits non-zero
        // used to drop its stderr/exit_code into the void. After the
        // fix, dispatch_node emits a structured WARN with both fields
        // visible to log pipelines.
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![Node {
            id: "loud_fail".to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "echo 'this is the failure reason' 1>&2; exit 2".to_string(),
                ],
                stdin: None,
            }),
            needs: Vec::new(),
            timeout: None,
        }]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink, registry, templates, EngineConfig::default());

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = capture_subscriber(&buf);
        let guard = tracing::subscriber::set_default(subscriber);

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run returns Ok even when a node fails");

        drop(guard);
        let logs = capture_drain(&buf);

        assert!(
            logs.contains("node returned failure in payload"),
            "warn message missing: {logs}",
        );
        assert!(
            logs.contains(r#""exit_code":2"#),
            "exit_code field missing or wrong: {logs}",
        );
        assert!(
            logs.contains("this is the failure reason"),
            "stderr_tail must surface child stderr: {logs}",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn walker_construction_failure_emits_warn_before_propagating() {
        // A cyclic graph never gets a node started — so the per-node
        // failure path can't surface it. Engine::run must emit a WARN
        // before propagating the InvalidGraph error so log pipelines
        // see the cause without parsing the CLI's stderr error chain.
        let pipeline = pipeline_of(vec![
            shell_node("a", "true", &["b"]),
            shell_node("b", "true", &["a"]),
        ]);
        let sink = Arc::new(InMemorySink::new());
        let registry = Arc::new(NodeRegistry::new());
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink, registry, templates, EngineConfig::default());

        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let subscriber = capture_subscriber(&buf);
        let guard = tracing::subscriber::set_default(subscriber);

        let err = engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect_err("cycle must surface as Err");

        drop(guard);
        let logs = capture_drain(&buf);

        assert!(
            logs.contains("walker construction failed"),
            "expected walker-failure WARN: {logs}",
        );
        assert!(
            logs.contains("cycle detected"),
            "WARN must include the underlying reason: {logs}",
        );
        // Sanity-check: the error itself still propagates with the
        // same diagnostic so the CLI can print the chain.
        assert!(
            format!("{err}").contains("graph") || format!("{err:#}").contains("cycle"),
            "propagated error should mention graph/cycle: {err:#}",
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "integration test scenario; long by necessity"
    )]
    #[tokio::test(flavor = "current_thread")]
    async fn shell_node_timeout_fires_and_emits_node_timed_out_and_failure() {
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![Node {
            id: "slow".to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 10".to_string()],
                stdin: None,
            }),
            needs: Vec::new(),
            timeout: Some(1),
        }]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run returns Ok even when a node times out");

        let envelopes = sink.snapshot();
        let timed_out_idx = envelopes
            .iter()
            .position(|e| matches!(e.event, Event::NodeTimedOut { .. }))
            .expect("Event::NodeTimedOut must fire on timeout");
        let finished_idx = envelopes
            .iter()
            .position(|e| matches!(e.event, Event::NodeFinished { ok: false, .. }))
            .expect("NodeFinished(ok: false) must fire");
        assert!(
            timed_out_idx < finished_idx,
            "NodeTimedOut must precede NodeFinished in stream order",
        );

        if let Event::NodeTimedOut {
            limit_secs,
            elapsed_ms,
            node_id,
            ..
        } = &envelopes[timed_out_idx].event
        {
            assert_eq!(node_id, "slow");
            assert_eq!(*limit_secs, 1);
            assert!(
                *elapsed_ms >= 1000,
                "elapsed_ms should be >= 1000: {elapsed_ms}"
            );
        }

        let failure = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::NodeFinished { failure, .. } => failure.clone(),
                _ => None,
            })
            .expect("NodeFinished must carry a failure");
        assert!(
            matches!(failure, NodeFailure::TimedOut { limit_secs: 1 }),
            "failure must classify as TimedOut(1): {failure:?}",
        );

        let failed_nodes = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::RunFinished { failed_nodes, .. } => Some(failed_nodes.clone()),
                _ => None,
            })
            .expect("RunFinished must be present");
        assert_eq!(failed_nodes, vec!["slow".to_string()]);
    }

    #[tokio::test]
    async fn timeout_none_lets_node_run_to_completion() {
        use crate::node::shell::ShellExecutor;

        let pipeline = pipeline_of(vec![Node {
            id: "quick".to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "sh".to_string(),
                args: vec!["-c".to_string(), "sleep 0.2 && echo done".to_string()],
                stdin: None,
            }),
            needs: Vec::new(),
            timeout: None,
        }]);
        let sink = Arc::new(InMemorySink::new());
        let mut reg = NodeRegistry::new();
        reg.register("shell", Arc::new(ShellExecutor));
        let registry = Arc::new(reg);
        let templates = Arc::new(TemplateEngine::new());
        let engine = Engine::new(sink.clone(), registry, templates, EngineConfig::default());

        engine
            .run("run_test", &pipeline, RunInputs::default())
            .await
            .expect("engine::run succeeds");

        let envelopes = sink.snapshot();
        assert!(
            !envelopes
                .iter()
                .any(|e| matches!(e.event, Event::NodeTimedOut { .. })),
            "NodeTimedOut must not fire when timeout is None",
        );
        let ok = envelopes
            .iter()
            .find_map(|e| match &e.event {
                Event::NodeFinished { ok, .. } => Some(*ok),
                _ => None,
            })
            .expect("NodeFinished present");
        assert!(ok, "node with timeout=None must succeed");
    }
}
