# ADR 0021 — DAG execution model: walker, context, and engine

- Status: accepted
- Date: 2026-04-21

## Context

Before Wave 5, `execution/dag.rs` held a `plan(pipeline)` function that
returned nodes in YAML source order with no cycle check, no
dependency-aware ordering, and no per-node execution state.
`Engine::run` looped over that flat list and emitted synthetic
`NodeStarted` / `NodeFinished { ok: true }` envelopes without ever
touching `NodeRegistry` or `NodeExecutor`. `docs/flows.md` flagged the
dispatch gap prominently — "`exec_sched` does not depend on `node` or
`node::registry` yet" — and the diagram's `Note` said the same.

The brainstorm run at
`.mz/reports/brainstorm_2026_04_21_orno_nodes.md` had already locked
v0.1 node kinds to `agent` and `shell` with universal cross-cutting
attributes (ADR 0017). The scheduler was the last unfinished seam
before Phase 4 (LLM transport).

Getting the scheduler out of the stub state required four things at
once:

- Real dispatch through `NodeExecutor`, so the live path exercises the
  registered executors rather than a synthetic success stream.
- A way for a failed node to cancel its downstream cone without
  aborting disjoint branches — the minimum correctness bar for a DAG
  runner.
- Per-branch context threading so a child's template sees only its own
  ancestry's outputs, mirroring the `nodes.<id>.output` contract in
  `docs/yaml-spec.md`.
- A real `ShellExecutor` so the test surface is not entirely synthetic
  and the Phase 4 agent-node work has a concrete peer to integrate
  against.

## Decision

### 1. Generator-style `DagWalker`

`crates/orno-core/src/execution/walker.rs` owns graph validation and
per-node state. Construction runs Kahn's algorithm over the
dependency graph; unknown `needs:` targets and cycles both raise
`PipelineError::InvalidGraph { reason }`. Cycle detection and
unknown-needs rejection happen in `DagWalker::new`, so the engine
never handles a mid-run structural error.

State vector is `state: Vec<NodeState>` with

```rust
enum NodeState {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped { reason: SkipReason },
}
```

`NodeState` deliberately does not derive `PartialEq` — tests use
`matches!` against the variant pattern. The enum is internal; the
walker exposes only `statuses()` as a snapshot for tests.

API is two methods:

- `next_ready() -> Option<&Node>` — pops the next `Ready` index and
  flips it to `Running`.
- `complete(id, ok) -> Vec<(String, SkipReason)>` — records the
  outcome and returns the cascade.

The complete-returns-cascade shape is load-bearing: the engine emits
`NodeSkipped` events for the cascade in causal order immediately
after the failing node's `NodeFinished { ok: false }`, without a
second traversal and without any walker-to-engine callback.

Source order is preserved among ready siblings via LIFO pop on a
reversed seed queue (`for idx in (0..n).rev()`). This is the
documented tie-breaker for independent ready nodes.

### 2. Transitive skip cascade via BFS

On `complete(id, false)`, the walker marks `id` `Failed`, then walks
the reverse adjacency (`dependents`) in BFS order. Every unfinished
transitive dependent flips to
`Skipped { reason: SkipReason::DependencyFailed { upstream } }`.

**`upstream` names the originator — the root failure — not the
skipped node's direct parent.** Reason: a consumer reading the event
stream sees one coherent causal chain. In a diamond where both middle
branches fail to matter (either could have produced the block),
pointing every skipped descendant at the single failed root is the
only attribution a user can act on; naming the direct parent forces
them to reconstruct the chain.

Disjoint subgraphs are untouched. Sibling branches from a common
ancestor keep executing if they are not in the failed cone.

The BFS returns `Vec<(node_id, SkipReason)>` in discovery order.
That is exactly the order the engine emits `NodeSkipped` events,
which gives a replay reader a single linear read of cause → effect
without post-hoc sorting.

