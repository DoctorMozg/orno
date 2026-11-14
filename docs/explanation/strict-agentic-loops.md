# Strict agentic loops

This page explains what makes orno's runtime contract *strict*, why each dimension has the shape it has, and which trade-offs are deliberate. The [reference docs](../reference/cli.md) cover *what* every surface accepts. This page covers *why*.

## The problem

A vendor SDK gives you an LLM client. It does not give you:

- A limit on how many times the model can loop before terminating.
- A closed set of tools the model is allowed to call.
- A gate on which side-effects those tools can produce.
- A budget on tokens, tool calls, or recursion depth.
- A way to replay a run byte-for-byte.

In an interactive setting (Claude Desktop, an IDE assistant) a human in the loop fills those gaps. In CI nobody is there. A pipeline that calls an agent at 3 AM has to declare its bounds *before* the model runs, not after the model has done something unexpected.

orno fills exactly those gaps with five guarantees. All are enforced at runtime, all are required, and all are observable on a wire-format event log.

## The five dimensions

### 1. Bounded iteration

Every agent loop has a hard cap on turns. `policy.max_iterations: 10` means the loop runs at most ten cycles of sending context, receiving a response, running tools and appending results. An overrun terminates the node with `IterationLimitExceeded`, and that variant lands typed on `node_finished.failure` so a downstream consumer can branch on it.

Why a cap and not a "soft warning"? A model that returns a tool-call turn on iteration *N* expects the loop to continue to *N+1*. Only a capped loop guarantees that a degenerate case terminates in finite time without operator intervention. Two examples: the model loops on a tool error it never resolves, or it emits a self-referential plan it can't complete.

The cap is per-agent, not per-pipeline. A subagent gets its own `max_iterations` bound. A node that calls subagents spends the parent's `max_iterations` budget on its own turns only, and the subagent dispatches count against `max_subagent_depth` and `max_total_tokens` instead.

### 2. Bounded tool surface

`allowed_tools` is the closed set the model may call. It is enumerated explicitly: `Bash`, `Read`, `Edit`, `Write`, `WebFetch`, `SetState`, plus optional `mcp.<server>.<tool>` and `subagent.<name>` entries. There is no "all tools" wildcard, no "default tools" inheritance, and no implicit additions from any source.

Calling a tool that isn't in `allowed_tools` is **terminal**. The agent loop returns `AgentError::UnknownToolCalled` and the node ends with `ok: false`. There is no retry path, and the loop doesn't feed the error back to the model. Termination is the whole point. A model that emits an unexpected tool name has a picture of the task and a picture of the tool surface that no longer match, so the right move is to stop.

This is what makes "what could this pipeline do?" auditable. `orno plan` reads `allowed_tools` directly, so the operator approving a pipeline reads the same closed list the runtime will enforce.

### 3. Bounded effects

`allowed_tools` controls *which tools the model can call*. The effect-class system controls *what those tools can do*:

- `policy.allow_mutations: false` denies any tool with effect `Mutations` or `MutationsAndNetwork`: `Edit`, `Write`, `Bash`, every MCP tool.
- `policy.allow_network: false` denies any tool with effect `Network` or `MutationsAndNetwork`: `WebFetch`, every MCP tool.
- `policy.allow_context_writes: false` denies the `SetState` builtin specifically.
- `policy.allowed_domains` and `policy.blocked_domains` filter network-capable tools at the URL level.

Effect denials are **non-terminal**. `Event::ToolDenied` fires, a denial string goes back to the model as the tool's result, and the loop continues. The model can recover, ask the operator a question, or pick a different tool. If every denial terminated the run, every agent would end up in a "permission dance." Treating denials as content lets the agent reason about them.

The asymmetry between `UnknownToolCalled` (terminal) and `ToolDenied` (non-terminal) is deliberate. An unknown tool is an *integrity* failure: the model and the runtime disagree about reality. A denied tool is a *capability* mismatch: the model wanted something it isn't authorized to do, and that's recoverable.

