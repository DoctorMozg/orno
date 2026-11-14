//! Walker-driven sequential DAG runner. `Engine::run` seeds a `Context`,
//! dispatches each ready node through its `NodeExecutor` and emits the
//! `NodeSkipped` events a failure cascades to before pulling the next node.
//! Per-node dispatch lives in `dispatch`.

mod dispatch;

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::instrument;

use crate::error::CoreError;
use crate::events::{Event, EventSink};
use crate::execution::context::Context;
use crate::execution::walker::DagWalker;
use crate::node::{NodeRegistry, kind_str};
use crate::pipeline::template::TemplateEngine;
use crate::pipeline::{Node, Pipeline};

use dispatch::DispatchOutcome;

/// Inputs resolved from CLI flags + `.env` files before a run begins.
/// The engine trusts these as the final values for the
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
    /// Pipeline `vars:` already rendered by the caller. When set the
    /// engine uses them as they are; `orno replay` supplies the vars
    /// recorded in the bundle, which must not be rendered a second time.
    pub rendered_vars: Option<BTreeMap<String, serde_json::Value>>,
}

/// Run-level outcome surfaced to the caller after `Engine::run`
/// completes successfully (i.e. setup did not fail). Mirrors the
/// `RunFinished` envelope's payload so a CLI wrapper that does not
/// also subscribe to the event stream can still convert a logical
/// failure into a non-zero process exit. Setup failures still arrive
/// as `Err(CoreError)`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunOutcome {
    /// `false` when at least one node finished with `ok: false`.
    pub ok: bool,
    /// Node ids that failed, in stream-emission order. Matches
    /// `RunFinished.failed_nodes`.
    pub failed_nodes: Vec<String>,
    /// Node ids that the cascade skipped, in stream-emission order.
    /// Matches `RunFinished.skipped_nodes`.
    pub skipped_nodes: Vec<String>,
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
    /// Cap on diagnostic excerpts in failure WARNs and event-stream
    /// excerpt fields (`LlmRequestStarted.prompt_excerpt`,
    /// `McpToolCallSent.input_excerpt`, etc.). Default 2 KB — small
    /// because excerpts are meant to fit in a log line. The captured
    /// payload that flows into `Context` is bounded separately by
    /// `max_node_output_bytes`.
    pub max_output_bytes: usize,
    /// Cap on a `kind: shell` node's captured `stdout` or `stderr`,
    /// per stream. The shell executor streams its child's pipes and
    /// stops appending once a stream reaches this cap, then continues
    /// draining the pipe to /dev/null so the child does not block on a
    /// full PIPE buffer. When a stream is truncated, an
    /// [`Event::NodeOutputTruncated`] envelope is emitted with the
    /// `total_bytes` count. Default 8 MiB — large enough for typical
    /// build output, small enough to keep a single hostile node from
    /// exhausting RAM. Distinct from `max_output_bytes` (which caps
    /// log-line excerpts at 2 KB).
    pub max_node_output_bytes: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            max_output_bytes: 2048,
            max_node_output_bytes: 8 * 1024 * 1024,
        }
    }
}

