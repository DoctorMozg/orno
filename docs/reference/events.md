# Event reference

`orno run` emits a stream of NDJSON events on **stdout** — one envelope per line. The stream is the public, versioned wire format. Internal `tracing` JSON goes to **stderr** under a separate, unversioned schema; do not pipe the two into the same consumer.

This page documents the envelope, every `Event` variant, and the typed failure payloads.

## Envelope

Every event is wrapped in:

```json
{
  "schema_version": 1,
  "seq": 0,
  "timestamp": "2026-04-21T15:30:00.123456789Z",
  "event": { "type": "...", ... }
}
```

| Field            | Type        | Description                                                                                          |
| ---------------- | ----------- | ---------------------------------------------------------------------------------------------------- |
| `schema_version` | integer     | Currently `1`. Bumped on backwards-incompatible event-schema changes.                                |
| `seq`            | integer     | Monotonic emission order, starting at `0` for `run_started`. Use this as the strict-ordering key.    |
| `timestamp`      | string      | RFC 3339 UTC instant (`time::serde::rfc3339`). Joinable with stderr `tracing` lines on wall clock.   |
| `event`          | object      | Internally-tagged variant — `event.type` discriminates.                                              |

`Event` is `#[non_exhaustive]` — replay consumers must tolerate unknown `type` values rather than rejecting the stream. New variants land in minor releases without bumping `schema_version`; the version increments only when an existing variant's shape changes.

## Lifecycle events

### `run_started`

Always the first event. Emitted before any node-level events.

```json
{ "type": "run_started", "run_id": "run_01J5K9..." }
```

| Field    | Type   | Description                                                                |
| -------- | ------ | -------------------------------------------------------------------------- |
| `run_id` | string | Run identifier. Format `run_<26-char Crockford ULID>`.                     |

### `run_finished`

Always the last event. Emitted after every node settles.

```json
{
  "type": "run_finished",
  "run_id": "run_01J5K9...",
  "ok": true,
  "failed_nodes": [],
  "skipped_nodes": []
}
```

| Field           | Type            | Description                                                                                              |
| --------------- | --------------- | -------------------------------------------------------------------------------------------------------- |
| `run_id`        | string          | Same `run_id` as `run_started`.                                                                          |
| `ok`            | boolean         | `true` iff every node finished `ok: true`.                                                               |
| `failed_nodes`  | array of string | Node ids that finished `ok: false`, in causal order.                                                     |
| `skipped_nodes` | array of string | Node ids that emitted `node_skipped`, in causal order.                                                   |

A tail-line read of `run_finished` summarizes the run's failure footprint without folding the full envelope log.

### `node_started`

Emitted just before a node's executor is invoked.

```json
{ "type": "node_started", "run_id": "run_...", "node_id": "review" }
```

### `node_finished`

Emitted after a node's executor returns (with success or failure).

```json
{
  "type": "node_finished",
  "run_id": "run_...",
  "node_id": "review",
  "ok": true,
  "failure": null
}
```