Effect classes are conservative on MCP tools. orno can't inspect a remote server's per-tool semantics at registration time, so it classifies every MCP tool as `MutationsAndNetwork`. Even if an MCP server's `tools/list` advertises a "read-only filesystem" tool, orno treats it as both mutating and networked. That's intentional. The operator has to declare *both* `allow_mutations` and `allow_network` before any MCP call lands, which means acknowledging the worst case explicitly.

### 4. Bounded resources

Three knobs:

- `max_total_tokens` is the sum of every LLM call's token usage in this agent's loop. Subagent tokens count against the *child's* cap, which cannot exceed the parent's remaining budget. The parent's loop sees the child's spend reflected in its own usage when the child returns.
- `max_tool_calls` counts every attempted tool call, including blocked ones and subagent dispatches. Blocked calls count so that a malicious or confused model can't exhaust the budget by spamming denied calls. A denied call costs the same as any other.
- `max_subagent_depth` sets how deep the recursion can go. `0` disables subagents entirely. Pipeline-load enforcement guarantees a child agent's recursion bound is at most `parent.max_subagent_depth - 1`, so nested authorization doesn't leak.

Wall-clock is **not** an agent-policy field. It's a node-level `timeout:` attribute that applies uniformly to `agent` and `shell` nodes. A wall-clock budget is an executor concern, not a loop concern. Putting it on the agent policy would suggest a `shell` node can't time out, which is wrong.

A budget breach surfaces as a typed `BudgetKind` (`Tokens` or `ToolCalls`) on `node_finished.failure.budget_kind`. Downstream alerting can branch on the dimension that breached without parsing the human-readable error message.

### 5. Bounded non-determinism

The first four dimensions bound what *can happen* in a run. The fifth bounds what *did happen*. Every external interaction (every LLM request and response, every tool call and response, every MCP exchange) can be captured into a bundle and replayed exactly.

`orno run --record-bundle run.ndjson` writes the bundle. `orno replay run.ndjson` re-executes the pipeline against the bundle's tapes, with no live LLM, no network, and no MCP server spawning. A tape miss during replay is a **hard error**, not a fallback to the live API. The replay either reproduces the original run byte-for-byte or fails loudly.

This one carries real weight, because the other four dimensions only constrain *future* runs. Without record/replay, a pipeline that misbehaved last night can't be examined today without a fresh LLM call, and the LLM is non-deterministic. Replay lets a postmortem look at the actual bytes the model emitted, the actual tool results that came back, and the actual ordering of events, without spending tokens or risking divergence. It also lets CI re-test a pipeline against a recorded bundle as an integration test.

Making tape misses hard errors is a deliberate sharp edge. A "fall back to live API on miss" implementation would silently weaken the guarantee: you'd think you were replaying when you were actually re-running. Better to fail and make the operator reconcile.

## Multi-agent without peer-to-peer

orno's multi-agent model is **recursive single-agent loops**, not peer-to-peer message passing. A parent agent calls a child via the `subagent.<name>` tool. The child runs its own bounded loop with its own policy and returns its final assistant message to the parent like any other tool result. There are no sibling channels, no shared blackboard, no broadcast.

Claude Code's `subagent` tool uses the same pattern. Three reasons it's the right model:

- **One responsibility per agent.** A "lens" pattern, with one agent per perspective (security, performance, style), is naturally tree-shaped. Sibling messaging would invite agents to coordinate, and coordinating means re-deriving each other's context, which means token cost squared.
- **Bounded composition.** A child can't relax its parent's `allow_mutations` or `allow_network`. A read-only parent can't delegate to a mutating child, and pipeline load rejects the configuration. Composition can only narrow strictness, never escape it.
- **Replayable hierarchy.** The bundle records the parent's prompt to the child and the child's final message back. Replay reconstructs the entire tree without modeling peer-to-peer coordination.

