# What is orno

orno is a CI-native runner for strict agentic loops. You declare a pipeline as YAML, hand it to `orno run`, and get back an NDJSON event log on stdout that records every decision, every tool call, every LLM exchange. Every run can be replayed bit-for-bit from a recorded bundle without spending tokens.

The point of orno is not "an agent framework." The point is the **contract**.

## The contract

Every `agent` node enforces five guarantees at runtime. A breach terminates the node with a typed event on the log.

1. **Bounded iteration.** `policy.max_iterations` caps how many times the agent loop can turn. Hit the cap → `IterationLimitExceeded` → terminate.
2. **Bounded tool surface.** `allowed_tools` is the closed set of names the model may call. Anything else → `UnknownToolCalled` → terminate.
3. **Bounded effects.** `policy.allow_mutations`, `policy.allow_network`, and `policy.allowed_domains` / `blocked_domains` gate what tools can actually do. Violation → `MutatingCallBlocked` / `NetworkBlocked` / `DomainBlocked`, and the model sees the denial as a tool result.
4. **Bounded resources.** `policy.max_total_tokens` and `policy.max_tool_calls` cap LLM and tool consumption. Subagent depth is capped by `policy.max_subagent_depth`. Wall-clock deadlines are a node-level `timeout:` attribute and apply to agent and shell nodes alike.
5. **Bounded non-determinism.** Every LLM request, every tool call, and every MCP exchange can be recorded into a bundle (`orno run --record-bundle`), and the bundle can be replayed (`orno replay`) to reproduce outputs, exit code, and event log byte-for-byte.

These five aren't a default profile you can disable — they are the runtime contract every agent node honors. There is no "raw mode."

## The pipeline shape

A pipeline declares:

- `vars` — template variables.
- `pass_env` and `secrets` — opt-in environment inputs and credentials.
- `agents` — named agent configurations (model, system prompt, allowed tools, policy).
- `mcp_servers` — Model Context Protocol servers spawned at run start, shut down at run end. Stdio and streamable-HTTP transports are both supported.
- `nodes` — the DAG. Two node kinds:
  - `kind: agent` — runs the strict loop. Final assistant message is readable from downstream nodes as `{{ nodes.<id>.output }}`.
  - `kind: shell` — deterministic subprocess. Output is split into `{{ nodes.<id>.stdout }}`, `.stderr`, `.exit_code`. Not subject to agent policy.

Templates use MiniJinja with three namespaces: `vars.*`, `env.*`, and `secrets.*`. Secrets render only when the template explicitly references them, and they are redacted from every event, every log line, and every replay tape.

## Multi-agent without peer-to-peer

orno's multi-agent model is recursive single-agent loops, not peer-to-peer messaging. A parent agent calls a child via the `subagent.<name>` tool — the child runs its own bounded loop with its own policy, and returns its final assistant message to the parent like any other tool result. There are no chat channels between siblings.

A child cannot relax its parent's effect policy: a read-only parent cannot delegate to a mutating child. This is enforced at pipeline load by `orno validate`.

## Hero surface: `plan` and `replay`

Two surfaces exist specifically so a pipeline can be reviewed and re-run without spending tokens.

**`orno plan`** is `terraform plan` for an agent pipeline. Static analysis only — no LLM calls, no network, no tool execution. Stdout is one `plan_node` line per node followed by a single `plan_summary`, all in NDJSON. Reviewers audit the worst-case ceiling — declared tools, declared effects, max tokens, max tool calls, MCP dependencies — before any spend is authorized.

**`orno replay`** takes a bundle written by `orno run --record-bundle` and re-executes the pipeline against the recorded LLM and tool tapes. No live LLM, no network, no MCP server spawning. A tape miss is a hard error, not a fallback to the live API. This is what makes a run portable across machines, machine learning models, and time.

## Stream discipline

`orno run` separates two streams:

- **stdout** — the user-facing event log: NDJSON envelopes that downstream tools consume.
- **stderr** — internal observability: tracing JSON for log pipelines.

Both timestamps are RFC 3339 UTC, so the two streams join trivially on wall clock. Exit code is `0` on success and non-zero on pipeline load failure or any node failure.

## What orno is not

- **Not** a chat UI. orno does not have a REPL, a streaming TUI, or a chat surface — it produces NDJSON on stdout and exits.
- **Not** a vendor SDK. Provider transport is pluggable via a single `LlmTransport` trait. Today the default is OpenRouter.
- **Not** a workflow engine. orno schedules a DAG, but it is not a substitute for Temporal, Inngest, or Airflow at the orchestration layer. Use it inside CI for the AI-bounded leaf of a larger workflow.
- **Not** an "autonomous agent." Bounded iteration, bounded tools, bounded effects, bounded resources — autonomy is what you authorize via the policy block, not the default.

## Where to go next

- [Install](install.md) — get a working `orno` binary.
- [Pipeline YAML grammar](reference/pipeline-yaml.md) — the full surface, every field.
- [`hello`](../examples/hello/) — the smallest runnable example, and a good shape to copy from.
- [Glossary](glossary.md) and [FAQ](faq.md) — vocabulary and common questions.
