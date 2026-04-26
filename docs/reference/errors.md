# Error reference

orno's public error surface is a fixed set of `thiserror` enums in `orno-core::error`. Every enum is `#[non_exhaustive]` — consumers must accept that new variants may appear in future releases without that being a breaking change.

This page enumerates each enum's variants and the conditions that produce them. Failures that surface on the user-facing event log are typed in [`events.md`](events.md) — `NodeFailure` and `LlmFailure` mirror these enums for wire emission.

## `CoreError`

The crate-level umbrella enum. Wraps each subsystem error so callers can match on the source category.

| Variant      | Carries        | Source                                       |
| ------------ | -------------- | -------------------------------------------- |
| `Pipeline`   | `PipelineError`| Pipeline load, parse, or validation failure. |
| `Node`       | `NodeError`    | Node-execution dispatch failure.             |
| `Agent`      | `AgentError`   | Agent-loop strictness breach.                |
| `Llm`        | `LlmError`     | LLM transport call failure.                  |
| `Tool`       | `ToolError`    | Tool-handler invocation failure.             |
| `Mcp`        | `McpError`     | MCP client failure.                          |

## `PipelineError`

Failures discovered while loading or validating a pipeline YAML.

| Variant         | Fields                                                | Cause                                                                                                                                  |
| --------------- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `Io`            | `#[from] std::io::Error`                              | Filesystem error reading the pipeline file.                                                                                            |
| `Parse`         | `#[from] serde_yaml_ng::Error`                        | YAML did not parse against the pipeline schema.                                                                                        |
| `Validation`    | `message: String`                                     | Pipeline parsed but failed semantic checks (unknown agent reference, child agent more permissive than parent, malformed tool name…).   |
| `Template`      | `name: String`, `#[source] minijinja::Error`          | A templated string (`initial_prompt`, MCP `command`, etc.) failed to render against the available context.                             |
| `InvalidGraph`  | `message: String`                                     | The DAG declared by `nodes[*].needs` is malformed — a cycle, a self-loop, or an edge to an undefined node.                             |
| `UnknownAgent`  | `name: String`                                        | A `nodes[*].agent` value or a `subagent.<name>` allowed-tool entry referenced an agent not declared in `agents:`.                       |

`PipelineError` always surfaces at load or `orno validate` time — never mid-run.

## `NodeError`

Failures from the node executor's dispatch path. Surface as `NodeFailure::ExecutorError` on the event log.

| Variant          | Fields                                              | Cause                                                                                                                          |
| ---------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `UnknownKind`    | `kind: String`                                      | A pipeline node has a `kind:` value with no registered executor. Surfaces as `NodeFailure::NoExecutorRegistered` on the wire.   |
| `NotImplemented` | `kind: String`                                      | An executor accepted dispatch but its handler is not yet available in this build.                                              |
| `UnsupportedYet` | `feature: String`                                   | A node feature exists in the schema but is not supported by the installed engine.                                              |
| `Execution`      | `kind: String`, `#[source] anyhow::Error`           | The executor's body returned `Err` (process spawn failed, transport error, etc.).                                              |

## `AgentError`

