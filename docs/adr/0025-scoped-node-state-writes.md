# ADR 0025 — Scoped node-state writes via a `SetState` builtin tool

- Status: accepted
- Date: 2026-04-23
- Phase: 5 follow-on — debuggability for mid-loop state publication

## Context

`Context` in `crates/orno-core/src/execution/context.rs` exposes four
template namespaces — `vars.*`, `env.*`, `secrets.*`, `nodes.*`. Only
`nodes.*` is mutable during a run, and the only mutator is the
scheduler itself: `record_node_output` is called exactly once per
successful node, pinning `nodes.<id>` to whatever the node returned in
its terminal response.

Nothing inside an agent loop can publish structured state. The five
builtin tools (ADR 0008) all target external effects:
`Bash`/`Read`/`Edit`/`Write` touch the filesystem or a subprocess;
`WebFetch` issues an HTTP GET. None of them land a value in the
pipeline's own template context. The single escape hatch is the final
assistant message, which the scheduler serializes into
`nodes.<id>.output` (ADR 0010).

That single final-message slot is not enough for three real patterns:

- **Incremental structured output.** An agent working on a plan wants to
  publish `plan.status = "drafting"`, later `plan.status = "ready"`,
  so downstream nodes can branch without re-parsing a free-form message.
- **Typed payloads over free text.** A digest agent wants downstream
  shell nodes to template `nodes.digest.pr_count` directly, not to
  re-parse the assistant message with a regex.
- **Multi-key outputs.** A triage agent wants
  `nodes.triage.labels = [...]`, `nodes.triage.assignee = "..."`, and a
  narrative `nodes.triage.output` — three distinct consumers,
  three distinct shapes, one node.

The user surfaced the gap directly: "we don't have tools for adding
data to the context or updating it, updateable context must be
specified in node permissions." The permission framing is correct —
ADR 0005 §3 ("bounded effects") already declares effect classes as
per-node `AgentPolicy` flags, so a context-write effect fits the
existing shape.

A brainstorm on 2026-04-23 weighed two alternatives:

- **A — scoped writes under `nodes.<self>.state.*`.** One new tool
  (`SetState`), one new `ToolEffect` variant, one new `AgentPolicy`
  flag. No new wire events. Solves incremental/typed/multi-key output
  for a single node.
- **B — cross-node mutable `state.*` namespace.** New top-level YAML
  block, per-node `writes:` allow-list, new event variant, parallelism
  ordering rules. Solves running-state patterns across the DAG.

This ADR accepts A for v0.1. B is deferred to ADR 0026 with explicit
revisit triggers.

## Decision

### 1. New builtin tool `SetState`

Add `SetState` to the v0.1 builtin tool set (amends ADR 0008). YAML
name is PascalCase to match the existing builtins.

```text
Tool: SetState
Args: { key: String, value: JsonValue }
Effect: ToolEffect::ContextSelf (new variant, see §3)
Returns: "ok" on success; on error, a ToolError variant.
```

`key` is a **single top-level identifier** under
`nodes.<self>.state` — no dotted paths, no traversal, no intermediate
object creation. This keeps the tool's semantics trivial for the
model and eliminates a class of shape-conflict errors before they can
happen.

- `key: "plan"` writes to `nodes.<self>.state.plan`.
- `key` must match `[A-Za-z_][A-Za-z0-9_]*` and be non-empty. Empty,
  dot-containing, or otherwise malformed keys are
  `ToolError::InvalidArgs { name: "SetState", message: "..." }` —
  reusing the pre-existing `InvalidArgs` variant that every other
  builtin returns for shape mismatches. The gate has already cleared
  the call, so this is an argument-validation error, not a policy
  denial.
- A second call with the same `key` **replaces** the prior value
  wholesale. There is no merge; if the agent wants nested structure,
  it passes a JSON object as `value` and replaces the whole subtree
  in one call.

`value` is any valid JSON. The redactor built for the run (ADR 0020)
runs across string leaves before the value is stored so a misrouted
`secrets.*` render never lands in the event log via the subsequent
tool-call excerpt (ADR 0024).

