# Record and replay a run

This tutorial walks through orno's hero feature: recording a run into a self-contained bundle, then replaying that bundle offline with no LLM calls and no network. By the end you'll have a `bundle.ndjson` file you can rerun any time, on any machine, with no API key.

**Time:** 15 minutes. **Prerequisites:** Completed [Your first pipeline](first-pipeline.md). An `OPENROUTER_API_KEY` for the live recording (replay needs no key).

## Why record and replay matters

A live run depends on:

- The LLM being available and returning the same content (it won't — models are non-deterministic).
- Tool side-effects reproducing (file system state, network responses, MCP server availability).
- API tokens being available and not exhausted.

A replayed run depends on **none of that**. The bundle is a self-contained tape of every external interaction. Replay is bit-for-bit deterministic against the bundle. Use it for:

- **Postmortems** — examine what an agent saw and did without re-spending tokens.
- **Integration testing** — re-run a known-good pipeline in CI as a regression test.
- **Audit trails** — a replayable bundle is a verifiable record of one run's behavior.

## Step 1 — Pick a pipeline with external interactions

Recording a pipeline that has no LLM call or no tool call is uninteresting — there's nothing external to capture. We'll use `examples/mcp-http-demo/pipeline.yaml`, which exercises a public, no-auth MCP server (GitMCP) and produces a one-paragraph documentation summary.

```bash
cd <path-to-orno>
ls examples/mcp-http-demo/
```

You'll see:

- `pipeline.yaml` — the agent + MCP server config.
- `record-replay.sh` — a three-phase walkthrough script (we'll do the steps by hand).
- `bundle.ndjson` — a pre-recorded bundle, safe to commit.

Have a look at `pipeline.yaml`. The relevant bits:

- `mcp_servers.gitmcp` configured with `transport: http`, `url: https://gitmcp.io/...`, and `auth.kind: none`.
- `agents.doc_summarizer` with `allowed_tools: ["mcp.gitmcp.*"]` (a wildcard that expands at run start against the server's advertised tools).
- A single node `summarize` that runs the agent.

## Step 2 — Run live (one-time spend)

Set up your secrets and execute the pipeline normally:

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
orno run examples/mcp-http-demo/pipeline.yaml --secrets-file .env.secrets
```

This is a regular run. You'll see `run_started`, MCP handshake events, an `llm_request_succeeded` with a content excerpt, an MCP tool call, another `llm_request_succeeded`, and `run_finished` with `ok: true`.

This is your reference output. Save it for comparison:

```bash
orno run examples/mcp-http-demo/pipeline.yaml --secrets-file .env.secrets \
  > live-events.ndjson 2> live-trace.log
```

## Step 3 — Run again, this time with `--record-bundle`

The flag tells orno to capture every external interaction into a bundle file:

```bash
orno run examples/mcp-http-demo/pipeline.yaml \
  --secrets-file .env.secrets \
  --record-bundle bundle.ndjson
```

The execution is identical to step 2 — same LLM call, same MCP exchanges, same outputs. The only difference is that orno writes `bundle.ndjson` alongside its stdout event stream.

Have a look:

```bash
wc -l bundle.ndjson
head -1 bundle.ndjson | jq '.kind'
grep -c llm_request_succeeded bundle.ndjson
```

The bundle contains every LLM request/response pair, every MCP exchange, every tool call/result, the final event log, and metadata about the original run. Secrets are redacted (the OpenRouter key is replaced with `[REDACTED]`).

## Step 4 — Replay offline

Now run the bundle through `orno replay`:

```bash
orno replay bundle.ndjson
```

What just happened:

- No LLM call. The model's responses are served from the bundle's recorded LLM tapes.
- No network. The MCP server is **not** spawned; its responses are served from the recorded MCP tapes.
- No `OPENROUTER_API_KEY` required. Replay needs no credentials.

The output is the same NDJSON event stream as the live run, modulo two things that change every run by design:

1. `run_id` — a fresh ULID per run.
2. Wall-clock `timestamp` fields — replay records replay time, not original time.

Everything else — `seq`, `content_excerpt`, tool arguments, tool results, errors, exit code — matches the live run.

## Step 5 — Verify the determinism

Compare the live and replayed event streams modulo the two known differences:

```bash
orno replay bundle.ndjson > replay-events.ndjson

# Strip run_id and timestamp before diffing.
jq -c 'del(.run_id, .timestamp)' live-events.ndjson > live-stripped.ndjson
jq -c 'del(.run_id, .timestamp)' replay-events.ndjson > replay-stripped.ndjson

diff live-stripped.ndjson replay-stripped.ndjson
```

A clean diff (no output) confirms byte-level reproduction of every other field.

## Step 6 — Try a tape miss

What happens if you replay a bundle but change the pipeline? Try editing `pipeline.yaml` and adding a new agent or changing the prompt, then replay the original bundle:

```bash
# Add a fictitious second tool to the agent or change the initial_prompt.
orno replay bundle.ndjson
```

The first iteration that diverges from the recorded tape will fail with an `LlmFailure::ReplayMiss` (or a tool-tape-miss error). This is intentional: a tape miss is a **hard error**, not a fallback to the live API.

The reason: a soft fallback would silently turn an audit replay into a re-run and erase the determinism guarantee. Better to fail loudly and force the operator to reconcile.

## Step 7 — Use the bundle in CI

A replayable bundle is the simplest possible integration test for an agent pipeline:

```yaml
# .github/workflows/agent-test.yml — sketch
- name: Regression-test the pipeline
  run: |
    orno replay tests/fixtures/agent-bundle.ndjson > /tmp/replay-events.ndjson
    # Assert on specific events, e.g. final node ok status:
    jq -e 'select(.event.type == "run_finished") | .event.ok' /tmp/replay-events.ndjson
```

The bundle is checked in alongside the pipeline. A code change that breaks the pipeline (renames a tool, changes the agent's surface) breaks the replay test. A change that doesn't affect agent behavior (style, refactor) does not — the test passes.

## Tape portability constraints

A bundle is a deterministic record of one specific (provider × model × orno build) combination. It is **not** a portable transcript that can be replayed across tooling boundaries. Concretely:

- **Per-provider.** The tool-tape key is `blake3(tool_name : call_id : args)`, and `call_id` values are emitted by the LLM provider. Anthropic and OpenAI use different ID formats for the same logical call, so an Anthropic-recorded bundle replayed against an OpenAI-configured pipeline will surface cascading `ReplayMiss` errors. Re-record per provider rather than swap.
- **Per-orno-version.** Bundles carry a `format_version` field in the `bundle_header` line. Each bump of `CURRENT_BUNDLE_VERSION` indicates an incompatible change to the wire format or to the tape-key derivation; an older bundle replayed by a newer reader is rejected up front with `BundleError::IncompatibleVersion` rather than silently misinterpreting the contents. Re-record after upgrading orno across a format boundary.
- **Pipeline-fingerprint sensitivity.** The LLM tape key hashes the canonical JSON form of the entire request, including the tool list, the system prompt, and any sampling parameters. Any change to the pipeline that alters those fields — adding a tool, editing the system prompt, switching to a different sampling temperature — invalidates every existing tape entry. The replay then surfaces `LlmFailure::ReplayMiss` on the first iteration that diverges.

The format-version gate is intentional: it converts what would otherwise be a silent misinterpretation into a structured error the operator can act on. Treat the bundle as a per-pipeline, per-build artifact, and re-record after any of the boundaries above is crossed.

## What you've learned

- `orno run --record-bundle <file>` captures a run into a bundle.
- `orno replay <file>` re-executes the pipeline from the bundle with no LLM, no network.
- Tape misses are hard errors, not soft fallbacks.
- Replays differ from live runs only on `run_id` and timestamps.
- Bundles are useful for postmortems, regression tests, and audit trails.
- Bundles are per-provider, per-orno-version, and per-pipeline-fingerprint — re-record across any of those boundaries.

## Next steps

- [Multi-agent PR review](multi-agent-pr-review.md) — using `subagent.<name>` for delegation.
- [Pipeline YAML › record/replay flags](../reference/cli.md#orno-run) — every flag of `orno run` and `orno replay`.
- [Strict agentic loops › Bounded non-determinism](../explanation/strict-agentic-loops.md#5-bounded-non-determinism) — why replay is load-bearing for the contract.
