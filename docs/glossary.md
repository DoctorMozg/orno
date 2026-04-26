# Glossary

Vocabulary you will see in the docs, in the codebase, and in event logs. Terms are grouped by what they describe.

## Core concepts

**Pipeline.** A YAML document declaring `vars`, `agents`, optional `mcp_servers`, and a list of `nodes`. The unit of input to `orno run`.

**Node.** A vertex in the DAG. Two kinds: `kind: agent` (runs the strict loop) and `kind: shell` (deterministic subprocess).

**DAG.** Directed acyclic graph of nodes. Edges come from `needs:` lists. Cycles fail at load with a typed error.

**Run.** One execution of a pipeline. Identified by a `run_id` of the form `run_<ULID>`, generated at run start.

**Event.** One entry on the user-facing event log. Each event is wrapped in an envelope (`schema_version`, `seq`, `timestamp`, `event`) and serialized as NDJSON on stdout.

**Bundle.** An NDJSON file capturing every external interaction in a run — LLM requests/responses and tool/MCP exchanges. Written by `orno run --record-bundle`, consumed by `orno replay`.

**Tape.** Subset of a bundle: an LLM tape captures only LLM calls; a tool tape captures only tool/MCP calls. Written via `--record-tape` / `--record-tool-tape`, replayed via `--replay-tape` / `--replay-tool-tape`.

## The five strictness dimensions

**Bounded iteration.** `policy.max_iterations` caps the number of times the agent loop can turn before terminating with `IterationLimitExceeded`.

**Bounded tool surface.** `allowed_tools` is the closed set of tool names the model may call. Anything outside it → `UnknownToolCalled` → terminate.

**Bounded effects.** `policy.allow_mutations`, `policy.allow_network`, `policy.allow_context_writes`, and `policy.allowed_domains` / `blocked_domains` gate what tools can actually do. Violations produce typed events; the model sees the denial as a tool result.

**Bounded resources.** `policy.max_total_tokens`, `policy.max_tool_calls`, `policy.max_subagent_depth`. Wall-clock is the node-level `timeout:` attribute.

**Bounded non-determinism.** Every external interaction in a run can be recorded into a bundle and replayed exactly. Tape misses during replay are hard errors.

## Agent loop

**Agent.** A named entry under the top-level `agents:` map: model, provider, system prompt, allowed tools, policy. Referenced by nodes (`nodes[*].agent`) and by other agents' subagent tools (`subagent.<name>` in `allowed_tools`).

**Iteration / loop turn.** One round of: send context to the model → receive response → execute zero or more tool calls → append results to context.

**Subagent.** A child agent invoked from a parent via the `subagent.<name>` tool. Runs a fresh loop with its own policy. Cannot relax its parent's `allow_mutations` / `allow_network`.

**Subagent depth.** Distance from the top-level node to the current agent. Top-level agent is depth 0; its children are depth 1, etc. Capped per agent by `policy.max_subagent_depth`.

**`on_parse_error`.** What the loop does when the model returns malformed JSON for a tool call's arguments. `fail` terminates; `retry_once` feeds the parse error back as a tool result and loops once more.

## Tools

**Builtin tool.** A handler shipped with orno. Today: `Bash`, `Read`, `Edit`, `Write`, `WebFetch`, `SetState`.

**MCP tool.** A tool advertised by a Model Context Protocol server declared under `mcp_servers:`. Referenced as `mcp.<server>.<tool>` in `allowed_tools` (or the wildcard `mcp.<server>.*`).

**Subagent tool.** The synthetic tool exposed for delegating to a child agent. Reference form: `subagent.<agent-name>`. Argument shape: `{ prompt: string }`.

**Effect class.** Declared category of side-effects a tool can produce. Builtins are classified statically (`Read` is `local_read`, `Bash` is `shell` (mutations + network), etc.). MCP tools are classified opaquely as both mutating and networked because orno cannot introspect a remote server's per-tool semantics at registration time.