Failures from the agent loop. Each variant maps to one of the [five strictness dimensions](../glossary.md#the-five-strictness-dimensions) or to a tool/LLM-source failure surfaced through the loop.

| Variant                  | Fields                                                 | Cause                                                                                                                                                        |
| ------------------------ | ------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `InvalidPolicy`          | `message: String`                                      | The `AgentPolicy` was rejected before the loop entered (e.g. `max_iterations: 0`).                                                                           |
| `UnsupportedYet`         | `feature: String`                                      | Loop entered but a referenced feature is not supported in this build.                                                                                        |
| `IterationLimitExceeded` | `max_iterations: u32`                                  | The loop reached `policy.max_iterations` without converging on a `stop` finish reason. Bounded-iteration breach.                                              |
| `UnknownToolCalled`      | `name: String`                                         | The model emitted a tool-call turn for a name not in `allowed_tools`. Bounded-tool-surface breach. Terminates immediately — no retry.                         |
| `BudgetExceeded`         | `kind: BudgetKind`, `used: u64`, `limit: u64`          | A running budget breached: `Tokens` (cumulative `max_total_tokens`) or `ToolCalls` (`max_tool_calls`). Bounded-resources breach.                              |
| `SubagentDepthExceeded`  | `attempted: u32`, `max_depth: u32`                     | A `subagent.<name>` call attempted while the parent agent's `max_subagent_depth` budget was already exhausted. Non-terminal: surfaces as a denial back to the model. |
| `ParseFailed`            | `message: String`                                      | The model returned malformed JSON for a tool-call argument and `policy.on_parse_error: fail` (or the retry was already consumed).                              |
| `Tool`                   | `#[from] ToolError`                                    | A tool dispatch returned `ToolError`. The loop's behavior depends on the variant — `InvalidArgs` is fed back as a denial; `Invocation` is terminal.            |
| `Llm`                    | `#[from] LlmError`                                     | The transport returned `LlmError`. Terminates the node.                                                                                                       |

## `LlmError`

Failures from the `LlmTransport` seam. Mirror to `LlmFailure` on the event wire (`Event::LlmRequestFailed`).

| Variant           | Fields                                                                      | Cause                                                                                                       |
| ----------------- | --------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `NotImplemented`  | `provider: String`                                                          | A request reached a provider that has no transport adapter built in.                                         |
| `Rejected`        | `String`                                                                    | The transport refused the request before it left the process (e.g. content filter on the adapter).           |
| `AuthFailed`      | `provider: String`                                                          | HTTP 401/403 or pre-flight `RequiresApiKey` / `NoAuthData`. Wire form: `LlmFailure::AuthFailed`.             |
| `RateLimited`     | `provider: String`                                                          | HTTP 429. Wire form: `LlmFailure::RateLimited`. v0.1 does not retry.                                         |
| `ModelNotFound`   | `provider: String`, `model: String`                                         | HTTP 404 on the chat endpoint — usually a model-name typo. Wire form: `LlmFailure::ModelNotFound`.           |
| `ApiError`        | `provider: String`, `status: u16`, `body: String`                           | Any other HTTP failure. Wire form: `LlmFailure::ApiError { status, body_excerpt }` (head-truncated).         |
| `Transport`       | `#[source] anyhow::Error`                                                   | Network/timeout/transport problem from the underlying client. Wire form: `LlmFailure::Transport`.            |
| `ConfigError`     | `String`                                                                    | Pre-flight misconfiguration caught before any network call. Wire form: `LlmFailure::ConfigError`.            |
| `ParseError`      | `String`                                                                    | Provider returned a payload the adapter could not parse. Wire form: `LlmFailure::ParseError`.                |
| `ReplayMiss`      | `key: String`                                                               | Replay tape miss — the caller is running against a tape from a different pipeline, or the tape is incomplete. Wire form: `LlmFailure::ReplayMiss`. |

## `ToolError`

Failures from a `ToolHandler::invoke` call.

| Variant         | Fields                                                              | Cause                                                                                                                                                    |
| --------------- | ------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Invocation`    | `name: String`, `#[source] Box<dyn Error>`                          | The tool's body returned `Err`. Process spawn failure, network failure, file-not-found, MCP transport error, etc.                                         |
| `InvalidArgs`   | `name: String`, `message: String`                                   | The model's argument JSON did not match the tool's schema. Fed back to the model as a tool-result string; loop continues.                                 |
| `StateTooLarge` | `name: String`, `bytes: usize`, `cap: usize`                        | A `SetState` write would push the node's serialized `state` past `EngineConfig.max_output_bytes`. The write rolls back; previous state survives intact.   |
| `NotImplemented`| `name: String`                                                      | The tool exists in the registry but its handler body is not yet built.                                                                                   |

## `McpError`

Failures from the MCP client.

| Variant                | Fields                                                                          | Cause                                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `SpawnFailed`          | `server: String`, `#[source] std::io::Error`                                    | The MCP server subprocess failed to start (stdio transport).                                                            |
| `HandshakeFailed`      | `server: String`, `message: String`                                             | The `initialize` + `tools/list` handshake failed (HTTP transport: bad URL or auth; stdio: process exited mid-handshake). |
| `CallFailed`           | `server: String`, `tool: String`, `#[source] Box<dyn Error>`                    | A `tools/call` request failed (transport-level — connection drop, timeout).                                             |
| `ToolError`            | `server: String`, `tool: String`, `message: String`                             | The server reported a tool-level error in the `tools/call` response (the response said `is_error: true`).               |
| `ServerCrashed`        | `server: String`, `reason: String`                                              | The server exited mid-run unexpectedly. Surfaces on `Event::McpServerCrashed`.                                          |
| `UnsupportedTransport` | `server: String`, `kind: String`                                                | The server's `transport:` value names a kind orno does not implement (e.g. `kind: basic` HTTP auth in v0.1).            |
| `ShutdownTimeout`      | `server: String`                                                                | The server did not exit cleanly within the shutdown deadline at run end.                                                |
| `UnknownTool`          | `server: String`, `tool: String`                                                | The agent's `allowed_tools` referenced `mcp.<server>.<tool>` for a tool the server's `tools/list` did not advertise.    |

## See also

- [Events](events.md) — `NodeFailure` and `LlmFailure` mirror this surface for wire-format emission.
- [Exit codes](exit-codes.md) — process-level discriminators.
- [Five strictness dimensions](../glossary.md#the-five-strictness-dimensions) — the contract violations behind most `AgentError` variants.
