# Strict agentic loops

This page explains what makes orno's runtime contract *strict*, why each dimension is shaped the way it is, and which trade-offs are deliberate. The [reference docs](../reference/cli.md) cover *what* every surface accepts; this page covers *why*.

## The problem

A vendor SDK gives you an LLM client. It does not give you:

- A limit on how many times the model can loop before terminating.
- A closed set of tools the model is allowed to call.
- A gate on which side-effects those tools can produce.
- A budget on tokens, tool calls, or recursion depth.
- A way to replay a run byte-for-byte.

In an interactive setting (Claude Desktop, an IDE assistant) those gaps are filled by a human in the loop. In CI, there is no human in the loop. A pipeline that calls an agent at 3 AM has to declare its bounds *before* the model runs, not after the model has done something unexpected.

orno fills exactly those gaps. Five guarantees, all enforced at runtime, all required, all observable on a wire-format event log.

## The five dimensions

### 1. Bounded iteration

Every agent loop has a hard cap on turns. `policy.max_iterations: 10` means the loop will execute at most ten cycles of *send context → receive response → run tools → append results*. An overrun terminates the node with `IterationLimitExceeded` and that variant lands typed on `node_finished.failure` so a downstream consumer can branch on it.

Why a cap and not a "soft warning"? A model that returns a tool-call turn on iteration *N* expects the loop to continue to *N+1*. A capped loop is the only way to guarantee that a degenerate case — model loops on a never-resolved tool error, model emits a self-referential plan it cannot complete — terminates in finite time without operator intervention.

The cap is per-agent, not per-pipeline. A subagent gets its own `max_iterations` bound, and a node that calls subagents has the parent's `max_iterations` budget to spend on its own turns plus subagent dispatches counted against `max_subagent_depth` and `max_total_tokens` instead.

### 2. Bounded tool surface

`allowed_tools` is the closed set the model may call. It is enumerated explicitly: `Bash`, `Read`, `Edit`, `Write`, `WebFetch`, `SetState`, plus optional `mcp.<server>.<tool>` and `subagent.<name>` entries. There is no "all tools" wildcard, no "default tools" inheritance, and no implicit additions from any source.

Calling a tool not in `allowed_tools` is **terminal**: `Event::UnknownToolCalled` fires, the agent loop returns `AgentError::UnknownToolCalled`, and the node ends with `ok: false`. There is no retry path. The loop does not recover by feeding the error back to the model — termination is the whole point. A model emitting an unexpected tool name is signaling that its understanding of the task and its understanding of the tool surface have diverged; the right move is to stop.

This is the dimension that makes "what could this pipeline do?" auditable. `orno plan` reads `allowed_tools` directly; the operator approving a pipeline is reading the same closed list the runtime will enforce.

### 3. Bounded effects

Where `allowed_tools` controls *which tools the model can call*, the effect-class system controls *what those tools can do*:

- `policy.allow_mutations: false` denies any tool with effect `Mutations` or `MutationsAndNetwork` — `Edit`, `Write`, `Bash`, every MCP tool.
- `policy.allow_network: false` denies any tool with effect `Network` or `MutationsAndNetwork` — `WebFetch`, every MCP tool.
- `policy.allow_context_writes: false` denies the `SetState` builtin specifically.
- `policy.allowed_domains` and `policy.blocked_domains` filter network-capable tools at the URL level.

Effect denials are **non-terminal**. `Event::ToolDenied` fires, a denial string is fed back to the model as the tool's result, and the loop continues. The model gets to recover, ask the operator a question, or pick a different tool. Terminating on every denial would force every agent into a "permission dance" pattern; treating denials as content lets the agent reason about them.

The asymmetry between `UnknownToolCalled` (terminal) and `ToolDenied` (non-terminal) is deliberate: an unknown tool is an *integrity* failure (the model and the runtime disagree about reality), while a denied tool is a *capability* mismatch (the model wanted to do something it isn't authorized to do — that's recoverable).

Effect classes are conservative on MCP tools. orno cannot inspect a remote server's per-tool semantics at registration time, so every MCP tool is classified `MutationsAndNetwork`. If an MCP server's `tools/list` advertises a "read-only filesystem" tool, orno treats it as both mutating and networked anyway. The conservative classification is a feature: it forces the operator to declare *both* `allow_mutations` and `allow_network` before any MCP call lands, so they are explicitly acknowledging the worst case.

### 4. Bounded resources

Three knobs:

- `max_total_tokens` — sum of every LLM call's token usage in this agent's loop. Subagent tokens are counted against the *child's* cap, which cannot exceed the parent's remaining budget. The parent's loop sees the child's spend reflected in its own usage when the child returns.
- `max_tool_calls` — count of every attempted tool call, including blocked ones and subagent dispatches. Counting blocked calls means a malicious or confused model cannot exhaust the budget by spamming denied calls — it costs the same.
- `max_subagent_depth` — how deep the recursion can go. `0` disables subagents entirely. Pipeline-load enforcement guarantees a child agent's recursion bound is at most `parent.max_subagent_depth - 1`, so nested authorization doesn't leak.