### 2. Scope of writes is strictly `nodes.<self>.state.*`

Three hard rules make the invariant trivial to reason about:

- `SetState` cannot write outside its own node. There is no argument
  for target node id; there is no way to write to another node's
  `state`. Cross-node state is ADR 0026's problem.
- `SetState` cannot touch `nodes.<self>.output`. The final assistant
  message still lands there per ADR 0010 and is not authored via this
  tool. `state` and `output` are sibling keys on `nodes.<self>`.
- `SetState` cannot touch `vars.*`, `env.*`, or `secrets.*`. Those
  three namespaces remain construction-time immutable per ADR 0020.
  `key` validation rejects dots outright, so there is no way to
  smuggle a write outside `nodes.<self>.state.<key>`.

This gives the invariant: every mutable key observable from a template
is rooted at `nodes.<id>`. The set of writable keys per node is
`nodes.<id>.state.**`, and the set of mutators is `{scheduler at
NodeFinished, SetState when the gate is open}`.

### 3. New `ToolEffect::ContextSelf` variant

`ToolEffect` already carries `ReadOnly`, `Mutations`, `Network`,
`MutationsAndNetwork`, and is `#[non_exhaustive]`. Append one variant:

```rust
/// Mutates `nodes.<self>.state.*` via the `SetState` builtin.
/// Requires `allow_context_writes`. Does not imply `Mutations` —
/// external side effects (fs/process) still require that flag.
ContextSelf,
```

The effect is intentionally **not** folded into `Mutations`.
`Mutations` today means external side effects on the host environment
(`Bash`, `Edit`, `Write`). `ContextSelf` writes live entirely inside
the template context of the current run. Gating them with one flag
would conflate blast radii — a pipeline that wants pure-in-process
planning can allow `ContextSelf` without granting filesystem mutation.

### 4. New `AgentPolicy.allow_context_writes: bool`

Amend ADR 0005 §3. `AgentPolicy` grows one required bool, defaulted in
examples but **not** silently in code (same discipline as
`allow_mutations` / `allow_network`):

```rust
pub struct AgentPolicy {
    // existing fields…
    pub allow_context_writes: bool,
}
```

Gate logic in `LoopAgent::check_policy_and_invoke`:

- `ToolEffect::ContextSelf` + `allow_context_writes == false` → return
  the tool-result denial string ``denied: tool `{name}` blocked by
  allow_context_writes=false``. Loop continues (consistent with ADR
  0005 §3 "denials feed back to the model as tool-result strings").
- `ToolEffect::ContextSelf` + `allow_context_writes == true` → invoke.

### 5. Per-node state size bounded by `max_output_bytes`

The serialized size of `nodes.<self>.state` after each `SetState`
call is compared against `EngineConfig.max_output_bytes` (the same cap
ADR 0022 / 0023 / 0024 use for stderr tails, API body excerpts, and
prompt/response excerpts). Exceeding the cap is
`ToolError::StateTooLarge { bytes, cap }` — the call is rejected and
`state` is left at its previous value. This keeps one worn-in budget
knob; no new config surface.

Sharing the cap is deliberate: a user who raises it for debugging tail
output also raises it for state payloads, which are the same category
of "values that will be serialized into events."

### 6. Visibility

- **Within the same loop iteration**, `SetState` has no effect on
  template rendering. `initial_prompt` is rendered once at node start
  (ADR 0011 + current `AgentNode` semantics). Tool results carry their
  own payload back to the model; the model does not re-render a
  template to see what it just wrote.
- **Across loop iterations of the same node**, the state written by
  prior tool calls is readable by the agent only by issuing another
  tool call (e.g., a future `GetState` — not in this ADR) or by
  re-receiving it as a `ToolResult` string. v0.1 returns the written
  value as the tool's result string so the model has immediate
  feedback ("ok: wrote plan = <value>"); no separate read tool.
- **Across nodes**, `nodes.<id>.state.*` is visible to every
  downstream node via the existing template context. This is the
  load-bearing behavior.

### 7. Replay and event emission

