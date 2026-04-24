# ADR 0027 — `SubagentHandler::effect` derives from the child agent's policy

- Status: accepted
- Date: 2026-04-24
- Depends on: ADR 0005 (strictness dimensions), ADR 0006 (subagent-as-tool-call), ADR 0008 (builtin tool set)

## Context

`SubagentHandler` (ADR 0006) implements `ToolHandler` so the parent's
agent loop can call a child agent through the same tool-dispatch path
as every other tool. `LoopAgent`'s policy gate (`agent::loop_agent::
policy::check_policy_and_invoke`) branches on `handler.effect()` and
denies the call if the declared effect exceeds the parent's
`AgentPolicy`.

Today, `SubagentHandler::effect` is hardcoded:

```rust
// crates/orno-core/src/tool/subagent.rs:134-142
fn effect(&self) -> ToolEffect {
    // A subagent inherits the union of its own policy's effects.
    // Declaring the handler as `MutationsAndNetwork` means the
    // parent must itself be allowed both — otherwise the policy
    // gate denies before we ever reach `invoke`. Per-subagent
    // effect composition (child ≤ parent) is deferred; the
    // parent's gate is the conservative ceiling for now.
    ToolEffect::MutationsAndNetwork
}
```

The comment is the ADR: this was a deferred design decision. The
consequence is that **any** parent wanting to dispatch **any**
subagent must grant `allow_mutations: true` AND `allow_network: true`,
even when the child is read-only and declares no mutating or
networking tools. Concretely:

- `examples/pr-review.yaml` defines `pr_reviewer` with
  `allow_mutations: false, allow_network: false` delegating to three
  read-only lens subagents. Pipeline validation accepts it (the
  compose-down rule is child ≤ parent, and `false ≤ false`). At
  runtime the first subagent tool call returns
  ``denied: tool `subagent.security_lens` blocked by
  allow_mutations=false`` and the loop makes no progress.
- The Phase 6 audit (`.mz/task/audit_phase6_031657/`) surfaced this
  when the new three-subagent regression fixture had to hardcode
  `allow_mutations: true, allow_network: true` on the parent
  — solely so the subagent dispatch gate would not deny the call.
  That fixture now lies: it declares broad permissions not because
  the pipeline needs them but because the handler asked for them.

The pipeline-load compose-down check at
`crates/orno-core/src/pipeline/load.rs:123-134` already guarantees:

```rust
if !agent_config.policy.allow_mutations && child.policy.allow_mutations {
    return Err(PipelineError::Validation(/* child more permissive */));
}
if !agent_config.policy.allow_network && child.policy.allow_network {
    return Err(PipelineError::Validation(/* child more permissive */));
}
```

So at the moment the parent tries to dispatch the child, we already
know `child.policy ≤ parent.policy` on both the mutation and network
axes. The runtime gate has no new information to add — it is strictly
more conservative than pipeline validation. Gating the subagent call
as `MutationsAndNetwork` forces the parent to hold the union of
every possible child's effects, defeating the point of the per-child
policy declaration.

## Decision

`SubagentHandler::effect` returns the effect class derived from the
child agent's `AgentPolicy.allow_mutations` and
`AgentPolicy.allow_network` booleans, captured at handler construction
from the child's `AgentConfig`. No new `ToolEffect` variant is
introduced — the existing four map the four combinations exactly:

| `child.allow_mutations` | `child.allow_network` | `SubagentHandler::effect()` |
| ----------------------- | --------------------- | --------------------------- |
| `false`                 | `false`               | `ToolEffect::ReadOnly`      |
| `true`                  | `false`               | `ToolEffect::Mutations`     |
| `false`                 | `true`                | `ToolEffect::Network`       |
| `true`                  | `true`                | `ToolEffect::MutationsAndNetwork` |

The derivation is computed once at construction and captured in a new
private field on the handler, not recomputed per call. `effect()`
remains `O(1)` and side-effect-free, consistent with every other
`ToolHandler` impl.

### 1. Why this is sound

Pipeline validation (ADR 0006 §compose-down; code in
`pipeline/load.rs:123-134`) is the single point of truth for
`child.policy ≤ parent.policy` on both booleans. By the time any
`SubagentHandler` instance exists, the invariant holds. The runtime
gate then asks only: *does the parent's policy admit the effects the
child's policy actually declares?* — which is the question the gate
is shaped to answer.

Two consequences fall out:

- A read-only parent delegating to a read-only child sees
  `ToolEffect::ReadOnly`, which bypasses every policy-gate branch and
  invokes the child loop directly. This is the `pr_reviewer` case.
- A parent that declares `allow_mutations: true` delegating to a
  read-only child sees `ToolEffect::ReadOnly` — the child runs
  without the parent's mutation grant *reaching the child*. The
  child's own policy gate (its own `LoopAgent::check_policy_and_invoke`
  one level down) denies any mutating tool the child might attempt.
  This preserves ADR 0005 §3's per-agent effect discipline: each
  agent's gate is the authoritative check for that agent's tools.

### 2. Handler construction stays compile-time-safe

`SubagentHandler::new` already takes the full `child_config:
AgentConfig` (see `crates/orno-core/src/tool/subagent.rs:85-99`), so
no signature change is required. The constructor computes the effect
from `child_config.policy` and stores it in a `declared_effect:
ToolEffect` field:

```rust
pub struct SubagentHandler {
    // existing fields…
    /// Effect class derived once from `child_config.policy` at
    /// construction. ADR 0027 — replaces the former hardcoded
    /// `MutationsAndNetwork` return.
    declared_effect: ToolEffect,
}

