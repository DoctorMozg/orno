# ADR 0005 — Strict agentic loops with five strictness dimensions

- Status: accepted; dimension 4 (bounded resources) narrowed by ADR 0017
- Date: 2026-04-21

## Context

The original scope was "thin executor for linear pipelines." Positioning
has sharpened: orno is a runner for *strict* agentic loops. Without
explicit strictness, an agent loop is CrewAI. With it, the loop is the
differentiator — the gap between a boring CI tool and "Devin that
racked up a $400 bill overnight." `docs/chat.md` §"What 'strict' means
in concrete terms" enumerates five dimensions; this ADR fixes them as
user-facing guarantees.

## Decision

Every `agent` node (ADR 0009) runs a loop with all five dimensions
enforced by the runtime, not by prompt discipline:

1. **Bounded iteration.** `max_iterations` is mandatory (default 10).
   Exceeding the bound emits `IterationLimitExceeded` and terminates
   the node. No exceptions, no "just one more loop."

2. **Bounded tool surface.** The pipeline declares exactly which
   builtins and MCP tools the model can call (ADR 0008). A model
   calling an undeclared tool emits `UnknownToolCalled` and
   terminates the node — never "the model invented a tool, let it
   figure out the error." Strictness is the point.

3. **Bounded effects.** Two orthogonal policy booleans on each agent:
   - `allow_mutations` gates `Edit`, `Write`, mutating MCP tools.
   - `allow_network` gates `WebFetch`, network MCP tools.
   - `Bash` requires both — it can do either.
   - `Read` needs neither.

   Two further knobs on network access: `allowed_domains` and
   `blocked_domains` per agent; blocklist wins on overlap. A blocked
   call emits `MutatingCallBlocked`, `NetworkBlocked`, or
   `DomainBlocked` and is returned to the model as a tool-call
   failure (not a loop termination) so the model can recover or give
   up.

4. **Bounded resources.** Every agent declares `max_total_tokens`,
   `max_tool_calls`, `max_wall_clock`. Budget breach emits
   `BudgetExceeded { kind: Tokens | ToolCalls | WallClock }` and
   terminates the node. Not a warning.

5. **Bounded non-determinism.** Every LLM request and every tool
   call is an event on the log (ADR 0003). Replay reproduces the
   run bit-for-bit given the recorded transport tape and the
   recorded tool results. The transport is the only non-deterministic
   seam, and it is recorded by definition.

Parallel tool calls returned by the model are executed **serially** in
declaration order. Pipeline-level parallelism still runs through the
DAG scheduler; agent-internal parallelism is out of scope and not a
roadmap item. Justification: serial execution keeps budget accounting
race-free and replay deterministic, at the cost of wall-clock time
that is rarely the bottleneck in CI.

## Consequences

- Every strictness violation is a typed `Event` variant. The event
  enum grows but stays `#[non_exhaustive]` (ADR 0003) so additions
  don't break replay consumers.
- Tool errors are injected into the conversation as tool-result
  messages; the model sees its failures and may recover. Budget
  costs accrue regardless.
- "Very strict" becomes a product claim in the README. Tests that
  exercise each violation path are mandatory — one negative test per
  dimension, minimum.
- An `AgentPolicy` struct aggregates the five dimensions; every
  agent has one, no defaults are implicit at the call site.
- The loop body is the code chat.md sketches in pseudocode — this ADR
  freezes its shape. Deviating from that shape (e.g., reordering
  budget checks after the LLM call) must be argued in a follow-up
  ADR.

## Amendments

ADR 0017 (node attributes over new kinds) narrows dimension 4 "Bounded
resources":

- The declared budgets are now `max_total_tokens` and `max_tool_calls`
  only. `max_wall_clock` is retired as an agent-specific budget.
- The `BudgetExceeded { kind: WallClock }` event variant is retired
  pre-v0.1. Token and tool-call breaches continue to emit
  `BudgetExceeded { kind: Tokens | ToolCalls }` and terminate the
  node as before.
- Wall-clock is now the universal `timeout:` attribute from ADR 0017,
  applicable to every `NodeKind` (`agent`, `shell`). Breach emits
  `NodeTimedOut { limit, elapsed }` and the node settles with
  `NodeStatus::TimedOut` (ADR 0010 amendment via ADR 0017). This is
  a hard terminator, not a strictness-mode dimension — enforcement
  posture is always hard-fail and does not appear in the ADR 0016
  trajectory table.
- The other four dimensions (iteration, tool surface, effects,
  non-determinism) are unchanged. The five-dimension framing and the
  AgentPolicy aggregate remain the user-facing contract; dimension
  4's internal membership shrank but its hard-fail guarantee is
  intact.

Rationale: wall-clock is orthogonal to the "agent-internal budget"
concept — a shell node benefits from the same deadline shape, and
modeling timeout as an agent-only budget forced a second enforcement
path once shell nodes started needing it. The ADR 0017 brainstorm
surfaced this as a category error and moved timeout to where it
belongs.
