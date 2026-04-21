# ADR 0006 — Subagent-as-tool-call, no peer-to-peer multi-agent

- Status: accepted
- Date: 2026-04-21

## Context

Multi-agent frameworks (CrewAI, AutoGen, the academic literature on
swarm/debate/blackboard systems) build on peer-to-peer message passing
between cooperating agents. Claude Code does something different: it
runs a single-agent loop, and delegation is a `Task` tool whose
implementation spawns a fresh subagent with its own prompt, its own
tool subset, its own context window, runs that subagent's loop to
completion, and returns the result as a tool-call output.

This is not "multiple agents coordinating." It is recursive
single-agent loops where the outer loop treats the inner loop as an
opaque tool. For CI, delegation forms a tree ("analyze security,
analyze performance, summarize") not a swarm; the tree abstraction is
sufficient and dramatically simpler than peer-to-peer.

## Decision

Orno implements delegation as subagent-as-tool-call, not as
peer-to-peer multi-agent. The pattern:

- One `trait Agent` with `async fn run(messages, policy, ctx) ->
  AgentOutcome`.
- `LoopAgent` is the concrete impl (the five-dimension loop from
  ADR 0005).
- `SubagentHandler` implements `ToolHandler` (ADR 0008) by calling
  `target_agent.run(..)` recursively with a derived `RunContext`
  (incremented depth, reduced budget, new event span).
- YAML shape: a top-level `agents:` block defines named agent
  configurations. A parent agent exposes a subagent as a tool by
  listing `subagent.<agent-name>` in its `allowed_tools`. The tool
  takes `{ prompt: string }`; the parent-emitted prompt becomes the
  child's `initial_prompt` for a fresh agent run. At the wire, dots
  are rewritten to underscores (`subagent_security_lens`) to satisfy
  provider tool-naming constraints.
- Recursion is bounded by `max_subagent_depth` (per-agent policy).
  Exceeding it emits `SubagentDepthExceeded` and fails the tool call.

Budget and effect rules compose down:

- Child budget = `min(remaining_parent_budget, child_policy_budget)`
  per resource.
- Child `allow_mutations` / `allow_network` cannot be stricter than
  the parent's — a read-only parent cannot delegate to a mutating
  child. Enforced at pipeline validation time, not at runtime.
- Errors flow up as tool-call failure strings; the parent agent
  decides whether to retry, branch, or give up.

Lifecycle events: `SubagentStarted`, `SubagentCompleted`,
`SubagentFailed`, `SubagentDepthExceeded`.

Explicitly rejected: peer-to-peer messaging, shared blackboards,
voting/debate protocols, actor systems, `tokio::sync::mpsc` between
agents. "Multi-agent" behavior emerges from trait composition, not
from concurrency primitives.

## Consequences

- No scheduler, no actor framework, no channels are added for agent
  coordination. Parallelism lives at the pipeline DAG level, not
  inside or between agents.
- The event log is hierarchical for free: the parent emits
  `SubagentStarted` before `run(..)` and `SubagentCompleted` after,
  so all child events are bracketed by the parent's span. Replay
  sees a tree.
- Subagent calls in replay are just tool-call results at the parent
  level. Drilling into a subagent means replaying the child's
  recorded log independently — same mechanism at every depth.
- The parent agent does not know a tool is a subagent. Its input
  schema is typically `{ prompt: string }`; the model sees a
  function that takes a string and returns a string.
- Subagent calls serialize with all other tool calls (ADR 0005);
  parallel delegation is expressed as parallel DAG nodes at the
  pipeline level.
- A `SubagentHandler.mutates()` flag is computed at agent-build
  time as `any(child.tools.mutates)` transitively, so mutation
  policy is static and auditable without runtime introspection.
