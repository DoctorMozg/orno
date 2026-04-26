# How to debug a failed run

A pipeline failed and you need to know why. This guide walks through the systematic approach: read the streams, isolate the failed node, identify the failure variant, and (if possible) replay to inspect deterministically.

## Step 1 — Capture both streams

Always capture stdout and stderr separately. They serve different audiences:

```bash
orno run pipeline.yaml --secrets-file .env.secrets \
  > events.ndjson 2> trace.log
```

- `events.ndjson` — the user-facing event stream. NDJSON envelopes, one per line.
- `trace.log` — internal observability. Tracing JSON; useful for low-level debugging when the event stream isn't enough.

If the run was already done and you didn't capture the streams: re-run with capture enabled. orno is reproducible enough that the same failure will reproduce (subject to LLM non-determinism). For deterministic replay, use `--record-bundle` and `orno replay`.

## Step 2 — Find the run's exit status

```bash
echo $?
```

- `0` — pipeline succeeded.
- non-zero — pipeline failed (load, validation, or any node).

The exit code is the first signal. Now find which node owns the failure.

## Step 3 — Find the failed node

The failure surfaces on `node_finished` with `ok: false`:

```bash
jq -c 'select(.event.type == "node_finished" and .event.ok == false)' events.ndjson
```

Output (typical):

```json
{
  "event": {
    "type": "node_finished",
    "node_id": "review",
    "ok": false,
    "duration_ms": 12500,
    "failure": {
      "kind": "BudgetExceeded",
      "budget_kind": "Tokens",
      "limit": 30000,
      "consumed": 30214
    }
  }
}
```

If multiple nodes failed (e.g., a downstream node was skipped because of upstream failure), `run_finished.failed_nodes` lists them in causal order:

```bash
jq -c 'select(.event.type == "run_finished")' events.ndjson
```

```json
{
  "event": {
    "type": "run_finished",
    "ok": false,
    "failed_nodes": ["review"],
    "skipped_nodes": ["publish_results"]
  }
}
```

A skipped node didn't fail itself; it was skipped because an ancestor failed. To see why a specific node was skipped:

```bash
jq -c 'select(.event.type == "node_skipped")' events.ndjson
```

## Step 4 — Identify the failure variant

The `failure.kind` is a typed discriminator. The full set:

- **`NoExecutorRegistered`** — an unrecognized `kind:` in the pipeline. Check the node's `kind:` field; it should be `agent` or `shell`.
- **`TemplateRenderFailed`** — a MiniJinja template referenced something that doesn't exist. Check the template literal: `{{ vars.* }}`, `{{ env.* }}`, `{{ nodes.<id>.* }}`.
- **`ExecutorError`** — the executor returned an error before producing output. Read the inner `message` field.
- **`NodePayloadFailure`** — shell node exited non-zero, or agent node terminated with a strict-mode breach. Read `exit_code` and `stderr_tail` for shell; agent failures cascade through `AgentError`.
- **`IterationLimitExceeded`** — agent loop hit `max_iterations`. Tighten the prompt or raise the limit.
- **`BudgetExceeded`** — agent loop hit `max_total_tokens` or `max_tool_calls`. `budget_kind` discriminates; see [tighten-budget](tighten-budget.md).
- **`ToolDenied`** — an effect-class denial. Most denials are non-terminal; this variant fires only when the loop terminates around a denial. Read the `tool` and `reason` fields.
- **`TimedOut`** — wall-clock deadline reached. Check the `timeout:` field on the node; raise it or speed up the work.

For LLM-specific failures, the run also emits `Event::LlmRequestFailed` paired with the failed `LlmRequestStarted`:

```bash
jq -c 'select(.event.type == "llm_request_failed")' events.ndjson
```

```json
{
  "event": {
    "type": "llm_request_failed",
    "failure": {
      "kind": "AuthFailed",
      "provider": "openrouter"
    }
  }
}
```

`LlmFailure` variants: `AuthFailed`, `RateLimited`, `ModelNotFound`, `ApiError { status, body_excerpt }`, `Transport`, `ConfigError`, `ParseError`, `ReplayMiss`, `Other`.

## Step 5 — Trace the agent's reasoning

For agent nodes, the most informative envelope is `agent_iteration_finished`. It records the model's content and tool calls per turn:

```bash
jq -c 'select(.event.type == "agent_iteration_finished")' events.ndjson
```

You'll see one envelope per loop iteration with `iteration`, `cumulative_tokens`, `cumulative_tool_calls`, `tool_calls_emitted`, and `content_excerpt`. The content excerpt is truncated for readability — the full content is in the recorded bundle if you ran with `--record-bundle`.

For tool-call detail:

