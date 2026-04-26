# How to tighten the budget

Budget knobs are how you bound cost before any spend. This guide shows how to size each knob, how to detect breaches early, and how to reduce a runaway pipeline to a known ceiling.

## The four budget knobs

Every agent has four budget-related fields under `policy:`:

| Knob                  | Bounds                                                  | Breach event                          |
| --------------------- | ------------------------------------------------------- | ------------------------------------- |
| `max_iterations`      | Loop turns (model → tools → loop)                       | `IterationLimitExceeded`              |
| `max_total_tokens`    | Sum of all LLM token usage in this agent's loop         | `BudgetExceeded { kind: Tokens }`     |
| `max_tool_calls`      | Count of every attempted tool call (incl. denied ones)  | `BudgetExceeded { kind: ToolCalls }`  |
| `max_subagent_depth`  | Recursion depth for `subagent.<name>` calls             | `SubagentDepthExceeded` (denial)      |

A fifth, related, knob lives at the **node** level:

| Knob       | Bounds                                                  | Breach event   |
| ---------- | ------------------------------------------------------- | -------------- |
| `timeout:` | Wall-clock deadline (applies to agent and shell nodes)  | `NodeTimedOut` |

## Step 1 — Use `orno plan` to see the worst case

Before running anything, get a worst-case ceiling for the entire pipeline:

```bash
orno plan pipeline.yaml | jq '.'
```

The `plan_summary` line gives the totals across all nodes:

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

This is the ceiling. The actual run will spend less; it will never spend more. Treat `max_tokens_total` as the upper bound for cost estimation.

## Step 2 — Size each knob

The right approach is "set bounds slightly above your observed peak." Recommended starting points by agent shape:

### Cheap, tool-free agent (greeter, classifier, summarizer)

```yaml
policy:
  max_iterations: 1
  max_total_tokens: 5_000
  max_tool_calls: 0
  max_subagent_depth: 0
```

One turn, no tools. Maximum spend: a single LLM round-trip.

### Read-only agent with WebFetch / Read

```yaml
policy:
  max_iterations: 6
  max_total_tokens: 30_000
  max_tool_calls: 15
  max_subagent_depth: 0
```

Up to six turns; expects to do at most a handful of reads or fetches per turn. Headroom for a few false starts.

### Mutating agent (Edit, Write, Bash)

```yaml
policy:
  max_iterations: 20
  max_total_tokens: 200_000
  max_tool_calls: 80
  max_subagent_depth: 0
```

Coding-style workloads need iteration headroom — the model often probes the codebase for several turns before committing changes.

### Parent reviewer with subagents

```yaml
policy:
  max_iterations: 10
  max_total_tokens: 50_000
  max_tool_calls: 12
  max_subagent_depth: 1
```

Parent's `max_total_tokens` covers only its own LLM turns. Subagent token spend is counted against the **child's** `max_total_tokens`, not the parent's.

## Step 3 — Watch the actual spend

During a run, every `agent_iteration_finished` envelope reports the cumulative usage so far:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  | jq -c 'select(.event.type == "agent_iteration_finished") | {iter: .event.iteration, tokens: .event.cumulative_tokens, calls: .event.cumulative_tool_calls}'
```

You'll see the trajectory:

```json
{"iter":1,"tokens":2150,"calls":2}
{"iter":2,"tokens":4982,"calls":5}
{"iter":3,"tokens":8123,"calls":7}
```

If the trajectory is steeper than you expected, the bound will trip eventually. Decide whether to raise the bound or fix the prompt.

## Step 4 — Read a budget breach

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
    "node_id": "review",
    "ok": false,
    "failure": {
      "kind": "BudgetExceeded",
      "budget_kind": "Tokens",
      "limit": 30000,
      "consumed": 30214
    }
  }
}
```

The `budget_kind` discriminates `Tokens` vs. `ToolCalls`, so a downstream alerting system can branch on the dimension that breached without parsing the human-readable error message.

For `IterationLimitExceeded`:

```json
{
  "event": {
    "type": "node_finished",
    "node_id": "review",
    "ok": false,
    "failure": {
      "kind": "IterationLimitExceeded",
      "limit": 10,
      "iterations": 10
    }
  }
}
```

## Step 5 — Tighten an over-provisioned bound

Once you have a few representative runs, tighten the bounds. Rule of thumb:

```
new_bound = ceil(observed_p99 × 1.2)
```

A 20% headroom over the p99 catches reasonable variance without leaving slack a runaway model can exploit.