Wall-clock is **not** an agent-policy field. It is a node-level `timeout:` attribute that applies uniformly to `agent` and `shell` nodes. A wall-clock budget is an executor concern, not a loop concern — putting it on the agent policy would suggest that a `shell` node can't time out, which is wrong.

A budget breach surfaces as a typed `BudgetKind` (`Tokens` or `ToolCalls`) on `node_finished.failure.budget_kind`. Downstream alerting can branch on the dimension that breached without parsing the human-readable error message.

### 5. Bounded non-determinism

The four dimensions above bound what *can happen* in a run. The fifth bounds what *did happen* — every external interaction (every LLM request and response, every tool call and response, every MCP exchange) can be captured into a bundle and replayed exactly.

`orno run --record-bundle run.ndjson` writes the bundle. `orno replay run.ndjson` re-executes the pipeline against the bundle's tapes — no live LLM, no network, no MCP server spawning. A tape miss during replay is a **hard error**, not a fallback to the live API. The replay either reproduces the original run byte-for-byte or fails loudly.

Why is this load-bearing rather than convenient? Because the other four dimensions only constrain *future* runs. Without record/replay, a pipeline that misbehaved last night cannot be examined today without a fresh LLM call — and the LLM is non-deterministic. Replay is what lets a postmortem on a failed run examine the actual bytes the model emitted, the actual tool results that came back, the actual ordering of events, without spending tokens or risking divergence. Replay is also what lets CI re-test a pipeline against a recorded bundle as an integration test.

Tape misses being hard errors is a deliberate sharp edge. A "fall back to live API on miss" implementation would silently degrade replay's guarantee — you'd think you were replaying when you were actually re-running. Better to fail and force the operator to reconcile.

## Multi-agent without peer-to-peer

orno's multi-agent model is **recursive single-agent loops**, not peer-to-peer message passing. A parent agent calls a child via the `subagent.<name>` tool. The child runs its own bounded loop with its own policy, and returns its final assistant message to the parent like any other tool result. There are no sibling channels, no shared blackboard, no broadcast.

This is the same pattern Claude Code's `subagent` tool uses. The reasons it's the right model:

- **One responsibility per agent.** A "lens" pattern — one agent per perspective (security, performance, style) — is naturally tree-shaped. Sibling messaging would invite agents to coordinate, which means coordinating means re-deriving each other's context, which means token cost squared.
- **Bounded composition.** A child cannot relax its parent's `allow_mutations` or `allow_network`. A read-only parent cannot delegate to a mutating child — pipeline load rejects the configuration. Composition cannot escape strictness; only narrow it.
- **Replayable hierarchy.** The bundle records the parent's prompt to the child and the child's final message back. Replay reconstructs the entire tree without needing to model peer-to-peer coordination.

The cost is that emergent multi-agent collaboration patterns (debate, market mechanisms, election protocols) don't fit. orno is opinionated against them — for CI use, you want a tree of bounded specialists, not a swarm.

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

The asymmetry maps cleanly onto a question: *did the model do something it was authorized to attempt?* If yes (denied tool, blocked domain), the loop continues. If no (unknown tool, parse failure with `fail`), it terminates.

## DAG scheduling vs agent loops

orno's parallelism is at the **DAG level**, not inside the agent loop. Two `kind: agent` nodes with no `needs:` between them run concurrently. A single agent loop, on the other hand, executes its own tool calls **serially** in the order the model emitted them — even when the model emits multiple tool calls in a single assistant turn.

Why? Because parallel tool calls inside a loop introduce ordering ambiguity. If the model emits `Read` and `Bash` together, was `Bash` supposed to see the file `Read` returned? Some calls are side-effect-free (`Read`); some are not (`Bash`); some are subagent dispatches with their own loops. Serial execution in declaration order is the only ordering the model could have meant unambiguously. Operators who want parallelism declare it at the DAG level, where the order is explicit.

This is why an agent's `policy.max_tool_calls` budget is sufficient to bound cost — there is no hidden parallelism to account for.

## What strict isn't

- **Not "safe"** — strict bounds tell you what the agent *can* do; they don't make a destructive operation harmless. An agent with `Bash` and `allow_mutations: true` can `rm -rf /` because you told it it could. The contract is *honesty*, not safety. Operators who want sandboxing should run orno inside a container.
- **Not "interactive"** — strict bounds are batch contracts. There is no "ask the user" tool. A run either completes, fails, or times out; it does not pause for input.
- **Not "best-effort"** — every bound is enforced runtime. There is no "warning mode" or "permissive flag." If you don't want a bound, don't declare it (e.g., `max_total_tokens: u64::MAX` is a no-budget shape).
- **Not "agent autonomy"** — autonomy is what the operator authorizes via the policy block. Strict mode is the absence of *implicit* autonomy. An empty `allowed_tools` and zero budgets is a perfectly legal pipeline; it just won't accomplish much.

## See also

- [Pipeline YAML › `policy` semantics](../reference/pipeline-yaml.md#policy-semantics) — every knob, every default.
- [Tools › Effect classes](../reference/tools.md#effect-classes) — the gate the policy fields control.
- [Events](../reference/events.md) — the wire format that makes every breach observable.
- [Errors](../reference/errors.md) — the typed enums behind every termination.
- [FAQ](../faq.md) — short-form answers to "why isn't there a *foo*."
