# ADR 0023 — Typed `LlmRequestFailed` and `RunFinished` aggregates

- Status: accepted
- Date: 2026-04-22
- Phase: 3 of the failure-surfacing initiative (continues ADR 0022)

## Context

ADR 0022 closed the silent-failure root cause for *node*-level failures
by adding `failure: Option<NodeFailure>` to `Event::NodeFinished` and
forcing every failure path through a structured WARN. Two gaps remained,
explicitly deferred to Phase 3:

1. **Dangling `LlmRequestStarted`.** The agent executor emits
   `LlmRequestStarted` immediately before the transport call, then
   maps the transport's `Err(LlmError)` straight into
   `NodeError::Execution`. The error eventually surfaces as
   `NodeFailure::ExecutorError { error: "<rendered chain>" }` on the
   downstream `NodeFinished`. From the wire-format consumer's
   perspective:

   - `LlmRequestStarted` fires.
   - No `LlmResponseReceived`, no `LlmRequestFailed`. Just an absence.
   - Some envelopes later, a generic `ExecutorError` blob with a
     human-readable error chain.

   Log pipelines that want to page on `auth_failed` or `rate_limited`
   separately from generic node failures cannot — the only signal is a
   regex against the rendered `Display` chain on `ExecutorError.error`,
   which is not a contract.

2. **No tail-line summary on `RunFinished`.** A consumer that only reads
   the last envelope of a stream knows whether the run succeeded
   (`ok: bool`) but not which nodes failed or which were skipped. To
   answer "what is broken in this pipeline" the consumer must fold the
   full envelope log, classifying every `NodeFinished` and `NodeSkipped`
   on the way. Cheap aggregates on `RunFinished` collapse this folding
   into one envelope read.

Both gaps are wire-format additions on `#[non_exhaustive]` enums, so
they ride the existing `EventEnvelope` versioning without a schema
bump (ADR 0018, ADR 0021).

## Decision

### 1. Wire format adds `Event::LlmRequestFailed`

```rust
LlmRequestFailed {
    run_id: String,
    node_id: String,
    provider: String,
    model: String,
    failure: LlmFailure,
}
```

The shape mirrors `LlmRequestStarted` plus a typed `failure` field, so
a consumer that already filters on `LlmRequestStarted { provider,
model }` can pair the two with the same logic. `provider` and `model`
are duplicated on the failed event — the `Started` / `Failed` /
`ResponseReceived` triple is read independently in most pipelines, and
forcing a join on `(run_id, node_id)` to recover provider/model on
the failed branch is friction without benefit.

### 2. `LlmFailure` is a typed mirror of `LlmError`

```rust
#[non_exhaustive]
pub enum LlmFailure {
    AuthFailed,
    RateLimited,
    ModelNotFound,
    ApiError { status: u16, body_excerpt: String },
    Transport { error: String },
    ConfigError { message: String },
    ParseError { message: String },
    ReplayMiss { key: String },
    Other { message: String },
}
```

Variants follow the same naming as `crate::error::LlmError` so the
mapping stays mechanical. Three design points:

- **Provider is not duplicated inside `LlmFailure`.** The parent event
  already carries `provider` and `model`. Repeating them inside the
  variant body bloats the wire format and creates two sources of truth
  for the same fact.
- **`ApiError.body_excerpt` is bounded.** HTTP error bodies from
  providers can be megabytes (think a 502 returning a full HTML error
  page). The bound is the same `EngineConfig.max_output_bytes` cap
  used for shell stderr tails, so a log reader sees consistent
  truncation across both surfaces. Truncation keeps the **head** of the
  body (opposite of stderr tails), because HTTP error bodies put the
  actionable signal at the front (status text, JSON `error.message`).
- **`Other { message }` is the catch-all** for `LlmError::Rejected`,
  `LlmError::NotImplemented`, and any future `#[non_exhaustive]`
  additions that have not yet earned a typed variant. Renders the
  full error chain so a downstream operator still sees the cause —
  `Other` degrades to a string but does not drop the signal.

