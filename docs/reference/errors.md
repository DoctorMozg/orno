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

| Variant        | Fields                                                  | Cause                                                                                                                                         |
| -------------- | ------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `Io`           | `path: String`, `#[source] std::io::Error`              | Filesystem error reading the pipeline file.                                                                                                   |
| `Parse`        | `#[source] serde_yaml_ng::Error` (tuple variant)        | YAML did not parse against the pipeline schema, including unknown keys (`additionalProperties: false`).                                       |
| `Validation`   | `String` (tuple variant)                                | Pipeline parsed but failed semantic checks. Several problems are reported at once as `N errors:` with one bullet each. See [validation](pipeline-yaml.md#validation). |
| `Template`     | `name: String`, `#[source] minijinja::Error`            | A templated string (`initial_prompt`, shell `args`, an MCP `url`, etc.) failed to compile or render against the available context.            |
| `InvalidGraph` | `reason: String`                                        | The DAG declared by `nodes[*].needs` is malformed: a cycle, a self-loop, or an edge to an undefined node.                                      |
| `UnknownAgent` | `name: String`                                          | A node referenced an agent missing from `agents:`. `validate()` rejects this first; the render path reports it for pipelines built in code.    |

`orno run`, `orno plan`, `orno replay` and `orno validate` all run the same validation, so a `PipelineError` surfaces before the first node starts.

## `NodeError`

Failures from the node executor's dispatch path. Surface as `NodeFailure::ExecutorError` on the event log.

| Variant          | Fields                                                              | Cause                                                                                                         |
| ---------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `NotImplemented` | `id: String`                                                        | A node reached a skeleton stub executor.                                                                      |
| `Execution`      | `id: String`, `#[source] Box<dyn Error + Send + Sync>`              | The executor's body returned `Err` (process spawn failed, agent loop error, transport error, etc.).           |
| `ReplayRefused`  | `id: String`                                                        | `orno replay` met a `kind: shell` node. Shell side effects are not recorded in a bundle, so they cannot be replayed. |

## `AgentError`

Failures from the agent loop. Each variant maps to one of the [five strictness dimensions](../glossary.md#the-five-strictness-dimensions) or to a tool/LLM-source failure surfaced through the loop.

| Variant                  | Fields                                              | Cause                                                                                                                                                                  |
| ------------------------ | --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `InvalidPolicy`          | `String` (tuple variant)                            | The policy was rejected before the first LLM call (e.g. `max_iterations: 0`, or Read/Write/Edit listed with empty `roots`).                                            |
| `IterationLimitExceeded` | `max: u32`                                          | The loop reached `policy.max_iterations` without a `stop` finish reason. Bounded-iteration breach.                                                                     |
| `UnknownToolCalled`      | `name: String`                                      | The model called a tool not in `allowed_tools`, or an `allowed_tools` entry has no registered handler. Bounded-tool-surface breach. Terminates immediately, no retry.   |
| `BudgetExceeded`         | `kind: BudgetKind`                                  | `Tokens` (`max_total_tokens`) or `ToolCalls` (`max_tool_calls`) breached. Both budgets cover the whole subagent tree. Bounded-resources breach.                        |
| `ParseFailed`            | `tool: String`, `error: String`                     | A tool's arguments failed to parse and `on_parse_error: fail` was set, or `retry_once` was already spent for that call.                                                |
| `Tool`                   | `name: String`, `#[source] ToolError`               | A tool handler returned a terminal `ToolError`. Recoverable errors never reach this variant; they are fed back to the model.                                           |
| `Llm`                    | `#[from] LlmError` (tuple variant)                  | The transport returned `LlmError`. Terminates the node.                                                                                                                |

## `LlmError`

Failures from the `LlmTransport` seam. Mirrored to `LlmFailure` on the event wire (`Event::LlmRequestFailed`).

| Variant         | Fields                                                          | Cause                                                                                                                                     |
| --------------- | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| `Rejected`      | `String` (tuple variant)                                        | The transport refused the request before it left the process. Wire form: `LlmFailure::Other`.                                             |
| `AuthFailed`    | `provider: String`                                              | HTTP 401/403. Wire form: `LlmFailure::AuthFailed`. A missing API key is a `ConfigError` instead.                                          |
| `RateLimited`   | `provider: String`                                              | HTTP 429. Not retried. Wire form: `LlmFailure::RateLimited`.                                                                              |
| `ModelNotFound` | `provider: String`, `model: String`                             | HTTP 404 whose body mentions the model. Any other 404 is an `ApiError`. Wire form: `LlmFailure::ModelNotFound`.                           |
| `ApiError`      | `provider: String`, `status: u16`, `body: String`               | Any other HTTP failure. The in-memory body is capped at 512 bytes. Wire form: `LlmFailure::ApiError { status, body_excerpt }`.            |
| `Transport`     | `#[source] Box<dyn Error + Send + Sync>` (tuple variant)        | Network, timeout or other transport problem. Each attempt is cut off after 5 minutes and a timed-out attempt is not retried. Wire form: `LlmFailure::Transport`. |
| `ConfigError`   | `String` (tuple variant)                                        | Misconfiguration caught before any network call: unknown provider, or a missing API key (the message names the env var to set). Wire form: `LlmFailure::ConfigError`. |
| `ParseError`    | `String` (tuple variant)                                        | The provider returned a payload the adapter could not parse. Wire form: `LlmFailure::ParseError`.                                         |
| `ReplayMiss`    | `key: String`                                                   | Replay tape miss. The run is replaying a tape from a different pipeline, or the tape is incomplete. Never falls through to a live call. Wire form: `LlmFailure::ReplayMiss`. |

## `ToolError`

Failures from a `ToolHandler::invoke` call. `ToolError::is_recoverable()` decides what the agent loop does next: recoverable errors are fed back to the model as an `error: <message>` tool result and the loop continues; every other variant terminates the node as `AgentError::Tool`.

| Variant         | Fields                                                              | Recoverable | Cause                                                                                                                                              |
| --------------- | ------------------------------------------------------------------- | ----------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Recoverable`   | `name: String`, `kind: RecoverableKind`, `message: String`          | yes         | The tool ran and failed in a way the model can act on. `kind` is listed below.                                                                     |
| `NotImplemented`| `name: String`, `feature: String`                                   | yes         | The tool declared a feature its handler does not implement yet. Reported to the model as `error: tool ... not yet implemented: ...`.               |
| `Invocation`    | `name: String`, `#[source] Box<dyn Error + Send + Sync>`            | no          | The underlying I/O or API call failed in a way the model cannot fix (spawn failure, WebFetch URL, redirect or address denial, Bash `cwd` outside the root, MCP transport error). |
| `InvalidArgs`   | `name: String`, `message: String`                                   | no          | The argument JSON did not match the tool's schema. With `on_parse_error: retry_once` the first one per call is fed back to the model; otherwise it becomes `AgentError::ParseFailed`. |
| `StateTooLarge` | `name: String`, `bytes: usize`, `cap: usize`                        | no          | A `SetState` write would push `nodes.<self>.state` past `EngineConfig.max_output_bytes`. The write is rejected and the previous state is kept.     |
| `Denied`        | `reason: String`                                                    | no          | A runtime safety check inside the handler refused the call: a path escaping `roots`, empty `roots` for Read/Write/Edit, Bash `cwd` without `roots`, a rendered subagent root outside the parent's roots. Distinct from the effect-class gate, which feeds a `denied: ...` result back to the model and never terminates. |

### `RecoverableKind`

Serialized as snake_case. Matched on by the tapes, so a replayed recoverable failure keeps its kind.

| Kind            | Raised by                                                                                       |
| --------------- | ----------------------------------------------------------------------------------------------- |
| `not_found`     | A path the model named does not exist (`Read`, `Edit`).                                         |
| `invalid_data`  | A file exists but its content cannot be used as asked.                                          |
| `timeout`       | `Bash` hit its `timeout_secs` (the message includes partial output) or `WebFetch` timed out.    |
| `no_match`      | `Edit`'s `old_string` matched zero or several places.                                           |
| `tool_reported` | An MCP tool returned `isError: true`.                                                           |

## `McpError`

Failures from the MCP client.

| Variant                | Fields                                                              | Cause                                                                                                                  |
| ---------------------- | ------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `SpawnFailed`          | `server: String`, `#[source] Box<dyn Error + Send + Sync>`          | The MCP server subprocess failed to start (stdio transport).                                                           |
| `HandshakeFailed`      | `server: String`, `#[source] Box<dyn Error + Send + Sync>`          | The `initialize` + `tools/list` handshake failed (HTTP: bad URL, headers or auth; stdio: process exited mid-handshake). |
| `CallFailed`           | `server: String`, `tool: String`, `#[source] Box<dyn Error + Send + Sync>` | A `tools/call` request failed at the transport level: connection drop, or no reply within `call_timeout_secs`. |
| `ToolError`            | `server: String`, `tool: String`, `message: String`                 | The server reported `isError: true`. The agent loop surfaces it to the model as `RecoverableKind::ToolReported`.       |
| `UnsupportedTransport` | `transport: String`                                                 | The server config names a transport or auth mode this build cannot connect (today, `auth: basic` over HTTP).           |

## See also

- [Events](events.md) — `NodeFailure` and `LlmFailure` mirror this surface for wire-format emission.
- [Exit codes](exit-codes.md) — process-level discriminators.
- [Five strictness dimensions](../glossary.md#the-five-strictness-dimensions) — the contract violations behind most `AgentError` variants.
