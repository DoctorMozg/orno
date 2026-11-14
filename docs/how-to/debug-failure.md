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
- `1` — pipeline failed to load or validate, or any node failed.
- `130` — interrupted by SIGINT.

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
    "run_id": "run_...",
    "node_id": "review",
    "node_kind": "agent",
    "ok": false,
    "failure": {
      "kind": "budget_exceeded",
      "budget_kind": "tokens"
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
    "run_id": "run_...",
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

The `failure.kind` is a snake_case discriminator. The full set:

- **`no_executor_registered`** — no executor for the node's kind (`node_kind` field). Only reachable when orno is embedded with a custom registry.
- **`template_render_failed`** — a MiniJinja template referenced something that doesn't exist. Read the `error` field and check `{{ vars.* }}`, `{{ env.* }}`, `{{ nodes.<id>.* }}`.
- **`executor_error`** — the executor returned an error before producing output (spawn failure, agent-loop error, LLM transport error). Read the `error` field.
- **`node_payload_failure`** — a shell node exited non-zero or was killed. Read `exit_code` (null when killed by a signal), `signal`, and `stderr_tail`.
- **`iteration_limit_exceeded`** — agent loop hit `max_iterations`. Tighten the prompt or raise the limit.
- **`budget_exceeded`** — agent loop hit `max_total_tokens` or `max_tool_calls`. `budget_kind` is `tokens` or `tool_calls`; see [tighten-budget](tighten-budget.md).
- **`tool_denied`** — reserved; effect-class denials are fed back to the model and surface as `tool_denied` events, not as a node failure.
- **`timed_out`** — wall-clock deadline reached (`limit_secs`). Check the `timeout:` field on the node.

For LLM-specific failures, the run also emits `Event::LlmRequestFailed` paired with the failed `LlmRequestStarted`:

```bash
jq -c 'select(.event.type == "llm_request_failed")' events.ndjson
```

```json
{
  "event": {
    "type": "llm_request_failed",
    "run_id": "run_...",
    "node_id": "review",
    "provider": "openrouter",
    "model": "openai/gpt-5",
    "failure": {
      "kind": "auth_failed"
    }
  }
}
```

`LlmFailure` kinds: `auth_failed`, `rate_limited`, `model_not_found`, `api_error` (`status`, `body_excerpt`), `transport`, `config_error`, `parse_error`, `replay_miss`, `other`. A missing API key is a `config_error` whose message names the env var to set.

## Step 5 — Trace the agent's reasoning

For agent nodes, read the per-turn envelopes. `agent_iteration_started` marks each turn, `llm_response_received` carries `finish_reason`, `usage` and a `content_excerpt`:

```bash
jq -c 'select(.event.type == "agent_iteration_started" or .event.type == "llm_response_received")' events.ndjson
```

Excerpts are redacted and truncated; the full content is in the recorded bundle if you ran with `--record-bundle`.

For tool-call detail:

```bash
jq -c 'select(.event.type == "tool_call_recorded")' events.ndjson
```

Each call shows `input_excerpt` and `output_excerpt` (with secrets redacted). A recoverable tool failure appears there as an `output_excerpt` starting `error:`; a policy denial starts `denied:` and has a matching `tool_denied` event.

## Step 6 — If the run was recorded, replay it

If you ran with `--record-bundle bundle.ndjson`, you can re-execute the pipeline against the recorded tapes:

```bash
orno replay bundle.ndjson | tee replay-events.ndjson
```

Replay reproduces the recorded LLM and tool results, so the event stream matches modulo `run_id` and timestamps (`kind: shell` nodes are refused during replay). This means:

- The failure is reproducible without spending tokens.
- You can iterate on inspection (different `jq` filters, manual envelope reads) as many times as you like.
- A bug in your pipeline that depends on LLM non-determinism is invisible during replay — but a bug in tool dispatch, template rendering, or node ordering is reproducible.

The bundle (format version 2) also contains the LLM and tool tapes as `llm_entry` and `tool_entry` lines, each `{key, seq, outcome}`, which preserve what the model and tools returned. You can read them directly:

```bash
jq 'select(.type == "llm_entry")' bundle.ndjson
jq 'select(.type == "tool_entry")' bundle.ndjson
```

## Step 7 — Use `orno validate` to catch load-time bugs

If the failure is at pipeline load (exit code non-zero, empty stdout), the YAML didn't pass validation. Run `orno validate` directly:

```bash
orno validate pipeline.yaml
```

You'll get every problem at once, each naming the offending node, agent or key. Common causes:

- **Unknown key** — a typo like `need:` or `blocked_domain:`; unknown keys are rejected.
- **Not a known builtin, subagent reference, or mcp tool** — typo in `allowed_tools` (e.g., `Reed` instead of `Read`).
- **Cycle or undefined `needs:`** — `InvalidGraph`.
- **Undeclared agent** — node references an `agent:` that isn't defined in `agents:`.
- **Cannot delegate** — a subagent is more permissive than its parent.
- **Listed in both `pass_env` and `secrets`** — keep it in `secrets:` only.
- **`nodes.<id>` is not an upstream dependency** — add the node to `needs:`.
- **Lists `Bash` but declares `allowed_domains`/`roots`** — drop `Bash` or set `unconfined_shell: true`.

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
- The model keeps re-calling the same tool with no progress (see `tool_call_recorded` for repeating calls with identical arguments).
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