The classifier is a `LlmFailure::from_llm_error(&LlmError, body_excerpt_max_bytes: usize)`
constructor next to the type. Lives in `events/mod.rs` rather than
`error.rs` because the concern is wire-format projection, not error
construction; `error.rs` stays free of `serde` / wire-shape coupling.

### 3. Agent executor classifies before propagating

Old code:

```rust
let response = self
    .transport
    .complete(llm_req)
    .await
    .map_err(|e| llm_error_to_node(node_id, e))?;
```

New code:

```rust
let response = match self.transport.complete(llm_req).await {
    Ok(resp) => resp,
    Err(err) => {
        let failure = LlmFailure::from_llm_error(&err, self.body_excerpt_max_bytes);
        self.sink.record(Event::LlmRequestFailed {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            provider: provider.clone(),
            model: model.clone(),
            failure,
        }).await;
        return Err(llm_error_to_node(node_id, err));
    }
};
```

The mapping into `NodeError::Execution` is unchanged — the typed
event lands *next to* the dangling `LlmRequestStarted`, the existing
`NodeFinished { failure: ExecutorError }` still fires from the
scheduler. Two surfaces, two audiences:

- `LlmRequestFailed` → log pipelines that page on transport-class
  failures (`auth_failed`, `rate_limited`) without folding node-level
  events.
- `NodeFinished.failure: ExecutorError` → tools that already match
  `NodeFinished` for run-level reporting, regardless of which subsystem
  produced the error.

### 4. `AgentExecutor::new` arity grew by one

`AgentExecutor::new(transport, sink, body_excerpt_max_bytes)` carries
the cap explicitly. The CLI threads `engine_config.max_output_bytes`
through so a single `--stderr-tail-bytes` flag governs both shell
stderr tails and LLM body excerpts.

For embedders that build the executor without an `EngineConfig`, an
`AgentExecutor::with_defaults(transport, sink)` convenience constructor
applies the same default (`2048 bytes`) the engine ships with. The
default constant lives next to the executor as
`DEFAULT_BODY_EXCERPT_BYTES` so an embedder reading the source sees
the policy without chasing it through `EngineConfig::default()`.

### 5. `Event::RunFinished` grows `failed_nodes` and `skipped_nodes`

```rust
RunFinished {
    run_id: String,
    ok: bool,
    failed_nodes: Vec<String>,
    skipped_nodes: Vec<String>,
}
```

Both vectors are populated in **causal order** — the same order the
per-node `NodeFinished` and `NodeSkipped` envelopes were emitted.
Causal order beats alphabetical because:

- It matches the order a user scrolling through stderr sees.
- The first failure is usually the originator; later failures are
  often consequences. Sorting alphabetically would scramble that.
- A downstream tool that wants alphabetical order can sort cheaply;
  reconstructing causal order from a sorted vector is impossible
  without rejoining against the per-node events.

Empty vectors on a clean run are a documented invariant — consumers
can branch on `failed_nodes.is_empty() && skipped_nodes.is_empty()`
without special-casing the absence of the keys. `Vec` over `BTreeSet`
because the walker already guarantees no duplicates and `Vec` keeps
order; `BTreeSet` would lose causal order with no benefit.

`ok: bool` stays alongside the new fields rather than being collapsed
into `failed_nodes.is_empty()`. Same back-compat reasoning as ADR 0022:
every existing destructure of `RunFinished` either uses `..` (tolerates
the new fields) or recompiles cleanly.

### 6. No `init_tracing` / CLI flag changes

Phase 3 is a pure wire-format addition. The verbose flag still
controls failure-WARN detail; nothing on the stderr side grows.

## What this is explicitly **not**

- **Not a `LlmResponseSucceeded` rename.** `LlmResponseReceived`
  remains the success event. Adding a `Failed` cousin is the
  consistent split — `Started` / `Failed` / `ResponseReceived` —
  rather than overloading the success event with an `Option<failure>`.
- **Not a per-iteration loop event.** Iteration-level events
  (`AgentIterationStarted`, `ToolCallRecorded`) land with the agent
  loop in Phase 5; this ADR only covers the single-shot transport
  path.