### 3. Per-node `Context`

`crates/orno-core/src/execution/context.rs` carries three fields:

```rust
pub struct Context {
    vars: BTreeMap<String, Value>,
    env: BTreeMap<String, String>,
    nodes: BTreeMap<String, Value>,
}
```

Each node executes against a `Context` snapshot built from its
ancestry; outputs stored under `nodes.<id>` are visible only to
descendants. The v0.1 engine uses one flat `Context` that grows as
successes land, because execution is serial; the shape is already
per-branch so parallel scheduling can snapshot per-node without a
second refactor.

`env` is captured once at construction (`std::env::vars().collect()`),
not read live per node. This preserves ADR 0005's bounded-non-
determinism guarantee: a `$HOME` change mid-run cannot leak into a
child node's template context. Every node on a given run sees the
same `env` snapshot, and record/replay can reproduce it bit-for-bit.

`snapshot_for_template()` returns
`json!({ "vars": ..., "nodes": ..., "env": ... })` — the exact shape
MiniJinja sees, and the exact shape documented in `docs/yaml-spec.md`.

`Context::merge(other) -> Vec<ContextConflict>` is last-writer-wins
with an advisory conflict list. `vars` and `nodes` merge by key;
overlapping keys produce a
`ContextConflict { path, left, right }` entry and the right value
wins. `env` is intentionally **not** merged — each context's env is
its own construction-time snapshot, and folding one into another
would change the namespace out from under templates already
evaluated against the originating snapshot. Merge exists for
diamond-convergence (multiple parents of a node fold into the child's
starting context); the conflict list is the audit trail for the
shadowing.

### 4. Engine drives walker + registry serially

`Engine::new(sink, registry, templates)` takes the three
collaborators. The binary assembles them in
`crates/orno-cli/src/commands/run.rs`:

```text
loop:
  node     = walker.next_ready()?           # None → break
  emit       NodeStarted { node_id }
  (kind, req) = render_request(&node.kind, &templates, &ctx)
                                             # {{ vars.* }}, {{ nodes.<id>.* }}
  resp     = registry.get(kind_str).execute(&node.id, req).await
  ok       = resp is Ok and (shell exit_code is 0 or absent)
  if ok:    ctx.record_node_output(node_id, resp.output)
  emit       NodeFinished { ok }
  cascade  = walker.complete(node_id, ok)
  for (id, reason) in cascade: emit NodeSkipped { reason }
```

Parallelism is intentionally deferred. The walker is already
generator-shaped — multiple nodes can be `Ready` simultaneously once
their dependencies are satisfied — so upgrading to bounded-parallel
dispatch is a pure scheduler change. The walker API, `Context`, and
event schema stay as-is.

## What this is explicitly **not**

- **Not a DAG-level parallel scheduler.** Walker supports it; engine
  runs serial in v0.1 so the event stream has a single interleaving
  per run and snapshot assertions stay deterministic.
- **Not a conditional-execution mechanism.** `when: "<jinja>"`
  predicates on nodes are a future ADR (flagged in `docs/arch.md`
  deferrals) and will appear as a new `SkipReason` variant, not a
  walker redesign.
- **Not a per-node `continue_on_error` opt-out.** Today a failed
  node hard-propagates skips. The opt-out is a future attribute on
  top of ADR 0017's universal-attributes pattern.
- **Not an observability story for shell effects.** Declared-effects
  enforcement is ADR 0013's territory; the walker is oblivious to it.
- **Not a new wire-format break.** `Event` remains `#[non_exhaustive]`;
  `NodeSkipped` is an additive variant. Existing recorded replays
  stay readable.

## Consequences

- **`execution/dag.rs` is deleted.** The unused `plan()` function and
  the unused-dispatch-gap pair documented in `docs/flows.md` are both
  closed by the same commit. `exec_sched` now depends on `node`,
  `node::registry`, `execution::context`, and `execution::walker`.
  The "Note the gap" paragraph in `docs/flows.md` is obsolete.