impl SubagentHandler {
    pub fn new(
        yaml_name: String,
        child_agent_name: String,
        child_config: AgentConfig,
        parent: Weak<LoopAgent>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        let declared_effect = effect_from_policy(&child_config.policy);
        Self { /* … */, declared_effect }
    }
}

impl ToolHandler for SubagentHandler {
    fn effect(&self) -> ToolEffect { self.declared_effect }
}

fn effect_from_policy(p: &AgentPolicy) -> ToolEffect {
    match (p.allow_mutations, p.allow_network) {
        (false, false) => ToolEffect::ReadOnly,
        (true, false) => ToolEffect::Mutations,
        (false, true) => ToolEffect::Network,
        (true, true) => ToolEffect::MutationsAndNetwork,
    }
}
```

`effect_from_policy` is a private module-level helper with a narrow
contract (two booleans → one variant). A unit test pins every
quadrant so adding a future `AgentPolicy` boolean forces the helper
to be revisited rather than silently skipped.

### 3. Scope: only the two boolean effect axes

This ADR explicitly scopes to `allow_mutations` and `allow_network`.
It does **not** address:

- **Domain allowlist/blocklist** (`allowed_domains`,
  `blocked_domains`). The parent's domain gate is a URL-level check
  on tools that expose a `url` argument (`WebFetch`, networked MCP).
  `SubagentHandler::invoke` has no URL, so the domain gate is a no-op
  at the subagent boundary today. Domain composition between parent
  and child is ADR-worthy but separate — the child's own `WebFetch`
  gate already applies the child's allowlist. If a future finding
  shows a child circumventing the parent's domain policy by routing
  through a subagent's `WebFetch`, revisit with a dedicated ADR.
- **`ToolEffect::ContextSelf`** (ADR 0025). Whether a child's
  `SetState` writes should require the parent to have
  `allow_context_writes: true` depends on where the state lands
  (parent-node slot vs. child-node slot) and is entangled with ADR
  0026's cross-node state story. Not decided here.
- **`mcp_handler.rs`'s equivalent hardcoded `MutationsAndNetwork`.**
  MCP tools declare no typed effect on the rmcp side; inferring
  per-tool effects from MCP tool metadata is its own research
  question (some servers expose a `destructive` hint, most do not).
  Out of scope for this ADR; tracked as a follow-up.
- **Transitive tool-set derivation** (ADR 0006 line 80's
  "`SubagentHandler.mutates()` flag … computed … as
  `any(child.tools.mutates)` transitively"). Deriving the effect
  from the child's *declared tool set* would be strictly more
  precise — a child with `allow_mutations: true` but zero mutating
  tools in `allowed_tools` could legally surface as `ReadOnly`. This
  ADR adopts the simpler policy-derived form because (a) the child's
  policy is the user-facing declaration; (b) tool-set derivation
  requires resolving MCP wildcards and recursing through nested
  subagents; (c) the policy-derived form already solves the
  `pr-review.yaml` motivating case. Tool-set derivation is a
  refinement a future ADR may adopt once the MCP effect question
  (previous bullet) is resolved.

### 4. Replay and event semantics

The handler's `invoke` body is unchanged: same `SubagentStarted` /
`SubagentCompleted` / `SubagentFailed` events, same child
`LoopAgent::run` dispatch. Recording and replay behavior is
unchanged — the effect is a schedule-time concern (does this call
dispatch?), not a wire-format concern.

`Event::ToolDenied` is emitted per ADR 0005 §3 if the derived
subagent effect exceeds the parent's policy at runtime. In practice
this is unreachable when pipeline validation ran (because
compose-down holds), but the runtime gate remains the last line of
defense: a pipeline loaded through a future `skip_validation` path
or a programmatically constructed `LoopAgent` would still see a
runtime denial rather than a silent breach.

## Consequences

- **`crates/orno-core/src/tool/subagent.rs`** gains a
  `declared_effect: ToolEffect` field and an `effect_from_policy`
  helper; `effect()` returns the field. Delete the "deferred"
  comment block and link to this ADR in its replacement one-liner.
- **Four-quadrant unit test** added alongside the existing
  `SubagentHandler` tests — one `#[rstest]` table pinning each
  `(allow_mutations, allow_network)` combination to the expected
  `ToolEffect`.