- **Not a `Run` aggregate of HTTP failures.** `failed_nodes` lists
  *node ids*, not transport failure classes. A consumer that wants
  "which providers failed today" still folds `LlmRequestFailed`
  events.
- **Not a record/replay extension.** Replay tapes recorded before
  Phase 3 stay readable because:
  - `LlmRequestFailed` is a brand-new variant on a `#[non_exhaustive]`
    enum — old envelopes simply do not contain it.
  - `failed_nodes` / `skipped_nodes` deserialize to `Vec::new()` via
    `#[serde(default)]` semantics (the fields are added, but the
    `RunFinished` variant already existed; old envelopes will be
    rejected unless the deserializer tolerates missing fields).
    **Implementation note:** if a future replay tooling reads pre-Phase-3
    envelopes, the `RunFinished` variant will need `#[serde(default)]`
    on the two new fields; v0.1 does not yet replay so this is a
    deferred constraint, not a regression.

## Consequences

- **Wire format addition.** `Event` grows `LlmRequestFailed`;
  `Event::RunFinished` grows two `Vec<String>` fields. `Event` is
  `#[non_exhaustive]` so the variant addition is non-breaking;
  destructures that used `..` continue to compile. Direct destructures
  of `RunFinished { ok, .. }` continue to work; explicit field grabs
  recompile cleanly with the new fields added.
- **`AgentExecutor::new` arity grew to 3.** Embedders constructing it
  directly must pass `body_excerpt_max_bytes` or use
  `with_defaults(transport, sink)`. The CLI is updated.
- **No CLI surface change.** `--verbose` and `--stderr-tail-bytes`
  retain their meaning; the same `max_output_bytes` value now also
  caps `LlmFailure::ApiError.body_excerpt`.
- **Test surface adds 6 cases**:
  - `events::tests::classifies_auth_rate_limit_and_model_not_found_without_carrying_provider`
  - `events::tests::api_error_truncates_body_to_configured_cap`
  - `events::tests::legacy_variants_fall_through_to_other_with_chain_preserved`
  - `node::agent::tests::transport_error_emits_llm_request_failed_before_propagating`
  - `execution::scheduler::tests::run_finished_aggregates_failed_and_skipped_in_causal_order`
  - `execution::scheduler::tests::run_finished_aggregates_are_empty_on_clean_run`
- **No `tracing-subscriber` dependency change.** Phase 3 emits no new
  WARNs — the typed event is the addition, the existing failure WARN
  on `dispatch_node`'s `ExecutorError` branch already covers the
  human-facing surface for transport failures.
- **CLI substring tests survive.** `crates/orno-cli/tests/cli.rs`
  uses count-based substring assertions on `"ok":true` / `"ok":false`.
  The new `RunFinished` JSON adds `"failed_nodes":[…]` and
  `"skipped_nodes":[…]` which contain neither substring; counts are
  unaffected.

## Phase 4 deferrals (next)

- **Schema-versioning policy for record/replay.** When `ReplayTransport`
  starts reading tapes recorded by older orno builds, `RunFinished`
  needs explicit `#[serde(default)]` on `failed_nodes` /
  `skipped_nodes` so old tapes deserialize cleanly. Today there is no
  replay reader so the field is a forward-compat liability, not a bug.
- **Aggregates of LLM failure classes** on `RunFinished` (e.g.
  `transport_failures: Vec<TransportFailureSummary>`) — only worth it
  once a real consumer asks for tail-line transport summaries.
- **`Event::AgentIterationStarted` / `ToolCallRecorded`** for the
  Phase 5 agent loop. Out of scope here.

## Amendments

- **File path — `LlmFailure::from_llm_error`.** The classifier originally
  lived in `crates/orno-core/src/events/mod.rs`; it now lives in
  `crates/orno-core/src/events/failure.rs` alongside the `LlmFailure`
  type itself. The `events` module was split into `event.rs`,
  `failure.rs`, and `mod.rs` (envelope + `truncate_excerpt` helper) to
  keep each file under the 300-LOC soft cap. Public paths are
  unchanged: `crate::events::{LlmFailure, NodeFailure, Event}` still
  resolve because `events/mod.rs` re-exports them.
