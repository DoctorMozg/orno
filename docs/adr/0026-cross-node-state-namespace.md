# ADR 0026 — Cross-node `state.*` namespace with declared per-node writes

- Status: proposed (deferred post-v0.1)
- Date: 2026-04-23
- Depends on: ADR 0025 (node-scoped `SetState`)

## Context

ADR 0025 lets an agent publish structured output under
`nodes.<self>.state.*`. Downstream nodes read it through their normal
template context. That pattern works for any graph where "publisher"
and "consumer" have an explicit DAG edge: the consumer declares
`needs: [<publisher-id>]`, and the state lands as
`nodes.<publisher>.state.*` for template use.

The pattern breaks down when the cross-node signal is **not** a DAG
edge:

- **Running counters.** Ten parallel agents each inspect one file; an
  eleventh summary node wants to know how many flagged issues. Today
  the summary node has to `needs:` all ten and fold
  `nodes.agent_01.state.issues + nodes.agent_02.state.issues + ...`
  manually — and re-foldsince the list grows.
- **Shared budget / shared plan.** A planner agent writes a plan;
  several worker agents read the plan, write their progress back into
  the same key, and a final auditor checks that progress. This is a
  logical "shared whiteboard," not a DAG of node outputs.
- **Fan-in checkpoints.** A quality-gate node wants to look at a
  single `state.release.signoff` key written by different approver
  nodes at different times. Today it has to `needs:` every approver
  even if only one of them ran.

These patterns all want a key that is (a) named independently of any
node id, (b) writable by a declared set of nodes, and (c) readable
from any template. That is the `state.*` namespace.

The user's original framing — "updateable context must be specified in
node permissions" — explicitly anticipated this shape. ADR 0025 is
the narrow cut; this ADR scopes the general version.

This ADR is **proposed**, not accepted. It exists to lock in a name,
an outline, and a set of open questions so the work can land
post-v0.1 without relitigating the shape.

## Revisit triggers

Promote to accepted when any of the following surfaces:

1. A real pipeline in `examples/` or a user-reported pattern needs
   cross-node shared state that ADR 0025 cannot express — i.e. the
   `needs:` edges required to fold it become the dominant noise in
   the DAG.
2. A parallel-branch pattern (fan-out + fan-in on a shared counter)
   lands in a user pipeline. Parallel execution on the DAG walker is
   on the roadmap; the first parallel user is likely to want this.
3. An embedder requests a "shared whiteboard" for a pattern where the
   writers are not known to the readers at pipeline-authoring time
   (e.g., dynamically-generated worker nodes under a future `loop:`
   primitive).

Absent any of these, defer.

## Proposed decision sketch

### 1. Top-level `state:` block

```yaml
state:
  release.signoff:
    default: null
  plan.status:
    default: "drafting"
  issue_count:
    default: 0
```

Declares every writable key up front. Keys are dotted paths; the
declaration is the set of keys the pipeline promises to produce. An
undeclared key cannot be written (template-time error) or read
(renders as undefined). This keeps the namespace closed and makes
static analysis by a future `orno plan` command tractable.

Open: whether declared keys carry JSON-Schema-shape validation or
remain free-form. Lean toward free-form at v1 of this ADR; schemas are
an additive follow-up.

### 2. Per-node `writes:` allow-list

```yaml
- id: planner
  kind: agent
  agent: planner
  initial_prompt: "..."
  writes: [plan.status, issue_count]
```

Each node declares which `state.*` keys it may write. The set is
enforced at pipeline load (`writes:` must be a subset of the declared
keys) and at tool-call gate time (a `SetSharedState` call targeting
a key outside `writes:` returns the tool-result denial string same as
other policy denials). Missing `writes:` is equivalent to
`writes: []`.

### 3. New builtin tool `SetSharedState`

Parallel to ADR 0025's node-scoped `SetState`, but writing to the
top-level `state.*` namespace instead of `nodes.<self>.state.*`:

```text
Tool: SetSharedState
Args: { key: String, value: JsonValue }
Effect: ToolEffect::ContextWrite (new variant)
```

The gate checks membership in `writes:` before dispatch. Shape
validation happens at the pipeline-load boundary; at tool-call time
only the allow-list matters. The tool is deliberately distinct from
node-scoped `SetState` (ADR 0025) so a reader of an `allowed_tools`
list can tell at a glance whether an agent writes to its own bucket
or to the shared graph-level namespace.

### 4. New `ToolEffect::ContextWrite` and
   `AgentPolicy.allow_state_writes`

- `ToolEffect::ContextWrite` gates `SetSharedState` (distinct from
  `ContextSelf` which gates node-scoped `SetState` per ADR 0025).
- `AgentPolicy.allow_state_writes: bool` enables the effect class.
  A node without this flag cannot call `SetSharedState` even if
  `writes:` is non-empty — the two combine via AND.

Two flags instead of one because the pipeline author may want to
declare `writes:` statically while temporarily disabling
state-writing for a run (e.g., a replay).

### 5. New `Event::StateUpdated`

