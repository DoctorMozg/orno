# Your first pipeline

This tutorial walks through writing, validating, and running the smallest pipeline that exercises a real agent loop. By the end you'll know what each block of a pipeline does and how to inspect a run's output.

**Time:** 10 minutes. **Prerequisites:** Rust toolchain (MSRV 1.95) installed and `orno` built. See [Install](../install.md) if you haven't.

## What you'll build

A one-node pipeline with a single agent that says hello to a target. No tools, no network, no MCP — just the bare minimum to prove the loop works.

## Step 1 — Write the pipeline

Save the following as `hello.yaml` somewhere convenient:

```yaml
version: 1

vars:
  target: README.md

agents:
  greeter:
    model: openai/gpt-5
    provider: openrouter
    system: "You are friendly."
    allowed_tools: []
    policy:
      max_iterations: 1
      max_total_tokens: 1000
      max_tool_calls: 0
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      on_parse_error: fail

nodes:
  - id: greet
    kind: agent
    agent: greeter
    initial_prompt: "Say hello to {{ vars.target }} in one sentence."
```

A quick tour:

- `version: 1` — pipeline schema version. Every pipeline starts with this.
- `vars:` — template variables. `{{ vars.target }}` interpolates anywhere a string is rendered.
- `agents.greeter` — defines a named agent. Pipelines can have multiple; each is referenced by name.
  - `model` and `provider` — which LLM to call. The default provider is `openrouter`, which fronts most providers.
  - `system` — the system prompt the model receives on every iteration.
  - `allowed_tools: []` — the closed set of tools this agent may call. Empty means none.
  - `policy.*` — the five strictness dimensions. We'll deconstruct these below.
- `nodes:` — the DAG. This pipeline has one node, `greet`, that runs the agent.

The five `policy` knobs:

| Knob                  | Meaning                                                                                |
| --------------------- | -------------------------------------------------------------------------------------- |
| `max_iterations: 1`   | The agent loop runs at most one cycle. After one model turn, the loop ends.            |
| `max_total_tokens`    | Total LLM tokens (input + output across all turns) capped at 1000.                     |
| `max_tool_calls: 0`   | Zero tool calls allowed. The agent can produce content but cannot invoke anything.     |
| `max_subagent_depth: 0` | No subagents. This agent cannot delegate.                                            |
| `allow_mutations`/`allow_network` | Both `false`. Even if a tool were on the surface, it couldn't mutate or reach the network. |
| `on_parse_error: fail` | If the model emits malformed tool-call syntax, terminate. (Other choice: `retry_once`.) |

## Step 2 — Validate without spending tokens

Before running, ask orno to load and check the pipeline:

```bash
orno validate hello.yaml
```

Validation parses the YAML, resolves template references, checks every tool name in `allowed_tools` is known, and verifies no agent's policy is internally inconsistent. It does **not** call the LLM.

If the file is well-formed you'll see no output and exit code `0`. If there's a problem (typo in a tool name, missing `version`, malformed policy), you'll get a typed error pointing at the offending field.

## Step 3 — Preview the worst case with `plan`

`orno plan` is the audit-style summary of what the pipeline *could* do:

```bash
orno plan hello.yaml
```

Output is two NDJSON lines on stdout:

```json
{"type":"plan_node","node_id":"greet","kind":"agent","depends_on":[],"timeout_secs":null,"agent_name":"greeter","model":"openai/gpt-5","provider":"openrouter","tools":[],"max_iterations":1,"max_total_tokens":1000,"max_tool_calls":0,"allow_mutations":false,"allow_network":false,"allowed_domains":[],"blocked_domains":[]}
{"type":"plan_summary","total_nodes":1,"agent_nodes":1,"shell_nodes":0,"agents_used":["greeter"],"max_iterations_total":1,"max_tokens_total":1000,"max_tool_calls_total":0,"mcp_servers":[],"dag_is_valid":true}
```

Every reviewable concern is here in one place: which model, which tools (none), what the agent can do (nothing destructive — `allow_mutations: false`, `allow_network: false`), the worst-case spend (1000 tokens, 0 tool calls).

You can pretty-print with `jq`:

```bash
orno plan hello.yaml | jq '.'
```

## Step 4 — Run without an API key (smoke test)

orno ships a deterministic dummy LLM transport for testing. Set the env var and run:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy orno run hello.yaml
```

You'll see NDJSON event envelopes on stdout. The shape of a run is always:

1. `run_started` — opens the run.
2. `node_started` — opens the agent node.
3. `agent_iteration_started` / `llm_request_started` / `llm_request_succeeded` / `agent_iteration_finished` — one cycle of the loop.
4. `node_finished` — closes the node with `ok: true`.
5. `run_finished` — closes the run.

Every envelope carries `schema_version`, `seq`, and `timestamp`. The `seq` field is monotonic per run; `timestamp` is RFC 3339 UTC.

## Step 5 — Run against a real LLM

Get an OpenRouter API key (https://openrouter.ai), then:

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
orno run hello.yaml --secrets-file .env.secrets
```

The `--secrets-file` flag tells orno to load credentials from a `KEY=VALUE` file. Provider-specific keys like `OPENROUTER_API_KEY` are auto-discovered when an agent's `provider:` matches.

The output is the same shape as the dummy run, but the `content_excerpt` field on `llm_request_succeeded` will contain the model's actual response.

## Step 6 — Read the model's reply

The agent's final assistant message is exposed as `nodes.greet.output`. To extract it from the event stream:

```bash
orno run hello.yaml --secrets-file .env.secrets \
  | jq -r 'select(.event.type == "node_finished") | .event.output'
```

You can also use this output in downstream nodes via `{{ nodes.greet.output }}` — that's the shape multi-node pipelines take.

## Step 7 — Stream discipline

`orno run` separates two streams:

- **stdout** — NDJSON events you just consumed with `jq`.
- **stderr** — internal `tracing` JSON for log pipelines.

This means a pipe like `orno run pipeline.yaml | downstream-tool` only sees the user-facing event stream, while `2>tracing.log` captures the internal observability separately. Both streams use the same RFC 3339 UTC timestamp format so they join on wall clock.

## What you've learned

- Every pipeline declares `version`, `vars`, `agents`, and `nodes`.
- Every agent declares a `policy` with five strictness dimensions.
- `orno plan` previews the worst case without spending tokens.
- `orno validate` checks well-formedness without spending tokens.
- `orno run` produces NDJSON on stdout and tracing JSON on stderr.

## Next steps

- [Record and replay a run](record-replay.md) — record a bundle and replay it offline.
- [Multi-agent PR review](multi-agent-pr-review.md) — using `subagent.<name>` for delegation.
- [How-to guides](../how-to/) — task-shaped recipes (secrets, MCP, state, debugging).
- [Pipeline YAML grammar](../reference/pipeline-yaml.md) — every field, every default.