| Field     | Type            | Description                                                                                |
| --------- | --------------- | ------------------------------------------------------------------------------------------ |
| `ok`      | boolean         | `true` iff the executor's payload reported success.                                        |
| `failure` | NodeFailure?    | Populated **only** when `ok: false`. Internally tagged on `kind`. See [NodeFailure](#nodefailure). |

### `node_skipped`

Emitted for every transitively-dependent node when an upstream fails. Disjoint branches keep running.

```json
{
  "type": "node_skipped",
  "run_id": "run_...",
  "node_id": "publish",
  "reason": { "kind": "dependency_failed", "upstream": "review" }
}
```

`reason.upstream` names the **originating** failure, not the direct parent. `reason` is internally tagged on `kind` and `#[non_exhaustive]`.

### `node_timed_out`

Emitted when a node's `timeout:` budget elapses before the executor returns.

```json
{
  "type": "node_timed_out",
  "run_id": "run_...",
  "node_id": "long_query",
  "limit_secs": 30,
  "elapsed_ms": 30042
}
```

Paired with a subsequent `node_finished { ok: false, failure: { kind: "timed_out", limit_secs } }`.

## Agent-loop events

### `agent_iteration_started`

Emitted at the start of each agent loop turn, before the LLM transport is called.

```json
{
  "type": "agent_iteration_started",
  "run_id": "run_...",
  "node_id": "review",
  "iteration": 0
}
```

`iteration` is 0-based — a single-shot agent emits exactly `iteration: 0`.

### `llm_request_started`

Emitted immediately before the transport is called. Carries provider/model identifiers and head-truncated, redacted excerpts of the rendered prompt.

```json
{
  "type": "llm_request_started",
  "run_id": "run_...",
  "node_id": "review",
  "provider": "openrouter",
  "model": "anthropic/claude-sonnet-4.5",
  "prompt_excerpt": "Review the diff for...",
  "system_excerpt": "You are a senior reviewer..."
}
```

`system_excerpt` is `null` (not `""`) when the agent declared no system prompt.

Excerpts are bounded by the engine's excerpt cap and pass through the per-run redactor — `secrets.*` values never reach the wire.

### `llm_response_received`

Emitted immediately after a successful transport call.

```json
{
  "type": "llm_response_received",
  "run_id": "run_...",
  "node_id": "review",
  "finish_reason": "stop",
  "usage": { "prompt_tokens": 412, "completion_tokens": 178, "total_tokens": 590 },
  "content_excerpt": "The diff looks correct..."
}
```

`finish_reason` is the provider-normalized value (`stop`, `length`, `tool_calls`, etc.) or `null` when the provider did not return one. `usage` is `null` when the provider did not return token counts.

### `llm_request_failed`

Emitted when the transport returned `Err`. Always paired with the preceding `llm_request_started`.

```json
{
  "type": "llm_request_failed",
  "run_id": "run_...",
  "node_id": "review",
  "provider": "openrouter",
  "model": "anthropic/claude-sonnet-4.5",
  "failure": { "kind": "rate_limited" }
}
```

`failure` is internally tagged on `kind`. See [LlmFailure](#llmfailure).

### `tool_call_recorded`

Emitted after each successful or denied tool call within an agent iteration.

```json
{
  "type": "tool_call_recorded",
  "run_id": "run_...",
  "node_id": "review",
  "tool_name": "Read",
  "call_id": "toolu_01ABC...",
  "input_excerpt": "{\"path\":\"src/main.rs\"}",
  "output_excerpt": "fn main() {..."
}
```

On a denied call, `output_excerpt` carries the denial reason string. Excerpts are redacted and head-truncated.

### `tool_denied`

Emitted when a tool call is denied by the policy gate (mutation/network/context-write disallowed, or domain blocked).

```json
{
  "type": "tool_denied",
  "run_id": "run_...",
  "node_id": "review",
  "tool_name": "Bash",
  "reason": "allow_mutations is false"
}
```

Non-terminal — the loop continues with the denial fed back to the model. Always paired with a `tool_call_recorded` carrying the same denial string.

## Subagent events

### `subagent_started`

Emitted before entering a child agent loop.

```json
{
  "type": "subagent_started",
  "run_id": "run_...",
  "parent_node_id": "review",
  "child_agent": "security_lens",
  "depth": 1
}
```

`depth` is the child's depth (parent's depth + 1).

### `subagent_completed`

Emitted when a subagent dispatch returns successfully.

```json
{
  "type": "subagent_completed",
  "run_id": "run_...",
  "parent_node_id": "review",
  "child_agent": "security_lens",
  "depth": 1,
  "iterations": 4,
  "total_tokens": 1842
}
```

### `subagent_failed`

Emitted when a subagent dispatch returns `AgentError`. The `error` field is the rendered `Display` chain of the underlying error.

```json
{
  "type": "subagent_failed",
  "run_id": "run_...",
  "parent_node_id": "review",
  "child_agent": "security_lens",
  "depth": 1,
  "error": "BudgetExceeded { kind: Tokens, used: 5012, limit: 5000 }: ..."
}
```

The parent loop still feeds the failure back to its LLM as a denial-style `ToolResult` string; this event records the structured observability trail.

### `subagent_depth_exceeded`

Emitted when a subagent dispatch attempt would exceed `max_subagent_depth`. The child is **never entered**.

```json
{
  "type": "subagent_depth_exceeded",
  "run_id": "run_...",
  "parent_node_id": "review",
  "attempted_child_agent": "deep_lens",
  "depth_attempted": 4,
  "max_depth": 3
}
```

Non-terminal — the parent's loop continues with a denial.

## MCP events

### `mcp_server_starting`

Emitted before spawning an MCP server (stdio) or opening its connection (http).

```json
{
  "type": "mcp_server_starting",
  "run_id": "run_...",
  "server": "github",
  "transport": "stdio"
}
```

`transport` is `"stdio"` or `"http"`.

### `mcp_server_handshaked`

Emitted after `initialize` + `tools/list` complete.

```json
{
  "type": "mcp_server_handshaked",
  "run_id": "run_...",
  "server": "github",
  "tool_count": 14
}
```

`tool_count` is the number of tools the server advertised. Wildcard `mcp.<server>.*` entries in agent `allowed_tools` resolve against this list at handshake.

### `mcp_tool_call_sent`

Emitted immediately before a `tools/call` is issued.

```json
{
  "type": "mcp_tool_call_sent",
  "run_id": "run_...",
  "node_id": "triage",
  "server": "github",
  "tool": "search_issues",
  "call_id": "toolu_01XYZ...",
  "input_excerpt": "{\"q\":\"is:open label:bug\"}"
}
```

### `mcp_tool_call_completed`

Emitted immediately after a `tools/call` returns.

```json
{
  "type": "mcp_tool_call_completed",
  "run_id": "run_...",
  "node_id": "triage",
  "server": "github",
  "tool": "search_issues",
  "call_id": "toolu_01XYZ...",
  "ok": true,
  "output_excerpt": "[{\"number\":482,...}]"
}
```

`ok: false` discriminates server-reported tool errors from successful calls; the `output_excerpt` carries the error message in that case.

### `mcp_server_shutting_down`

Emitted before initiating clean shutdown at run end.

```json
{ "type": "mcp_server_shutting_down", "run_id": "run_...", "server": "github" }
```

### `mcp_server_exited`

Emitted after a clean shutdown completes.

```json
{ "type": "mcp_server_exited", "run_id": "run_...", "server": "github" }
```

### `mcp_server_crashed`

Emitted when a server exited mid-run unexpectedly. The owning agent terminates with a tool-call failure.

```json
{
  "type": "mcp_server_crashed",
  "run_id": "run_...",
  "server": "github",
  "reason": "process exited with code 137"
}
```

## Budget event

### `budget_exceeded`

Emitted when a run-level budget breaches. The breach is also surfaced on `node_finished.failure` with a typed `BudgetKind`.

```json
{
  "type": "budget_exceeded",
  "run_id": "run_...",
  "reason": "max_total_tokens=5000 exceeded by node review (used=5012)"
}
```

## NodeFailure

Carried on `node_finished.failure` exactly when `ok: false`. Internally tagged on `kind`, `#[non_exhaustive]`.

| `kind`                    | Fields                                                  | Cause                                                                                                                                |
| ------------------------- | ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `no_executor_registered`  | `node_kind: string`                                     | The node's `kind:` had no registered executor — configuration mismatch between YAML and the embedder's registry.                     |
| `template_render_failed`  | `error: string`                                         | MiniJinja rendering of the node's request failed (unknown variable, malformed expression, type mismatch).                            |
| `executor_error`          | `error: string`                                         | The executor returned `Err`. Process spawn, transport error, etc.                                                                    |
| `node_payload_failure`    | `exit_code: int?`, `stderr_tail: string?`               | The executor returned `Ok` but the payload signaled failure (today, only shell with non-zero exit). `stderr_tail` is bounded.        |
| `iteration_limit_exceeded`| `max_iterations: int`                                   | Agent reached `max_iterations` without converging on a `stop` finish reason.                                                         |
| `budget_exceeded`         | `budget_kind: "tokens" \| "tool_calls"`                 | Running budget breached.                                                                                                             |
| `tool_denied`             | `tool_name: string`, `reason: string`                   | Reserved for future strict-mode use. Today, denials are non-terminal and fed back to the model.                                      |
| `timed_out`               | `limit_secs: int`                                       | Node did not return before its `timeout:` elapsed. Paired with the preceding `node_timed_out` envelope.                              |

The field is `node_kind` (not `kind`) on `no_executor_registered`, and `budget_kind` on `budget_exceeded`, because `kind` is the serde tag discriminator on `NodeFailure` itself.

## LlmFailure

Carried on `llm_request_failed.failure`. Internally tagged on `kind`, `#[non_exhaustive]`. Mirrors the typed variants of [`LlmError`](errors.md#llmerror) so downstream alerting can branch on failure class without regex-matching error strings.

| `kind`            | Fields                                       | Cause                                                                                                            |
| ----------------- | -------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `auth_failed`     | none                                         | HTTP 401/403 or pre-flight auth check failed. Provider is on the parent envelope.                                 |
| `rate_limited`    | none                                         | HTTP 429.                                                                                                        |
| `model_not_found` | none                                         | HTTP 404 on the chat endpoint. Usually a model-name typo.                                                        |
| `api_error`       | `status: int`, `body_excerpt: string`        | Any other HTTP failure. `body_excerpt` is head-truncated.                                                        |
| `transport`       | `error: string`                              | Network/timeout/transport problem from the underlying client.                                                    |
| `config_error`    | `message: string`                            | Pre-flight misconfiguration caught before any network call.                                                      |
| `parse_error`     | `message: string`                            | Provider returned a payload the adapter could not parse.                                                         |
| `replay_miss`     | `key: string`                                | Replay tape miss. The caller is replaying against an incompatible tape.                                          |
| `other`           | `message: string`                            | Catch-all for legacy and future variants. Carries the rendered error chain so the cause is not lost.             |

## Excerpt and redaction rules

Every excerpt field on the wire is processed in this order before emission:

1. **Redaction.** `secrets.*` values that appear in the rendered text are replaced with `***`. Name-based — anything classified as a secret is redacted regardless of source. The redactor is per-run and applied in the agent loop before the event leaves the process.
2. **Truncation.** Bodies are head-truncated at the engine's `max_output_bytes` cap (default 2 KB; 64 KB with `--verbose`; explicit `--stderr-tail-bytes` always wins). The truncated form is suffixed with a single `…` character on a UTF-8 boundary.

The cap applies to: prompt and system excerpts on `llm_request_started`, content excerpts on `llm_response_received`, input/output excerpts on `tool_call_recorded` and `mcp_tool_call_sent`/`completed`, `body_excerpt` on `llm_failure.api_error`, and shell `stderr_tail` on `node_payload_failure`.

Stderr tails truncate from the **front** (the relevant signal sits at the end of stderr). HTTP error bodies and prompt/response excerpts truncate from the **back** (the actionable signal sits at the start of those payloads).

## See also

- [CLI › `orno run`](cli.md#orno-run) — how to capture and replay event streams.
- [Errors](errors.md) — typed error enums that mirror `NodeFailure` and `LlmFailure` on the internal API.
- [Exit codes](exit-codes.md) — when `run_finished.ok` and the process exit code can disagree.
- [Tools](tools.md) — what fires `tool_call_recorded`, `tool_denied`, and the MCP envelope variants.
