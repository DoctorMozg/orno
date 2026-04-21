# ADR 0017 — Node attributes over new kinds; v0.1 node-taxonomy deferrals

- Status: accepted
- Date: 2026-04-21

## Context

On 2026-04-21 a five-lens brainstorm (engineer, product, cto, mathematician,
devops) examined the question "what nodes are necessary for the initial
prototype?" at the point of v0.1 scoping. Fifteen ideas surfaced; consensus
(3/5) settled on a doctrine rather than a specific new kind.

**Winning position**: hard no on speculative node kinds (`http`, `parse`,
`assert`, `transform`, `approval`, `artifact`, `parallel`, `loop`). Invest
instead in two cross-cutting attributes — `retry` and `timeout` — that
compose on every kind. Three robust kinds with orthogonal cross-cutting
attributes beats six half-built kinds duplicating retry/timeout logic.

**Dissent (2/5)**: voted for a `gate` node as the minimum branching
primitive. Convergent across ideation, split on whether it should be a
kind or a `when:` / `await:` attribute. Neither side argued it was
load-bearing for v0.1.

**Adjacent decision, unanimous**: the current `external` stub is wrong.
The three lenses proposed three different fixes (cut entirely, rename to
`plugin`, demote to `transport:` axis). The ADR resolution below cuts
entirely and commits to the `transport:` axis shape at v0.2+.

This ADR records:

1. The universal attribute doctrine as architecture.
2. A breaking change to shell-node output shape that had unanimous
   ideation interest (lens-product) and was latent debt otherwise.
3. Full removal of `NodeKind::External`, superseding ADR 0014's
   `--unstable-plugins` gate.
4. Explicit deferrals for `gate`, `checkpoint`, `probe`, `emit_metrics`,
   and `pure` with concrete revisit triggers.

## Decision

### 1. Universal node attributes

Every `NodeKind` at v0.1 (`Agent`, `Shell`) carries two optional
cross-cutting attributes. They apply uniformly; no kind opts out.

#### `timeout: Duration`

Wall-clock ceiling from node start to node result. On breach:

- Emit `NodeTimedOut { limit: Duration, elapsed: Duration }`
  (new event variant, append-only per ADR 0003).
- Terminate the node with `NodeStatus::TimedOut` (new `NodeStatus`
  variant, added under `#[non_exhaustive]` per ADR 0010).

Per-kind termination mechanics:

- **Shell**: subprocess receives SIGTERM, then a 5s grace, then SIGKILL.
  Fixed at v0.1; the grace window becomes configurable if real users
  ask.
- **Agent**: the loop is cancelled at the next check-point — checked
  before each iteration and before each tool-call dispatch. In-flight
  LLM calls and tool calls are not preempted; they return, their
  result is recorded (budget accounted), and the loop then exits
  with `TimedOut`.

#### `retry: RetryConfig`

```text
retry:
  max_attempts: u32        # default 1 (no retry)
  backoff: Backoff         # default exponential { initial: 1s, max: 60s }
  share_budget: bool       # default true (agent only; ignored for shell)
```

`Backoff` is one of:

- `fixed(Duration)`
- `exponential { initial: Duration, max: Duration }`

On a `Failed` node result (ADR 0010), the executor re-runs the node up
to `max_attempts` total attempts (so `max_attempts: 3` is one original
plus two retries). Between attempts, emit
`NodeRetryAttempted { attempt: u32, prev_status: NodeStatus }`.

If the final attempt still produces `Failed`, emit
`NodeRetryExhausted { attempts: u32 }` and the node settles `Failed`.

`share_budget` scopes the agent's `AgentPolicy` budget across attempts:

- `true` (default) — retry draws from the same `max_total_tokens` /
  `max_tool_calls` counters as the first attempt. A loop that burned
  the budget and failed does not get a fresh budget to try again.
- `false` — each attempt opens a fresh `AgentPolicy` budget. The
  pipeline-level token ledger still aggregates across attempts;
  `timeout` is always per-attempt regardless of `share_budget`.

For shell nodes there is no budget, so `share_budget` is ignored.

A `TimedOut` node result is **not** retried by default — timeout
retries are a config-error trap (same timeout, same expected outcome).
Future knob: `retry.retry_on: [failed, timed_out]` if demand appears.

### 2. Shell node structured output

Shell nodes produce three template context fields. The single `.output`
field is **removed on shell** at v0.1 (breaking change, pre-release):

```jinja
{{ nodes.<id>.stdout }}       # captured stdout, String
{{ nodes.<id>.stderr }}       # captured stderr, String
{{ nodes.<id>.exit_code }}    # process exit code, i32
```

`nodes.<id>.status` remains as defined in ADR 0010 (with the new
`TimedOut` value). `nodes.<id>.output` is **not available** on shell
nodes and references to it are a template-render error.

**Agent nodes keep `nodes.<id>.output`** (final assistant message per
ADR 0010). The asymmetry is deliberate: shell has three distinct channels
a downstream consumer may need; an agent has one final message.

### 3. `NodeKind::External` is removed

`NodeKind::External` is **not** in the v0.1 `NodeKind` enum. Not gated
behind a flag, not kept as a stub. This supersedes ADR 0014's
`--unstable-plugins` posture.

Rationale:

- ADR 0014's own context section enumerates five things the reserved
  wire format lacks (JSON handshake, capability negotiation, streaming,
  stream discipline, cancel ladder). Keeping a stub freezes a shape
  that is not a superset of the real protocol; the stub is debt, not
  optionality.