The cost is that emergent multi-agent patterns (debate, market mechanisms, election protocols) don't fit. orno is opinionated against them. For CI you want a tree of bounded specialists, not a swarm.

## Termination vs continuation

Strict-mode breaches fall into two camps:

| Breach                         | Effect                                      | Why                                                                                                        |
| ------------------------------ | ------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| `UnknownToolCalled`            | Terminate the node.                         | Model and runtime disagree about the tool surface. Not recoverable.                                        |
| `IterationLimitExceeded`       | Terminate the node.                         | Loop did not converge. Continuing would just hit the same wall.                                            |
| `BudgetExceeded` (tokens/calls)| Terminate the node.                         | Hard resource ceiling. Pre-budgeted spend is the contract.                                                 |
| `ParseFailed` (with `fail`)    | Terminate the node.                         | Operator chose strict; honor the choice.                                                                   |
| `Llm` transport error          | Terminate the node.                         | Cannot make progress without the model.                                                                    |
| `ToolDenied` (effect/domain)   | Continue the loop, feed denial to model.    | Capability mismatch is recoverable content, not an integrity failure.                                       |
| `SubagentDepthExceeded`        | Continue the loop, feed denial to parent.   | Same — capability mismatch.                                                                                |
| `ParseFailed` (with `retry_once`) | Retry once, then terminate on second.    | One retry is the operator-approved escape hatch. Two parses failing in a row is integrity, not capability. |

The split comes down to one question: *did the model attempt something it was authorized to attempt?* If yes (denied tool, blocked domain), the loop continues. If no (unknown tool, parse failure with `fail`), it terminates.

## DAG scheduling vs agent loops

orno schedules at the **DAG level**, not inside the agent loop. Today the engine runs nodes one at a time: two `kind: agent` nodes with no `needs:` between them still run serially, and a failure in one branch doesn't stop the other. A single agent loop, on the other hand, executes its own tool calls **serially** in the order the model emitted them, even when the model emits several tool calls in one assistant turn.

Parallel tool calls inside a loop would make ordering ambiguous. If the model emits `Read` and `Bash` together, was `Bash` supposed to see the file `Read` returned? Some calls are side-effect-free (`Read`), some aren't (`Bash`), and some are subagent dispatches with their own loops. Serial execution in declaration order is the only ordering the model could unambiguously have meant. Ordering between steps belongs at the DAG level, where it's explicit.

That's also why an agent's `policy.max_tool_calls` budget is enough to bound cost. There's no hidden parallelism to account for.

## What strict isn't

- **Not "safe."** Strict bounds tell you what the agent *can* do. They don't make a destructive operation harmless. An agent with `Bash` and `allow_mutations: true` can `rm -rf /` because you told it it could. The contract is *honesty*, not safety. If you want sandboxing, run orno inside a container.
- **Not "interactive."** Strict bounds are batch contracts. There is no "ask the user" tool. A run completes, fails, or times out, and it never pauses for input.
- **Not "best-effort."** Every bound is enforced at runtime. There is no "warning mode" or "permissive flag." If you don't want a bound, don't declare it (e.g., `max_total_tokens: u64::MAX` is a no-budget shape).
- **Not "agent autonomy."** The operator authorizes autonomy through the policy block. Strict mode is the absence of *implicit* autonomy. An empty `allowed_tools` and zero budgets is a perfectly legal pipeline, though it won't accomplish much.

## See also

- [Pipeline YAML › `policy` semantics](../reference/pipeline-yaml.md#policy-semantics): every knob, every default.
- [Tools › Effect classes](../reference/tools.md#effect-classes): the gate the policy fields control.
- [Events](../reference/events.md): the wire format that makes every breach observable.
- [Errors](../reference/errors.md): the typed enums behind every termination.
- [FAQ](../faq.md): short-form answers to "why isn't there a *foo*."