The `SetState` call is recorded via the existing tool-call event
flow (`ToolCallStarted` / `ToolCallCompleted`, plus the tool-call
excerpt fields introduced in ADR 0024). On replay, the same tool call
is re-issued with the same args and produces the same state
transition. No new event variant is required for determinism.

Tool-call excerpts emitted from `SetState` run through the same
redactor + head-truncation as every other excerpt (ADR 0020 / 0024).
The `value` argument is redacted before being stored **and** before
being emitted.

## Consequences

- **`tool::SetState`** is a new builtin handler in
  `crates/orno-core/src/tool/set_state.rs`, exported from
  `tool::mod`. Amends ADR 0008 §"v0.1 builtin tool set".
- **`ToolEffect::ContextSelf`** is a new variant. Append under
  `#[non_exhaustive]`; no downstream break.
- **`AgentPolicy.allow_context_writes: bool`** is a new required
  field. Amends ADR 0005 §3. Schema regen is required
  (`cargo run -p orno-cli -- schema > schemas/pipeline.schema.json`).
- **`AgentOutput`** grows an optional `state: Option<serde_json::Value>`
  field carrying the node's final `state` tree. `AgentExecutor`
  serializes `NodeResponse.output` as
  `{ "output": <final message>, "state": <state or null> }` so the
  engine's existing `record_node_output` stores the combined object
  under `nodes.<id>`. Templates read `nodes.<id>.output` and
  `nodes.<id>.state.*` exactly as described in §2.
- **`ToolError`** gains one new variant under `#[non_exhaustive]`:
  `StateTooLarge { name, bytes, cap }`. The pre-existing
  `InvalidArgs { name, message }` variant already covers malformed-key
  reporting and is reused as-is — no churn to every other builtin.
- **`LoopAgent` and `LoopAgentConfig`** grow a per-node state buffer
  plumbed to the `SetState` handler. The plumbing is internal; the
  public `LoopAgent::new` signature is unchanged. `ToolInvocation`
  gains an optional `state_handle: Option<StateHandle<'a>>` so the
  `SetState` handler can reach the buffer without a global. Existing
  handlers ignore the field.
- **Wire format is unchanged.** No new `Event` variant. Tool-call
  excerpts already cover the debuggability need.
- **CLI surface** is unchanged. `orno run` behavior differs only when
  a pipeline sets `allow_context_writes: true` on an agent and the
  agent calls `SetState`.
- **Documentation**: `docs/yaml-spec.md` gains `allow_context_writes`
  in the `AgentPolicy` table and a "Node-scoped state" section
  describing `nodes.<id>.state.*`. `examples/` gains one pipeline
  that exercises the happy path.
- **Test coverage** lands in four layers:
  1. Handler unit tests for `SetState` (happy path, malformed key,
     oversize payload, redaction of a secret leaf).
  2. `LoopAgent` policy-gate test (`ContextSelf` denied when
     `allow_context_writes=false`; same `#[rstest]` table as the
     other strictness-dimension cases).
  3. `AgentExecutor` integration test asserting `nodes.<id>.state.*`
     is readable from a downstream shell node template.
  4. Insta snapshot over the event stream for a pipeline that writes,
     reads, and redacts state.

## Amendments

- Amends **ADR 0005** §3: `AgentPolicy` gains
  `allow_context_writes: bool`; `ToolEffect::ContextSelf` is the new
  effect class gated by it. Dimensions 1, 2, 4, 5 are unchanged.
- Amends **ADR 0008**: the v0.1 builtin tool set is
  `{Bash, Read, Edit, Write, WebFetch, SetState}`. `SetState` is
  declared with `ToolEffect::ContextSelf`.
- Amends **ADR 0010**: `nodes.<id>` is a two-field object
  `{ output, state }` on agent nodes; `output` remains the final
  assistant message, `state` is `null` when the agent made no
  `SetState` calls and is the merged state tree otherwise.
  `nodes.<id>.state` does not exist on shell nodes (shell keeps its
  `stdout`/`stderr`/`exit_code` split from ADR 0017).
- Amends **ADR 0020**: the per-run redactor also redacts `SetState`
  values and the tool-call excerpts emitted for them.
