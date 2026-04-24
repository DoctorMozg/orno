# orno

CI-native runner for strict agentic loops.

orno runs LLM agents under a runtime-enforced contract: bounded iteration, bounded tool surface, bounded effects, bounded resources, bounded non-determinism. Every decision is emitted on a versioned event log, and every run can be replayed byte-for-byte without spending tokens.

## Hero surface

### `orno plan` — preview before spend

Static analysis of a pipeline. No LLM calls, no tool execution, no network. Emits one `plan_node` line per node followed by a single `plan_summary` line as NDJSON on stdout. Exit code is `0` iff the pipeline loads, validates, and is spendable.

```
$ orno plan examples/hello.yaml
{"type":"plan_node","node_id":"greet","kind":"agent","depends_on":[],"timeout_secs":null,"agent_name":"greeter","model":"openai/gpt-5","provider":"openrouter","tools":[],"max_iterations":1,"max_total_tokens":1000,"max_tool_calls":0,"allow_mutations":false,"allow_network":false,"allowed_domains":[],"blocked_domains":[]}
{"type":"plan_summary","total_nodes":1,"agent_nodes":1,"shell_nodes":0,"agents_used":["greeter"],"max_iterations_total":1,"max_tokens_total":1000,"max_tool_calls_total":0,"mcp_servers":[],"dag_is_valid":true}
```

Treat it as `terraform plan` for an agent pipeline: a reviewer audits the worst-case ceiling — tokens, tool calls, declared effects, MCP dependencies — before any spend is authorized.

### `orno replay` — replay without tokens

Given a bundle file recorded by a prior run, orno re-executes the pipeline from the recorded LLM and tool tapes. No live LLM calls, no network, no MCP server spawning — every external interaction is served from the bundle. Outputs, exit code, and event log are reproduced bit-for-bit.

Record a bundle:

```
orno run examples/hello.yaml --record-bundle run.ndjson
```

Replay it:

```
orno replay run.ndjson
```

A tape miss during replay is a hard error, not a fallback to the live API.

## The five strictness dimensions

Every `agent` node enforces all five at runtime. A breach terminates the node with the corresponding event on the log.

| Dimension                | What it bounds                                  | Config key(s)                                                                                        |
| ------------------------ | ----------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Bounded iteration        | Agent-loop turns                                | `policy.max_iterations`                                                                              |
| Bounded tool surface     | Which tools the model may call                  | `allowed_tools` (builtin names, `mcp.<server>.<tool>`, `subagent.<name>`)                            |
| Bounded effects          | Mutating ops, network calls, domain reach       | `policy.allow_mutations`, `policy.allow_network`, `policy.allowed_domains`, `policy.blocked_domains` |
| Bounded resources        | Total tokens, total tool calls, subagent depth  | `policy.max_total_tokens`, `policy.max_tool_calls`, `policy.max_subagent_depth`                      |
| Bounded non-determinism  | Every LLM call recorded; replay is exact        | `orno run --record-bundle` / `orno replay`                                                           |

Wall-clock deadlines are a node-level attribute (`timeout:`) and apply uniformly to agent and shell nodes (ADR 0017).

## Quickstart

### Installation

```
cargo install orno-cli
```

(Stub — crates.io publishing happens at the v0.1.0 tag. Until then, run from source.)

### Run an example

`examples/hello.yaml` calls a real LLM via OpenRouter. To run it without an API key, set the dummy transport — it returns a deterministic canned response:

```
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- plan examples/hello.yaml
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- run examples/hello.yaml
```

For a real run:

```
export OPENROUTER_API_KEY=sk-or-v1-...
cargo run -p orno-cli -- run examples/hello.yaml
```

`examples/hello.yaml` in full:

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

## Pipeline YAML shape

A pipeline declares `vars`, named `agents`, optional `mcp_servers`, and a list of `nodes` forming a DAG. Two node kinds ship in v0.1.0:

- `kind: agent` — runs the strict loop against a named agent. Final assistant message is readable from downstream nodes as `nodes.<id>.output`.
- `kind: shell` — deterministic subprocess. Output is split into `nodes.<id>.stdout`, `.stderr`, and `.exit_code`. Not subject to agent policy.

Templates use MiniJinja with three namespaces: `vars.*`, `env.*` (opt-in pipeline inputs), and `secrets.*` (redacted credentials). See `docs/yaml-spec.md` for the full grammar, and `examples/pr-review.yaml`, `examples/release-notes.yaml`, `examples/flaky-test-triage.yaml` for functionality-heavy samples.

## Commands

| Command                          | Description                                                                                       | Key flags                                                                                                                                                                |
| -------------------------------- | ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `orno run <pipeline.yaml>`       | Execute a pipeline. NDJSON events to stdout, tracing JSON to stderr.                              | `-e KEY=VAL`, `--env-file`, `--secrets-file`, `-v` / `--verbose`, `--stderr-tail-bytes`, `--record-bundle`, `--record-tape`, `--replay-tape`, `--record-tool-tape`, `--replay-tool-tape` |
| `orno validate <pipeline.yaml>`  | Load and validate the full policy surface (tool names, agent and MCP references, budget fields). |                                                                                                                                                                          |
| `orno plan <pipeline.yaml>`      | Static preview. Emits `plan_node` and `plan_summary` records as NDJSON. No LLM or network.       |                                                                                                                                                                          |
| `orno replay <bundle.ndjson>`    | Replay a bundle written by `orno run --record-bundle`. No live LLM calls, no network.            |                                                                                                                                                                          |
| `orno schema`                    | Print the pipeline JSON Schema to stdout. Used to regenerate `schemas/pipeline.schema.json`.     |                                                                                                                                                                          |
| `orno completions <shell>`       | Emit shell completions (bash, zsh, fish, elvish, powershell).                                    |                                                                                                                                                                          |

`orno run` separates streams: NDJSON event envelopes go to stdout (downstream tools), tracing JSON goes to stderr (log pipelines). Both timestamps are RFC 3339 UTC, so the two streams join on wall clock. Exit `0` on success; non-zero on pipeline load failure or any node failure.

## Deferred

- Parallel DAG execution — linear node execution in v0.1.0; parallel scheduling lands in v0.1.1.
- `WebSearch` builtin — needs a `SearchProvider` trait with a Tavily/Brave impl. Use MCP for now.
- SQLite `EventSink` — the seam exists; the impl ships when durability is requested.
- Inline agent config at the node level — every agent lives under `agents.*` for readability.
- Streaming LLM responses with mid-flight budget enforcement.

See `docs/roadmap.md` for the full deferred list and the v0.2.0+ plan.

## License

AGPL-3.0-only. See `Cargo.toml` for the canonical SPDX identifier.