```bash
jq -c 'select(.event.type == "tool_invoked")' events.ndjson
```

Each call shows the arguments (with secrets redacted) and the result. A failed tool call shows up as `tool_failed` instead.

## Step 6 — If the run was recorded, replay it

If you ran with `--record-bundle bundle.ndjson`, you can re-execute the pipeline against the recorded tapes:

```bash
orno replay bundle.ndjson 2>&1 | tee replay-events.ndjson
```

Replay is **byte-for-byte deterministic** modulo `run_id` and timestamps. This means:

- The failure is reproducible without spending tokens.
- You can iterate on inspection (different `jq` filters, manual envelope reads) as many times as you like.
- A bug in your pipeline that depends on LLM non-determinism is invisible during replay — but a bug in tool dispatch, template rendering, or node ordering is reproducible.

The bundle also contains the full LLM and tool tapes, which preserve the exact bytes the model emitted. You can read them directly:

```bash
jq 'select(.kind == "llm_response")' bundle.ndjson
jq 'select(.kind == "tool_response")' bundle.ndjson
```

## Step 7 — Use `orno validate` to catch load-time bugs

If the failure is at pipeline load (exit code non-zero, no `run_started` envelope), the YAML didn't pass validation. Run `orno validate` directly:

```bash
orno validate pipeline.yaml
```

You'll get a typed error pointing at the offending field. Common causes:

- **`UnknownTool`** — typo in `allowed_tools` (e.g., `Reed` instead of `Read`).
- **`DuplicateNode`** — two nodes with the same `id:`.
- **`DependencyCycle`** — cycle in `needs:` declarations.
- **`UnknownAgent`** — node references an `agent:` that isn't defined in `agents:`.
- **`ChildExceedsParentPolicy`** — a subagent has more permissive effects than its parent.
- **`MissingSecret`** — pipeline references a secret name that's not declared in `secrets:`.
- **`UnknownEnvVar`** — pipeline references `env.NAME` but `pass_env:` doesn't include `NAME`.

## Step 8 — Dig into the trace log

If the event stream isn't enough — for example, a transport-level error that didn't produce a typed `LlmFailure` — read the trace log:

```bash
jq -c 'select(.level == "ERROR" or .level == "WARN")' trace.log
```

Tracing fields use OpenTelemetry-style naming: `pipeline.run_id`, `node.id`, `tool.name`, `llm.model`, etc. So you can filter on the dimensions of interest:

```bash
jq -c 'select(."node.id" == "review")' trace.log
```

The tracing log is internal observability — not a stable API — but it's where you'll find low-level details when the event stream's typed failures don't have enough context.

## Common failure patterns

### "Pipeline ran fine, then suddenly fails"

Usually one of:

- **API rate limit.** `LlmFailure::RateLimited`. Wait or move to a different provider key.
- **Quota exhaustion.** `LlmFailure::AuthFailed` after a successful auth — usually means the account is out of credits.
- **Model deprecation.** `LlmFailure::ModelNotFound`. The provider removed the model; pin a different one.

### "Agent never converges"

`IterationLimitExceeded` after exhausting `max_iterations`. The model isn't reaching a final-content turn. Common causes:

- The system prompt asks for an impossible thing (e.g., "search this URL" when WebFetch isn't allowed).
- The model keeps re-calling the same tool with no progress (see `tool_invoked` for repeating calls with identical arguments).
- The tool surface is too narrow — the model lacks the tool it needs and keeps trying.

Fix the prompt or the surface; raising the iteration limit is rarely the right move.

### "Node failed with `TemplateRenderFailed`"

A template references something that doesn't exist. Common causes:

- `{{ nodes.upstream.output }}` but the upstream node failed (so its output is undefined).
- `{{ env.MY_VAR }}` but `MY_VAR` is not in `pass_env:`.
- `{{ vars.typo }}` instead of the actual name.
- `{{ nodes.x.state.y }}` but the upstream agent never called `SetState` with key `y`.

The error message names the missing reference. Fix the template or fix the upstream.

### "Tool worked yesterday, fails today"

For `Bash`/`WebFetch` tools that depend on external state: external state changed.

For MCP tools: the MCP server may have changed its tool surface. Run with verbose logging (`-v`) to see the `tools/list` response at handshake.

For `Edit`: the file changed. `Edit` requires the `old_string` to be unique in the file; a recent edit may have made it ambiguous.

## See also

- [Errors](../reference/errors.md) — every typed error variant.
- [Events](../reference/events.md) — every event type and its fields.
- [Exit codes](../reference/exit-codes.md) — what each non-zero exit means.
- [Tutorials › Record and replay](../tutorials/record-replay.md) — capturing a bundle for offline inspection.