```rust
StateUpdated {
    run_id: String,
    node_id: String,
    key: String,
    value_excerpt: String,   // redacted + truncated per ADR 0020 / 0024
    seq_before: u64,         // last seq that wrote this key (for CAS diagnostics)
}
```

Distinct from the generic tool-call event because cross-node state is
a wire-format concern: consumers inspecting the event stream need to
reconstruct the state timeline without replaying tool calls. Both
events are emitted (tool-call event from the call dispatch; state-
update event from the state mutation) so the log is self-describing.

### 6. Parallelism ordering rules (open)

This is the hardest open question. Three candidates:

- **Last-writer-wins by `seq`.** Simple, deterministic, but silently
  clobbers concurrent writes. Not acceptable for counters.
- **Typed merge per key.** Each declared key carries a merge policy
  (`overwrite`, `sum`, `append`, `min`, `max`). The walker applies
  the policy when two parallel branches try to write the same key.
  More powerful but adds surface.
- **Single-writer invariant.** Only one branch may be a declared
  writer for any given key at any time. Walker enforces at schedule
  time. Restrictive but provably race-free.

ADR 0021 (DAG execution model) currently runs nodes serially. When
the walker gains parallelism, this decision must be made at the same
time — not bolted on later. Lean: single-writer invariant by default
with typed-merge as an opt-in follow-up.

### 7. Replay semantics

`StateUpdated` is replayable via the event log directly. The replay
engine walks `StateUpdated` events in `seq` order and applies each
write to a freshly-constructed `state.*` map. The tool-call event is
still recorded for fidelity with the original execution but is not
load-bearing for replay.

## Open questions

Enumerated here to be resolved when the ADR is promoted:

- **Key namespacing vs structured keys.** `state.release.signoff` vs
  `state { release: { signoff: ... } }`. Dotted flat keys are easier
  to allow-list; nested keys are more natural to template. Picking
  one shape changes the `writes:` syntax and the template access
  syntax.
- **Default-value lifecycle.** Does `default:` apply once at run
  start, or does "no writes yet" render as the default on every
  read? The latter is simpler for consumers but makes
  `state.issue_count == 0` ambiguous (default or written-to-0?).
- **Removal semantics.** Can a node `SetState(key, null)` to clear a
  key, or is `null` just another value? The event log has to
  disambiguate either way.
- **Read tool surface.** Is there a `GetSharedState` tool so the same
  agent that wrote a key can read it back mid-loop without a re-render?
  ADR 0025 punted the analogous `GetState`; the same punt may not be
  right at graph scope.
- **Interaction with `continue_on_error` and skip cascades (ADR
  0021).** If a writer is skipped, does its pre-skip state persist or
  roll back? Rollback is expensive and requires a journal; persist is
  cheap but lets partial writes leak.
- **Observability cost.** Every `StateUpdated` event is one more
  envelope on stdout. With hot loops this adds up. Need a cap — reuse
  ADR 0012's bounded-log backpressure story or declare keys as
  `silent: true` to elide events for truly hot counters.

## Non-goals

- **Not a general key-value store.** `state.*` is a template-context
  namespace, not a durable datastore. No TTL, no external persistence,
  no cross-run state. A future ADR may define a persistent-state
  seam; this is not it.
- **Not a synchronization primitive.** `state.*` does not provide
  locks, conditions, or notifications. A consumer sees the state at
  its render time; there is no "wait until state.x == y" primitive.
  That is a scheduler/control-flow concern (see ADR 0017's deferred
  `gate` kind).
- **Not an arbitrary variable namespace.** `vars.*` remains immutable
  construction-time inputs (ADR 0020). `state.*` is explicitly the
  mutable, named-key namespace; conflating the two would lose the
  "inputs are inputs" invariant.

## Consequences (when promoted)

- **`Pipeline.state`** is a new top-level `BTreeMap<String, StateDecl>`
  field on the schema.
- **`Node.writes`** is a new optional `Vec<String>` field on every
  node kind.
- **`tool::SetSharedState`** is a new builtin handler.
- **`ToolEffect::ContextWrite`** and
  **`AgentPolicy.allow_state_writes`** extend the strictness surface.
- **`Event::StateUpdated`** is a new wire-format event variant.
- **`Context`** grows a fifth namespace (`state: BTreeMap<String,
  Value>`) exposed alongside `vars / env / secrets / nodes`.
- **Schema regen** required on every variant addition.
- **Parallelism story** must be decided in this ADR before the walker
  goes parallel — otherwise the first parallel user locks in a default
  by accident.

## Relationship to ADR 0025

ADR 0025 is the node-scoped cut; this ADR is the graph-scoped cut.
Both can coexist: a pipeline can use `SetState` for incremental
structured output under `nodes.<id>.state.*` and `SetSharedState` for
cross-node shared keys under `state.*`. The two tools are distinct,
the two effect classes are distinct, and the two policy flags are
distinct. An agent that needs both grants both; an agent that needs
neither grants neither.

ADR 0025 intentionally does **not** pre-commit to any of this ADR's
choices. If the revisit triggers never fire, this ADR can be closed
as obsolete without any rework to ADR 0025.
