# Multi-agent PR review

This tutorial builds a multi-agent pipeline: a parent reviewer delegates to three specialist subagents (security, performance, docs), each running its own bounded loop, and the parent synthesizes their findings into a single verdict. By the end you'll know how to compose agents, how subagent depth is bounded, and how the parent–child compose-down rule prevents a child from doing more than its parent.

**Time:** 25 minutes. **Prerequisites:** Completed [Your first pipeline](first-pipeline.md). An `OPENROUTER_API_KEY`. `git` available, with a checkout that has commits ahead of `origin/main`.

## What you'll build

A pipeline with:

- One **shell node** that collects a unified diff via `git diff`.
- One **parent agent** (`pr_reviewer`) that delegates to three specialists.
- Three **child agents** (`security_lens`, `performance_lens`, `docs_lens`), each read-only, each scoped to one concern.
- A final JSON verdict produced by the parent.

This is the shape orno is designed for: bounded leaves, recursive delegation, no peer-to-peer messaging.

## Step 1 — Understand the multi-agent model

orno's "multi-agent" is **recursive single-agent loops**, not peer-to-peer messaging. Practically:

- A parent agent calls a child via the `subagent.<name>` tool.
- The child runs its own bounded loop with its own policy.
- The child returns its final assistant message to the parent like any other tool result.
- There are no sibling messages, no shared blackboard, no broadcast.

This is the same pattern as the Claude Agent SDK's subagent tool. It maps onto a **tree** of bounded specialists, not a swarm.

The compose-down rule: a child cannot relax its parent's effect policy. A read-only parent (`allow_mutations: false`) cannot delegate to a mutating child. Pipeline load rejects the configuration before the run starts.

## Step 2 — Read the example pipeline

```bash
cd <path-to-orno>
cat examples/pr-review/pipeline.yaml
```

Study the structure:

```yaml
version: 1

vars:
  pr_number: "{{ env.PR_NUMBER }}"
  base_ref: origin/main

pass_env:
  - PR_NUMBER

mcp_servers:
  filesystem:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-filesystem", "/workspace"]

agents:
  pr_reviewer:
    model: anthropic/claude-sonnet-4.5
    provider: openrouter
    system: |
      You are a lead PR reviewer. You do not read code directly — you
      delegate to specialist lens subagents (security, performance, docs)
      and synthesize their findings.
      ...
    allowed_tools:
      - "subagent.security_lens"
      - "subagent.performance_lens"
      - "subagent.docs_lens"
    policy:
      max_iterations: 10
      max_total_tokens: 40000
      max_tool_calls: 12
      max_subagent_depth: 1
      allow_mutations: false
      allow_network: false
      on_parse_error: retry_once

  security_lens:
    # ... read-only, no network, max_subagent_depth: 0

  performance_lens:
    # ... read-only, no network, max_subagent_depth: 0

  docs_lens:
    # ... read-only, no network, max_subagent_depth: 0

nodes:
  - id: collect_diff
    kind: shell
    command: git
    args: ["diff", "--unified=3", "{{ vars.base_ref }}...HEAD"]

  - id: review
    kind: agent
    agent: pr_reviewer
    initial_prompt: |
      Review PR #{{ vars.pr_number }} against {{ vars.base_ref }}.
      Diff:
      {{ nodes.collect_diff.stdout }}
      ...
    needs: [collect_diff]
```

Three things to notice:

1. **`allowed_tools`** on the parent lists `subagent.<name>` entries — this is how the parent gets permission to delegate.
2. **`max_subagent_depth: 1`** on the parent caps how deep the recursion goes. The lens agents have `max_subagent_depth: 0` so they cannot recurse further.
3. **`pass_env: [PR_NUMBER]`** opts into reading one specific environment variable. orno does not expose the host environment by default.

## Step 3 — Walk the compose-down rule

Try modifying one of the lens agents:

```yaml
security_lens:
  allowed_tools: [Read, Edit]   # Add Edit
  policy:
    allow_mutations: true        # And enable mutations
```

Now run validation:

```bash
orno validate examples/pr-review/pipeline.yaml
```

You'll get a typed `ChildExceedsParentPolicy` error: the parent (`pr_reviewer`) has `allow_mutations: false`, but the child (`security_lens`) has `allow_mutations: true`. Pipeline load rejects this before any LLM is invoked.