- **New event variant** `Event::NodeSkipped { run_id, node_id, reason:
  SkipReason }` plus a new `SkipReason` enum whose only v0.1 variant
  is `DependencyFailed { upstream }`. `SkipReason` is
  `#[non_exhaustive]` so future skip classes (`ConditionFalse` for
  `when:`, `Timeout` for upstream timeouts) are additive.
- **`ShellExecutor` is real over `tokio::process::Command`.**
  Workspace `tokio` feature list gains `"process"`. Stdout, stderr,
  and exit code are captured into `nodes.<id>.{stdout,stderr,exit_code}`
  per ADR 0017 §2.
- **`PipelineError::InvalidGraph { reason: String }`** is the single
  error the walker raises during construction. Duplicate ids are
  caught upstream in `pipeline::load::validate` and reach the walker
  as a panic (validator-bug assertion), not an error variant.
- **Run identifiers** continue to come from
  `orno_core::execution::new_run_id()` (ADR 0019); the walker-driven
  engine threads the caller-provided id through every envelope
  without change.
- **Test surface**: 10 walker unit tests (linear chain, cycle,
  unknown-needs, diamond converges, single-level failure,
  transitive failure, diamond failure, disjoint nodes, sibling
  independence, source order); 7 context tests (snapshot shape,
  record, merge with conflict, merge without conflict, env snapshot
  discipline, merge skips env, from-vars exposure); 3 shell tests
  (echo stdout, non-zero exit, unknown program); 4 CLI integration
  tests (shell success, shell failure, two-node skip propagation,
  template rendering).
- **Known gap flagged for a future ADR.**
  `Event::NodeFinished { run_id, node_id, ok }` does not carry
  `stdout` / `stderr` / `exit_code`. Node output is threaded into
  `Context` for downstream templating but not re-emitted on the event
  stream. A future ADR decides whether the envelope grows output
  fields or whether consumers must snapshot the sink in-process.

## Amendments

### 2026-04-21 — superseded by ADR 0020 for `Context` env/secrets shape

§3 of this ADR describes `Context` as a three-field struct
`{ vars, env, nodes }` whose `env` map is captured at construction
via `std::env::vars().collect()`. That shape is **superseded by
ADR 0020** (_Env and secrets namespaces_). Concretely, as landed:

- `Context` has **four** fields: `vars`, `env`, `secrets`, `nodes`.
- `env` and `secrets` are **caller-provided** (via
  `RunInputs { env, secrets }` on `Engine::run`); the context
  constructor no longer reads the process environment.
- `snapshot_for_template()` exposes `env.*` and `secrets.*` as two
  disjoint top-level template namespaces.
- `merge(...)` leaves both `env` and `secrets` untouched on the
  receiver (not just `env`).

The rest of §3 — per-branch snapshots, `nodes.<id>` scoping,
last-writer-wins merge on `vars` and `nodes`, conflict-list audit
trail — carries over unchanged.

The execution flow in §4 is also updated by ADR 0020: `Engine::run`
now takes a third argument, `inputs: RunInputs`, which the CLI
resolves from `--env-file` / `--secrets-file` / `-e` flags and the
pipeline's `pass_env:` / `secrets:` declarations before the run
begins.

### 2026-04-21 — test surface counts are approximate

The Consequences bullet listing test counts (10 walker / 7 context /
3 shell / 4 CLI integration) reflects the state at ADR drafting
time. The landed suite is larger (9 context, 4 shell, 9 CLI, plus
6 scheduler-level tests and 4 pipeline-validator tests and 3 node
helper tests) because Phase 6 test review added coverage for the
missing-executor branch, the template-render-error CLI path, and
the `pass_env`/`secrets` constructor semantics from ADR 0020.
Counts are illustrative; `cargo test --workspace --all-targets` is
the source of truth.