/// Walker-driven DAG runner. Dispatches ready nodes through the
/// registered [`crate::node::NodeExecutor`]s and emits lifecycle events via the
/// [`EventSink`]. Dispatch is sequential; a single `Engine` runs
/// nodes serially in YAML source order among ties.
pub struct Engine {
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
    /// Returns `Ok(RunOutcome)` whenever the engine finishes the
    /// dispatch loop — regardless of whether any node succeeded.
    /// `RunOutcome.ok` is `false` exactly when at least one node
    /// finished with `ok: false`; CLI callers convert that into a
    /// non-zero process exit so `orno run | …` behaves like the rest
    /// of the unix toolchain. `Err(CoreError)` is reserved for setup
    /// failures that prevent the run from starting at all (invalid
    /// graph, walker construction error).
    #[instrument(skip(self, pipeline, inputs), fields(pipeline.run_id = %run_id))]
    pub async fn run(
        &self,
        run_id: &str,
        pipeline: &Pipeline,
        mut inputs: RunInputs,
    ) -> Result<RunOutcome, CoreError> {
        self.sink
            .record(Event::RunStarted {
                run_id: run_id.to_string(),
            })
            .await;

        let mut walker = match DagWalker::new(pipeline) {
            Ok(w) => w,
            Err(err) => {
                // No node ever runs, so the per-node failure path never
                // logs this; give log pipelines a hook with the run span.
                tracing::warn!(error = %err, "walker construction failed");
                self.finish_run(run_id, &RunOutcome::aborted()).await;
                return Err(err.into());
            },
        };

        // Render `vars` against `{ env, secrets }` once up front; MiniJinja
        // renders a template in a single pass, so a `{{ env.TAG }}` left in
        // `vars.tag` would reach downstream nodes as literal source.
        let rendered_vars = match inputs.rendered_vars.take() {
            Some(vars) => vars,
            None => match crate::pipeline::template::render_pipeline_vars(
                &self.templates,
                &pipeline.vars,
                &inputs.env,
                &inputs.secrets,
            ) {
                Ok(v) => v,
                Err(err) => {
                    self.finish_run(run_id, &RunOutcome::aborted()).await;
                    return Err(err.into());
                },
            },
        };

        // Built before `Context::new` consumes `inputs.secrets`; every
        // user-visible string that reaches the event stream goes through it.
        let redactor = crate::events::Redactor::new(&inputs.secrets);
        let mut run = RunState {
            run_id,
            pipeline,
            redactor,
            context: Context::new(rendered_vars, inputs.env, inputs.secrets),
            kind_by_id: pipeline
                .nodes
                .iter()
                .map(|n| (n.id.as_str(), kind_str(&n.kind)))
                .collect(),
            outcome: RunOutcome {
                ok: true,
                failed_nodes: Vec::new(),
                skipped_nodes: Vec::new(),
            },
        };

        while let Some(node) = walker.next_ready() {
            self.run_node(&mut run, &mut walker, node).await;
        }

        self.finish_run(run_id, &run.outcome).await;
        Ok(run.outcome)
    }

    /// Dispatch one ready node, fold its result into the run state and
    /// emit `NodeSkipped` for every node the failure cascades to.
    async fn run_node(&self, run: &mut RunState<'_>, walker: &mut DagWalker<'_>, node: &Node) {
        let run_id = run.run_id;
        let node_kind = kind_str(&node.kind);
        self.sink
            .record(Event::NodeStarted {
                run_id: run_id.to_string(),
                node_id: node.id.clone(),
                node_kind: node_kind.to_string(),
            })
            .await;

        let DispatchOutcome {
            ok,
            failure,
            output,
        } = self.dispatch_node(run, node).await;

        // Recorded on failure too: a failed shell node's unbounded
        // `stderr` / `exit_code` stay readable to recovery branches, while
        // the wire `NodeFailure` carries only a bounded tail.
        if let Some(mut output) = output {
            run.redactor.redact_json(&mut output);
            run.context.record_node_output(&node.id, output);
        }
        if !ok {
            run.outcome.ok = false;
            run.outcome.failed_nodes.push(node.id.clone());
        }

        self.sink
            .record(Event::NodeFinished {
                run_id: run_id.to_string(),
                node_id: node.id.clone(),
                node_kind: node_kind.to_string(),
                ok,
                failure,
            })
            .await;

        for (skipped_id, reason) in walker.complete(&node.id, ok) {
            let skipped_kind = run
                .kind_by_id
                .get(skipped_id.as_str())
                .expect("walker only reports ids from the pipeline it was built over");
            run.outcome.skipped_nodes.push(skipped_id.clone());
            self.sink
                .record(Event::NodeSkipped {
                    run_id: run_id.to_string(),
                    node_id: skipped_id,
                    node_kind: (*skipped_kind).to_string(),
                    reason,
                })
                .await;
        }
    }

    /// The wire envelope and the returned [`RunOutcome`] carry the same
    /// aggregates, so a tail-line read agrees with the process exit.
    async fn finish_run(&self, run_id: &str, outcome: &RunOutcome) {
        self.sink
            .record(Event::RunFinished {
                run_id: run_id.to_string(),
                ok: outcome.ok,
                failed_nodes: outcome.failed_nodes.clone(),
                skipped_nodes: outcome.skipped_nodes.clone(),
            })
            .await;
    }
}

impl RunOutcome {
    /// Outcome of a run that failed before any node dispatched.
    fn aborted() -> Self {
        Self {
            ok: false,
            failed_nodes: Vec::new(),
            skipped_nodes: Vec::new(),
        }
    }
}

/// Mutable state threaded through the per-node steps of one run.
struct RunState<'a> {
    run_id: &'a str,
    pipeline: &'a Pipeline,
    redactor: crate::events::Redactor,
    context: Context,
    // Skip events only know the skipped node's id; indexing once avoids a
    // linear scan of `pipeline.nodes` per skip on wide DAGs.
    kind_by_id: BTreeMap<&'a str, &'static str>,
    outcome: RunOutcome,
}

#[cfg(test)]
mod tests;