- Every user-facing kind at v0.1 carries test coverage, docs, example
  pipelines, and error-message surface. Two robust kinds
  (`agent`, `shell`) cost less than three half-built ones.
- The ADR 0004 and ADR 0014 deferrals remain conceptually correct;
  this ADR simply removes the residual symbol from v0.1's public
  surface until the stabilization ADR is written.

**Reintroduction path (v0.2 or later)**:

- When subprocess plugins return, they are a `transport: builtin |
  subprocess` axis on the existing kinds — not a sibling kind. This
  follows the lens-mathematician framing: "external" is a transport
  isomorphism on any kind, not a taxonomy point.
- ADR 0014's stabilization checklist (handshake, capabilities, stream
  discipline, cancel ladder) carries over unchanged into the follow-up
  ADR.

### 4. Deferred node kinds

Explicit v0.2-or-later candidates with revisit triggers. Shipping any
of these at v0.1 is a scope violation requiring a new ADR.

| Kind            | Rationale for deferral                                                                                                                                                         | Revisit trigger                                                                                                                      |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| `gate`          | 2/5 dissent. Unanimous ideation interest, split on whether it's a kind or a `when:` / `await:` attribute. Either shape is cheap; neither is load-bearing for v0.1.             | First user asks for conditional branching or approval-hold. Settle kind-vs-attribute in a follow-up ADR before implementation.        |
| `checkpoint`    | Requires stabilized event schema (ADRs 0003/0012) and a cache-key invariant no real user has validated. Premature binding of keys to be re-litigated later.                    | First reproducible "3am resume" need or cached re-run request from a real pipeline.                                                  |
| `probe`         | Thin `NodeExecutor` wrapper around assertions; adds public surface before first user complains about wasted token spend.                                                       | First user reports wasted LLM spend on a precondition that could have been asserted up front.                                        |
| `emit_metrics`  | Requires `needs: [*]` terminal-node scheduling pattern; out of scope for the Phase 7 scheduler.                                                                                | First production adopter wants Prometheus/OTLP aggregates at DAG completion.                                                         |
| `pure`          | MiniJinja in `vars` and agent prompts covers the deterministic-transform use cases already; the purity tax does not pay for itself at v0.1.                                    | First user wants a `pure` node whose transform MiniJinja cannot express without agent scaffolding.                                   |

## Consequences

- **`NodeKind` in v0.1**: `Agent`, `Shell`. Two variants. The enum
  remains `#[serde(tag = "kind")] #[non_exhaustive]` per CLAUDE.md
  invariants.
- **`NodeStatus` gains `TimedOut`** variant (append-only under
  `#[non_exhaustive]`). Descendants of a `TimedOut` node are skipped
  by default, same as `Failed` (ADR 0010); `continue_on_error: true`
  covers both.
- **`BudgetExceeded { kind: WallClock }` is retired** pre-v0.1 (ADR
  0005 amendment). Timeout breaches emit `NodeTimedOut`, which is
  orthogonal to the budget dimension and applies to every kind, not
  just agents.
- **`RetryConfig` and `TimeoutConfig`** are new public types on the
  pipeline surface. Both are `#[non_exhaustive]` so `Backoff` and
  future retry knobs can grow without breaking.
- **`NodeRetryAttempted` and `NodeRetryExhausted`** are new `Event`
  variants under `#[non_exhaustive]`.
- **Shell template migration**: pre-v0.1 has no published users, so
  `.output → .stdout/.stderr/.exit_code` is a rename, not a migration.
  Any internal usage in `examples/` and tests is updated in the same
  commit that lands the change.
- **`PipelineError::UnstableNodeKind` and `--unstable-plugins`** are
  removed (from ADR 0014). Accepting a YAML pipeline with
  `kind: external` becomes an "unknown variant" parse error via serde,
  same as any other unknown kind.
- **Schema regen**: `cargo run -p orno-cli -- schema >
  schemas/pipeline.schema.json` after the node-kind enum change.
- **Docs**: `docs/yaml-spec.md` gains `retry:` and `timeout:` under
  every node kind, documents the shell `.stdout/.stderr/.exit_code`
  shape, and drops any `kind: external` examples or references.

## Amendments

- Amends **ADR 0004**: the reserved `NodeKind::External` stub is
  removed from v0.1. Post-v0.1 reintroduction takes a
  `transport: builtin | subprocess` axis shape per §3 of this ADR,
  not a sibling kind.
- Amends **ADR 0005**: dimension 4 "Bounded resources" is narrowed to
  `max_total_tokens` + `max_tool_calls`. `max_wall_clock` and the
  `BudgetExceeded { kind: WallClock }` variant are retired; wall-clock
  is handled by the universal `timeout:` attribute, which emits
  `NodeTimedOut` on breach.
- Amends **ADR 0009**: the v0.1 node-kind set is `agent, shell` (not
  `agent, shell, external`).
- Amends **ADR 0010**: shell output is split into
  `.stdout / .stderr / .exit_code`; agent retains `.output`.
  `NodeStatus` gains `TimedOut`; scheduler semantics for `TimedOut`
  match `Failed` (skip descendants, covered by `continue_on_error`).
- Supersedes **ADR 0014**: `NodeKind::External` is removed from v0.1
  entirely; `--unstable-plugins` is retired. The stabilization
  checklist in ADR 0014 carries over into the ADR that reintroduces
  subprocess transport at v0.2+.