For example, if your reviewer never spent more than 24,000 tokens across 50 representative runs:

```yaml
policy:
  max_total_tokens: 30_000      # ceil(24_000 * 1.2) = 28_800, round up
```

A run that suddenly spends 50,000 tokens trips the budget and terminates instead of completing — that's the desired behavior. Cost is bounded; investigation can find why the spend spiked.

## Step 6 — Wall-clock as a separate dimension

`policy` has no `max_wall_clock` field — by design. Wall-clock is a node-level attribute:

```yaml
nodes:
  - id: review
    kind: agent
    agent: reviewer
    timeout: 300         # seconds; applies to agent and shell nodes alike
```

Why separate? Because wall-clock is an executor concern, not an agent-loop concern. A shell node can also time out; the field lives on the node, not the policy.

A timeout fires `Event::NodeTimedOut` and ends the node with `ok: false`. It does **not** fire when an agent is mid-LLM-call; orno polls for the deadline between turns. To kill mid-call you need OS-level signals (e.g., `timeout` wrapper around `orno run`).

## Step 7 — Counting denied calls

`max_tool_calls` includes denied calls. A model that emits 20 forbidden tool names burns 20 tool-call budget entries even though none of them executed. This is intentional: a malicious or confused model cannot exhaust the budget by spamming denied calls without paying for them.

If you're seeing budget breaches with no successful tool calls:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  | jq -c 'select(.event.type == "tool_denied")'
```

You'll see the denials. The model is calling forbidden tools — usually because the prompt didn't tell it which tools it has, or because the tool surface is misconfigured. Fix the prompt or the surface, not the budget.

## Step 8 — Subagent budget composition

A subagent's spend does **not** count against its parent's `max_total_tokens`. Each agent has its own budget. The parent's loop sees the child's spend reflected as a single tool result.

This means a parent with `max_total_tokens: 10_000` can dispatch three subagents that each spend 30,000 tokens, for an effective tree spend of 100,000 tokens. To bound the tree's total spend, set each child's budget conservatively.

A useful heuristic for trees: `parent_budget + (subagent_count × child_budget)` is the worst case. `orno plan` reports `max_tokens_total` as the sum across all agents, which is exactly this number.

## Recipe 1 — emergency lockdown

You suspect a runaway agent. Reduce its budget to the absolute minimum:

```yaml
policy:
  max_iterations: 2
  max_total_tokens: 5_000
  max_tool_calls: 3
  max_subagent_depth: 0
```

This forces the agent to one or two productive turns. If it still trips the budget, the problem is the prompt, not the agent — re-prompt or re-architect.

## Recipe 2 — exploration session

You're iterating on a prompt and don't yet know the right bounds. Allow generous headroom and watch:

```yaml
policy:
  max_iterations: 50
  max_total_tokens: 500_000
  max_tool_calls: 200
  max_subagent_depth: 0
on_parse_error: retry_once
```

After 5–10 runs, tighten to the observed p99 + 20%.

## Recipe 3 — cost-capped CI run

A scheduled CI job needs a hard cost ceiling. Compute the worst-case cost from `orno plan`:

```bash
orno plan pipeline.yaml | jq -r '.[] | select(.type == "plan_summary") | .max_tokens_total'
# 130000
```

At ~$0.001 per 1000 tokens for a cheap model, that's $0.13 worst-case. If the budget needs to be lower, edit the policy fields and re-plan.

## Failure modes summary

| Failure                          | What you should do                                                |
| -------------------------------- | ------------------------------------------------------------------ |
| `BudgetExceeded { kind: Tokens }` | If unexpected, examine the token trajectory; usually a runaway loop. |
| `BudgetExceeded { kind: ToolCalls }` | Check for `tool_denied` storms — model is calling forbidden tools.   |
| `IterationLimitExceeded`         | The loop didn't converge; tighten the prompt or raise iterations.    |
| `SubagentDepthExceeded`          | Tree is too deep; recompose into a flatter shape.                    |
| `NodeTimedOut`                   | Wall-clock too tight, or the LLM provider is slow; raise `timeout:`. |

## See also

- [Pipeline YAML › `policy` semantics](../reference/pipeline-yaml.md#policy-semantics) — every policy field with its default.
- [Events › `agent_iteration_finished`](../reference/events.md#agent_iteration_finished) — the schema you read for usage telemetry.
- [Strict agentic loops › Bounded resources](../explanation/strict-agentic-loops.md#4-bounded-resources) — rationale.
