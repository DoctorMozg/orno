# How to tighten the budget

Budget knobs bound cost before any spend happens. This guide covers how to size each knob, how to spot a breach early, and how to pin a runaway pipeline to a known ceiling.

## The four budget knobs

Every agent has four budget-related fields under `policy:`:

| Knob                  | Bounds                                                  | Breach event                          |
| --------------------- | ------------------------------------------------------- | ------------------------------------- |
| `max_iterations`      | Loop turns (model → tools → loop)                       | `IterationLimitExceeded`              |
| `max_total_tokens`    | Sum of all LLM token usage across the agent and its subagent tree | `BudgetExceeded { kind: Tokens }`     |
| `max_tool_calls`      | Count of every attempted tool call (incl. denied ones, and every subagent's) | `BudgetExceeded { kind: ToolCalls }`  |
| `max_subagent_depth`  | Recursion depth for `subagent.<name>` calls             | `SubagentDepthExceeded` (denial)      |

A fifth, related knob lives at the **node** level:

| Knob       | Bounds                                                  | Breach event   |
| ---------- | ------------------------------------------------------- | -------------- |
| `timeout:` | Wall-clock deadline (applies to agent and shell nodes)  | `NodeTimedOut` |

## Step 1: Use `orno plan` to see the worst case

Before running anything, get a worst-case ceiling for the whole pipeline:

```bash
orno plan pipeline.yaml | jq '.'
```

The `plan_summary` line gives totals across all nodes:

```json
{
  "type": "plan_summary",
  "total_nodes": 4,
  "agent_nodes": 3,
  "max_iterations_total": 30,
  "max_tokens_total": 130000,
  "max_tool_calls_total": 47,
  "mcp_servers": ["filesystem"],
  "dag_is_valid": true
}
```

That's the ceiling. A real run spends less and never more, so use `max_tokens_total` as the upper bound when you estimate cost.

## Step 2: Size each knob

Set each bound slightly above the peak you've observed. If you haven't observed anything yet, these starting points work by agent shape.

### Cheap, tool-free agent (greeter, classifier, summarizer)

```yaml
policy:
  max_iterations: 1
  max_total_tokens: 5_000
  max_tool_calls: 0
  max_subagent_depth: 0
```

One turn, no tools. The most it can spend is a single LLM round-trip.

### Read-only agent with WebFetch / Read

```yaml
policy:
  max_iterations: 6
  max_total_tokens: 30_000
  max_tool_calls: 15
  max_subagent_depth: 0
```

Up to six turns, with at most a handful of reads or fetches per turn. That leaves room for a few false starts.

### Mutating agent (Edit, Write, Bash)

```yaml
policy:
  max_iterations: 20
  max_total_tokens: 200_000
  max_tool_calls: 80
  max_subagent_depth: 0
```

Coding workloads need more iterations. The model often probes the codebase for several turns before it commits to a change.

### Parent reviewer with subagents

```yaml
policy:
  max_iterations: 10
  max_total_tokens: 50_000
  max_tool_calls: 12
  max_subagent_depth: 1
```

`max_total_tokens` and `max_tool_calls` cover the whole subagent tree. A child's spend and tool calls count against the parent and every ancestor, and the child's own cap applies on top. Iterations stay per loop.

## Step 3: Watch the actual spend

During a run, every `llm_response_received` envelope carries that call's `usage`. Pull the per-call numbers with `jq`:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  | jq -c 'select(.event.type == "llm_response_received") | {node: .event.node_id, tokens: .event.usage.total_tokens}'
```

`usage` is `null` when the provider omits token counts. The token budget can't charge such a response, and orno logs a warning.

The output shows the trajectory:

```json
{"node":"review","tokens":2150}
{"node":"review","tokens":2832}
{"node":"review","tokens":3141}
```

If it climbs faster than you expected, the bound will trip eventually. Decide whether to raise the bound or fix the prompt.

## Step 4: Read a budget breach

When a budget trips, the loop terminates and the node ends with a typed failure:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  | jq -c 'select(.event.type == "node_finished" and .event.ok == false)'
```

Output:

```json
{
  "event": {
    "type": "node_finished",
    "run_id": "run_...",
    "node_id": "review",
    "node_kind": "agent",
    "ok": false,
    "failure": {
      "kind": "budget_exceeded",
      "budget_kind": "tokens"
    }
  }
}
```

`budget_kind` is either `tokens` or `tool_calls`. A downstream alerting system can branch on it without parsing the human-readable error message.

For `IterationLimitExceeded`:

```json
{
  "event": {
    "type": "node_finished",
    "run_id": "run_...",
    "node_id": "review",
    "node_kind": "agent",
    "ok": false,
    "failure": {
      "kind": "iteration_limit_exceeded",
      "max_iterations": 10
    }
  }
}
```

## Step 5: Tighten an over-provisioned bound

Once you have a few representative runs, tighten the bounds. A rule of thumb:

```
new_bound = ceil(observed_p99 × 1.2)
```

Twenty percent over the p99 absorbs ordinary variance and leaves little slack for a runaway model to use.

Say your reviewer never spent more than 24,000 tokens across 50 representative runs:

```yaml
policy:
  max_total_tokens: 30_000      # ceil(24_000 * 1.2) = 28_800, round up
```

A run that suddenly spends 50,000 tokens trips the budget and terminates instead of completing. That's what you want: cost stays bounded, and you can investigate the spike afterward.

## Step 6: Wall-clock as a separate dimension

`policy` has no `max_wall_clock` field, by design. Wall-clock is a node-level attribute:

```yaml
nodes:
  - id: review
    kind: agent
    agent: reviewer
    timeout: 300         # seconds; applies to agent and shell nodes alike
```

Why separate? Wall-clock is an executor concern, not an agent-loop concern. Shell nodes can time out too, so the field lives on the node, not the policy.

A timeout fires `Event::NodeTimedOut` and ends the node with `ok: false`. The deadline wraps the whole node, so it also cuts off an LLM call or tool call still in flight, and it kills a shell node's process group.

## Step 7: Counting denied calls

`max_tool_calls` includes denied calls. A model that emits 20 forbidden tool names burns 20 tool-call budget entries even though none of them ran. This is intentional. A malicious or confused model can't exhaust the budget by spamming denied calls for free.

If you see budget breaches with no successful tool calls, look for denials:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  | jq -c 'select(.event.type == "tool_denied")'
```

The model is calling forbidden tools, usually because the prompt didn't say which tools it has or because the tool surface is misconfigured. Fix the prompt or the surface, not the budget.

## Step 8: Subagent budget composition

A subagent's spend **does** count against its parent. Tokens and tool calls are charged up the whole chain, so grandchild spend lands on the root. The tightest limit up the chain applies to each request.

The root's `max_total_tokens` therefore bounds the tree's total spend, and each child's own cap still bounds that child. `orno plan` reports `max_tokens_total` as the sum across agents, which is a looser ceiling than the root's cap.

## Recipe 1: emergency lockdown

You suspect a runaway agent. Cut its budget to the minimum:

```yaml
policy:
  max_iterations: 2
  max_total_tokens: 5_000
  max_tool_calls: 3
  max_subagent_depth: 0
```

That leaves the agent one or two productive turns. If it still trips the budget, the problem is the prompt, not the agent. Re-prompt or re-architect.

## Recipe 2: exploration session

You're iterating on a prompt and don't know the right bounds yet. Allow generous headroom and watch:

```yaml
policy:
  max_iterations: 50
  max_total_tokens: 500_000
  max_tool_calls: 200
  max_subagent_depth: 0
on_parse_error: retry_once
```

After 5–10 runs, tighten to the observed p99 + 20%.

## Recipe 3: cost-capped CI run

A scheduled CI job needs a hard cost ceiling. Get the worst-case token count from `orno plan`:

```bash
orno plan pipeline.yaml | jq -r 'select(.type == "plan_summary") | .max_tokens_total'
# 130000
```

At ~$0.001 per 1000 tokens for a cheap model, that's $0.13 worst-case. To go lower, edit the policy fields and re-plan.

## Failure modes summary

| Failure                          | What you should do                                                |
| -------------------------------- | ------------------------------------------------------------------ |
| `BudgetExceeded { kind: Tokens }` | If unexpected, examine the token trajectory; usually a runaway loop. |
| `BudgetExceeded { kind: ToolCalls }` | Check for `tool_denied` storms — model is calling forbidden tools.   |
| `IterationLimitExceeded`         | The loop didn't converge; tighten the prompt or raise iterations.    |
| `SubagentDepthExceeded`          | Tree is too deep; recompose into a flatter shape.                    |
| `NodeTimedOut`                   | Wall-clock too tight, or the LLM provider is slow; raise `timeout:`. |

## See also

- [Pipeline YAML › `policy` semantics](../reference/pipeline-yaml.md#policy-semantics): every policy field with its default.
- [Events › `llm_response_received`](../reference/events.md#llm_response_received): the schema you read for usage telemetry.
- [Strict agentic loops › Bounded resources](../explanation/strict-agentic-loops.md#4-bounded-resources): rationale.