- **Integration coverage** extended so a fresh test dispatches a
  read-only child from a read-only parent end-to-end and asserts
  `Event::ToolDenied` is *not* emitted. Today's fixture
  (`crates/orno-cli/tests/fixtures/three-lens.yaml`) can drop the
  parent's `allow_mutations: true, allow_network: true` workaround
  and rely on `false/false` to exercise the motivating
  `pr-review.yaml` shape. The fixture becomes the regression case.
- **`examples/pr-review.yaml`** now actually runs at runtime with
  its declared `false/false` permissions. Replay fidelity is
  unchanged.
- **Schema, event envelope, CLI surface**: unchanged. No
  `ToolEffect` variant added, no `AgentPolicy` field added, no
  `Event` variant added. The change is strictly internal to the
  handler.
- **Validator stays authoritative.** The compose-down check at
  `pipeline/load.rs:105-137` is untouched. This ADR relies on it,
  not around it.
- **Audit trail preserved.** The runtime gate still emits
  `ToolDenied` when the invariant is somehow violated (e.g., the
  handler is constructed through a path that bypasses validation).
  Deleting `MutationsAndNetwork` as the default does not weaken
  the audit surface; it tightens the claim the event records ("the
  child's declared effect exceeded the parent's") to match
  reality.

## Amendments

- Amends **ADR 0006** §"compose down": the invariant is unchanged
  (child ≤ parent), but the runtime realization is moved from a
  conservative constant (`MutationsAndNetwork`) to the child-policy
  derivation above. Line 80's deferred "`SubagentHandler.mutates()`
  flag … computed transitively" remains deferred — this ADR
  supplies the policy-derived form, tool-set derivation is the
  further refinement.
- Amends **ADR 0008**: `subagent.<child>` rows in the effect-class
  table are no longer a single `MutationsAndNetwork` entry; the
  effect class is derived from the child's `AgentPolicy` as in §1's
  table. The two orthogonal booleans continue to be the gate; the
  handler just asks the narrower of the two questions.

## Revisit triggers

Promote decisions left as out-of-scope when any of these lands:

1. A pipeline hits `WebFetch` routing through a subagent boundary
   where the child's `allowed_domains` differs from the parent's —
   motivating a domain-composition ADR.
2. `ToolEffect::ContextSelf` (ADR 0025) gets a cross-agent use case,
   motivating a decision about whether a child's `SetState` should
   require the parent's `allow_context_writes`.
3. An MCP tool metadata convention (destructive hint, official
   Effect annotation) emerges in the rmcp ecosystem, letting
   `McpToolHandler::effect` drop its own hardcoded
   `MutationsAndNetwork`.
4. A real pipeline declares `allow_mutations: true` on a child that
   clearly cannot mutate given its declared tools, and the parent
   wants to be read-only despite that. Motivates tool-set-derived
   effect over policy-derived effect.
