# What is orno

orno is a CI-native runner for strict agentic loops. You declare a pipeline as YAML and hand it to `orno run`. On stdout you get an NDJSON event log that records every decision, every tool call, every LLM exchange. A recorded bundle lets you replay any run bit-for-bit without spending tokens.

orno isn't trying to be an agent framework. What it offers is a **contract**.

## The contract

Every `agent` node enforces five guarantees at runtime. A breach ends the node and puts a typed event on the log.

1. **Bounded iteration.** `policy.max_iterations` caps how many times the agent loop can turn. Once the cap is hit, the node emits `IterationLimitExceeded` and terminates.
2. **Bounded tool surface.** `allowed_tools` is the closed set of names the model may call. Any other name produces `UnknownToolCalled` and the node terminates.
3. **Bounded effects.** `policy.allow_mutations`, `policy.allow_network`, and `policy.allowed_domains` / `blocked_domains` gate what tools can actually do. A violation emits a `tool_denied` event, and the model sees the denial as a tool result.
4. **Bounded resources.** `policy.max_total_tokens` and `policy.max_tool_calls` cap LLM and tool consumption. `policy.max_subagent_depth` caps subagent depth. Wall-clock deadlines are a node-level `timeout:` attribute, and they apply to agent and shell nodes alike.
5. **Bounded non-determinism.** Every LLM request, tool call, and MCP exchange can be recorded into a bundle (`orno run --record-bundle`). Replaying that bundle (`orno replay`) reproduces outputs, exit code, and event log byte-for-byte.

You can't switch these off. They aren't a default profile but the runtime contract every agent node honors, and there is no "raw mode."

## The pipeline shape

A pipeline declares:

- `vars`: template variables.
- `pass_env` and `secrets`: opt-in environment inputs and credentials.
- `agents`: named agent configurations (model, system prompt, allowed tools, policy).
- `mcp_servers`: Model Context Protocol servers, spawned at run start and shut down at run end. Stdio and streamable-HTTP transports are both supported.
- `nodes`: the DAG. There are two node kinds:
  - `kind: agent` runs the strict loop. Downstream nodes read the final assistant message as `{{ nodes.<id>.output }}`.
  - `kind: shell` runs a deterministic subprocess. Its output is split into `{{ nodes.<id>.stdout }}`, `.stderr`, and `.exit_code`. Agent policy doesn't apply to it.

Templates use MiniJinja with three namespaces: `vars.*`, `env.*`, and `secrets.*`. A secret renders only when the template references it explicitly, and it's redacted from every event, every log line, and every replay tape.

## Multi-agent without peer-to-peer

orno's multi-agent model is recursive single-agent loops, not peer-to-peer messaging. A parent agent calls a child through the `subagent.<name>` tool. The child runs its own bounded loop under its own policy and returns its final assistant message to the parent, like any other tool result. Siblings have no chat channels between them.

A child can't relax its parent's effect policy, so a read-only parent can't delegate to a mutating child. `orno validate` enforces this when the pipeline loads.

## Hero surface: `plan` and `replay`

Two commands exist so that a pipeline can be reviewed and re-run without spending tokens.

**`orno plan`** is `terraform plan` for an agent pipeline. It does static analysis only: no LLM calls, no network, no tool execution. Stdout is one `plan_node` line per node, then a single `plan_summary`, all in NDJSON. Reviewers audit the worst-case ceiling (declared tools, declared effects, max tokens, max tool calls, MCP dependencies) before anyone authorizes spend.

**`orno replay`** takes a bundle written by `orno run --record-bundle` and re-executes the pipeline against the recorded LLM and tool tapes. It uses no live LLM, no network, and spawns no MCP servers. A tape miss is a hard error, never a fallback to the live API. That's what lets a run move across machines, models, and time.

## Stream discipline

`orno run` keeps two streams apart:

- **stdout** is the user-facing event log: NDJSON envelopes that downstream tools consume.
- **stderr** is internal observability: tracing JSON for log pipelines.

Both streams stamp timestamps as RFC 3339 UTC, so they join on wall clock without any conversion. The exit code is `0` on success and non-zero on pipeline load failure or any node failure.

## What orno is not

- **Not** a chat UI. orno has no REPL, no streaming TUI, and no chat surface. It writes NDJSON to stdout and exits.
- **Not** a vendor SDK. Provider transport is pluggable behind a single `LlmTransport` trait. Today the default is OpenRouter.
- **Not** a workflow engine. orno schedules a DAG, but it doesn't replace Temporal, Inngest, or Airflow at the orchestration layer. Use it inside CI for the AI-bounded leaf of a larger workflow.
- **Not** an "autonomous agent." Iteration, tools, effects, and resources are all bounded. Autonomy is what you authorize in the policy block, not the default.

## Where to go next

- [Install](install.md): get a working `orno` binary.
- [Pipeline YAML grammar](reference/pipeline-yaml.md): the full surface, every field.
- [`hello`](../examples/hello/): the smallest runnable example, and a good shape to copy from.
- [Glossary](glossary.md) and [FAQ](faq.md): vocabulary and common questions.