Revert the change. The compose-down rule is what makes "multi-agent in CI" auditable: a reviewer reading the parent's policy knows the upper bound on what the entire tree can do, because no descendant can escape it.

## Step 4 — Run the pipeline

Set up environment and secrets, then run:

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
export PR_NUMBER=482
orno run examples/pr-review/pipeline.yaml --secrets-file .env.secrets
```

The event stream will show:

1. `run_started` — opens the run.
2. `mcp_server_started` — the filesystem MCP server is spawned.
3. `node_started` (id: `collect_diff`) → `node_finished` — git diff captured to `nodes.collect_diff.stdout`.
4. `node_started` (id: `review`) — parent agent loop begins.
5. Parent's first iteration: `agent_iteration_started`, `llm_request_succeeded` (parent's first turn — likely emits a `subagent.security_lens` tool call).
6. `subagent_started` — child agent loop begins. Has its own `agent_iteration_*` events nested under it.
7. `subagent_finished` — returns child's final message.
8. Parent's next iteration consumes the child's result, emits the next subagent call.
9. Repeat for `performance_lens` and `docs_lens`.
10. Parent's final iteration synthesizes all three into the JSON verdict.
11. `node_finished` (id: `review`, `ok: true`).
12. `mcp_server_stopped` — MCP server shut down.
13. `run_finished`.

The parent's JSON verdict is in `nodes.review.output`.

## Step 5 — Read the verdict

Extract the verdict from the event stream:

```bash
orno run examples/pr-review/pipeline.yaml --secrets-file .env.secrets \
  | jq -r 'select(.event.type == "node_finished" and .event.node_id == "review") | .event.output' \
  | jq '.'
```

You'll get a structured verdict:

```json
{
  "verdict": "request_changes",
  "findings": [
    {
      "severity": "high",
      "lens": "security",
      "location": "src/auth.rs:142",
      "finding": "Session token logged at info level",
      "suggestion": "Replace with a token-id-only log entry"
    },
    ...
  ]
}
```

This is what makes the pattern useful: each lens contributes findings in a structured shape, and the parent enforces a uniform output. A consumer (a CI bot, a Slack notifier) can ingest the JSON without parsing prose.

## Step 6 — Inspect the recursion depth

Re-run with `--record-bundle` and inspect the bundle:

```bash
orno run examples/pr-review/pipeline.yaml \
  --secrets-file .env.secrets \
  --record-bundle pr-review.bundle.ndjson

jq -c 'select(.event.type | startswith("subagent_"))' pr-review.bundle.ndjson | head
```

You'll see `subagent_started` and `subagent_finished` envelopes for each lens, with `parent_node_id` pointing back to `review`. The `depth` field shows how deep in the recursion this child is — `1` for our lenses (one level below the root).

If you tried to set `max_subagent_depth: 2` on the parent and a lens *also* delegated, that lens's call would fail with `SubagentDepthExceeded` returned to it as a tool denial — non-terminal, the parent can recover. Only the bound is enforced; the child can decide what to do with the denial.

## Step 7 — Adjust the budget

The parent has `max_total_tokens: 40000` for itself. A subagent's tokens are counted against the **child's** cap, not the parent's. The parent's loop sees the child's spend reflected as a single tool result.

If you reduce the parent's budget too far:

```yaml
pr_reviewer:
  policy:
    max_total_tokens: 5000
```

You'll get a `BudgetExceeded { kind: Tokens }` mid-run as the parent terminates. The lens subagents that already returned results stay in the partial bundle; the parent's final synthesis is missing.

This is the cost ceiling working: each agent's budget is enforced independently, and a runaway agent cannot bankrupt others.

## What you've learned

- Multi-agent in orno is recursive single-agent loops via `subagent.<name>`.
- The compose-down rule prevents children from doing more than their parents.
- `max_subagent_depth` caps recursion; `0` disables subagents entirely.
- Each agent has its own token and tool-call budget; budgets are checked independently.
- A subagent's final assistant message returns to the parent like any other tool result.

## Next steps

- [How to add an MCP server](../how-to/add-mcp-server.md) — stdio + streamable-HTTP recipes.
- [How to debug a failure](../how-to/debug-failure.md) — reading the event stream and isolating the failure.
- [Strict agentic loops › Multi-agent without peer-to-peer](../explanation/strict-agentic-loops.md#multi-agent-without-peer-to-peer) — why orno chose this model.