**`SetState`.** Builtin tool that writes single-level keys under `nodes.<self>.state.*`. Gated by `policy.allow_context_writes`. Downstream nodes read via `nodes.<id>.state.<key>`.

## Templates

**Template.** A MiniJinja string, auto-escape disabled. Renders against the per-node context.

**Template namespaces.** Three: `vars.*` (top-level `vars:` map), `env.*` (opt-in pipeline inputs), `secrets.*` (redacted credentials).

**`vars.<name>`.** A value from the top-level `vars:` map. Constant across the run.

**`env.<NAME>`.** A pipeline input, sourced (highest precedence first) from `-e KEY=VAL`, `--env-file`, or `pass_env:`. Visible in events. Undeclared names → template-render error.

**`secrets.<NAME>`.** A credential, sourced (highest precedence first) from `--secrets-file` or process env. Auto-pulled for provider keys (e.g. `OPENROUTER_API_KEY` when an agent's provider is `openrouter`). Always redacted to `***` in events and tapes.

**`nodes.<id>.output`.** Final assistant message from a completed upstream `kind: agent` node.

**`nodes.<id>.stdout` / `.stderr` / `.exit_code`.** Per-channel results from a completed upstream `kind: shell` node.

**`nodes.<id>.state.<key>`.** Scoped state key written by an upstream agent's `SetState` call.

**`nodes.<id>.status`.** Terminal `NodeStatus` for any completed upstream node: `completed | failed | timed_out | skipped`.

## MCP

**MCP.** Model Context Protocol — the protocol for connecting agents to tool servers. orno acts as the MCP client.

**Stdio transport.** `transport: stdio` — orno spawns the server as a subprocess and speaks MCP over its stdin/stdout. Configured with `command:` and `env:`.

**Streamable-HTTP transport.** `transport: http` — orno talks MCP over a single long-lived HTTP connection. Configured with `url:`, optional `auth:` (`kind: bearer | basic | none`), and optional `headers:`.

**Tool advertisement.** At server start, orno calls `tools/list` and resolves any `mcp.<server>.*` wildcards in agent `allowed_tools` to the concrete set the server exposes.

## Events and runtime

**Event envelope.** `{ schema_version, seq, timestamp, event }`. The wire-format wrapper around every event. `seq` is a monotonically increasing integer; `timestamp` is RFC 3339 UTC.

**`run_started` / `run_finished`.** First and last events on the log. `run_finished` carries `failed_nodes` and `skipped_nodes` aggregates so a tail-line read summarizes the run.

**`node_started` / `node_finished` / `node_skipped` / `node_timed_out`.** Per-node lifecycle events. `node_finished` carries `failure: Option<NodeFailure>` populated exactly when `ok: false`.

**Skip cascade.** When a node fails, every transitively-dependent node emits `node_skipped` with `reason: dependency_failed { upstream }` naming the originator (not the direct parent). Disjoint branches keep running.

**Redactor.** A per-run component that replaces secret values with `***` before emission. Redaction is name-based: anything classified as a secret is redacted, regardless of which source provided the value.

**Replay tape miss.** Replay receives a request whose recorded form is not in the bundle. Treated as a hard error, not as a fallback to the live API.

## CLI

**`orno run`.** Execute a pipeline. NDJSON events to stdout, tracing to stderr.

**`orno validate`.** Load and validate the full policy surface (tool names, agent and MCP references, budget fields).

**`orno plan`.** Static preview of a pipeline. One `plan_node` per node + a single `plan_summary`. No LLM, no network.

**`orno replay`.** Replay a bundle written by `orno run --record-bundle`.

**`orno schema`.** Print the pipeline JSON Schema to stdout. Used to regenerate `schemas/pipeline.schema.json`.

**`orno completions`.** Emit shell completions for bash, zsh, fish, elvish, or powershell.
