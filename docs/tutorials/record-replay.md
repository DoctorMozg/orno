# Record and replay a run

This tutorial covers orno's hero feature: recording a run into a self-contained bundle, then replaying that bundle offline with no LLM calls and no network. By the end you'll have a `bundle.ndjson` file you can rerun any time, on any machine, with no API key.

**Time:** 15 minutes. **Prerequisites:** Completed [Your first pipeline](first-pipeline.md). An `OPENROUTER_API_KEY` for the live recording (replay needs no key).

## Why record and replay matters

A live run depends on:

- The LLM being available and returning the same content. It won't, because models are non-deterministic.
- Tool side-effects reproducing: file system state, network responses, MCP server availability.
- API tokens being available and not exhausted.

A replayed run depends on **none of that**. The bundle is a self-contained tape of every external interaction, and replay serves every recorded LLM and tool result from it, so the event stream is reproducible. Use it for:

- **Postmortems.** Examine what an agent saw and did without re-spending tokens.
- **Integration testing.** Re-run a known-good pipeline in CI as a regression test.
- **Audit trails.** A replayable bundle is a verifiable record of one run's behavior.

## Step 1: Pick a pipeline with external interactions

A pipeline with no LLM call or tool call gives you nothing to capture. We'll use `examples/mcp-http-demo/pipeline.yaml`, which talks to a public, no-auth MCP server (GitMCP) and produces a one-paragraph documentation summary.

```bash
cd <path-to-orno>
ls examples/mcp-http-demo/
```

You'll see:

- `pipeline.yaml`: the agent and MCP server config.
- `record-replay.sh`: a three-phase walkthrough script (we'll do the steps by hand).
- `bundle.ndjson`: a pre-recorded bundle, safe to commit.

Open `pipeline.yaml`. The relevant bits:

- `mcp_servers.gitmcp` is configured with `transport: http`, `url: https://gitmcp.io/...`, and `auth.kind: none`.
- `agents.doc_summarizer` has `allowed_tools: ["mcp.gitmcp.*"]`, a wildcard that expands at run start against the server's advertised tools.
- A single node, `summarize`, runs the agent.

## Step 2: Run live (one-time spend)

Export your key (orno reads the provider key from the environment and redacts it) and execute the pipeline normally:

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
orno run examples/mcp-http-demo/pipeline.yaml
```

This is a regular run. You'll see `run_started`, `mcp_server_starting` / `mcp_server_handshaked`, an `llm_response_received` with a content excerpt, an `mcp_tool_call_sent` / `mcp_tool_call_completed` pair, another `llm_response_received`, and `run_finished` with `ok: true`.

That output is your reference. Save it for comparison:

```bash
orno run examples/mcp-http-demo/pipeline.yaml \
  > live-events.ndjson 2> live-trace.log
```

## Step 3: Run again, this time with `--record-bundle`

The flag tells orno to capture every external interaction into a bundle file:

```bash
orno run examples/mcp-http-demo/pipeline.yaml \
  --record-bundle bundle.ndjson
```

Execution is identical to step 2: same LLM call, same MCP exchanges, same outputs. The only difference is that orno also writes `bundle.ndjson` alongside its stdout event stream.

Inspect it:

```bash
wc -l bundle.ndjson
head -1 bundle.ndjson | jq '.type'          # "bundle_header"
grep -c '"type":"llm_entry"' bundle.ndjson
```

A bundle is one JSON object per line, tagged on `type`. It opens with a `bundle_header` (`format_version: 2`), then one `pipeline_yaml` line with the pipeline text and one `run_inputs` line (rendered `vars`, `env`, and secret names with masked values). After those come the `llm_entry` and `tool_entry` lines, each `{key, seq, outcome}`, and a closing `bundle_trailer` with a BLAKE3 digest. The event log isn't in there, because replay regenerates it. Secret values never enter the bundle: entries are recorded with secrets masked as `***`, and literal MCP credentials in the embedded pipeline YAML become `<REDACTED>`.

## Step 4: Replay offline

Now run the bundle through `orno replay`:

```bash
orno replay bundle.ndjson
```

What just happened:

- No LLM call. The model's responses came from the bundle's recorded LLM tapes.
- No network. The MCP server is **not** spawned, and its responses came from the recorded MCP tapes.
- No `OPENROUTER_API_KEY` required. Replay needs no credentials.

The output is the same NDJSON event stream as the live run, apart from two things that change every run by design:

1. `run_id`, a fresh ULID per run.
2. Wall-clock `timestamp` fields, because replay records replay time, not original time.

Everything else matches the live run: `seq`, `content_excerpt`, tool arguments, tool results, errors, exit code.

## Step 5: Verify the determinism

Compare the live and replayed event streams, ignoring the two known differences:

```bash
orno replay bundle.ndjson > replay-events.ndjson

# Strip run_id and timestamp before diffing.
jq -c 'del(.timestamp, .event.run_id)' live-events.ndjson > live-stripped.ndjson
jq -c 'del(.timestamp, .event.run_id)' replay-events.ndjson > replay-stripped.ndjson

diff live-stripped.ndjson replay-stripped.ndjson
```

A clean diff (no output) confirms every other field matches.

## Step 6: Try a tape miss

What happens if you replay a bundle after changing the pipeline? Edit `pipeline.yaml` to add a new agent or change the prompt, then replay the original bundle:

```bash
# Add a fictitious second tool to the agent or change the initial_prompt.
orno replay bundle.ndjson
```

The first iteration that diverges from the recorded tape fails with an `llm_request_failed` carrying `failure.kind: replay_miss` (or a tool-tape-miss error). The node finishes `ok: false` and `orno replay` exits `1`. This is intentional: a tape miss is a **hard error**, not a fallback to the live API.

A soft fallback would quietly turn an audit replay into a re-run and erase the determinism guarantee. Failing loudly forces the operator to reconcile the difference.

## Step 7: Use the bundle in CI

A replayable bundle is about the simplest integration test an agent pipeline can have:

```yaml
# .github/workflows/agent-test.yml — sketch
- name: Regression-test the pipeline
  run: |
    orno replay tests/fixtures/agent-bundle.ndjson > /tmp/replay-events.ndjson
    # Assert on specific events, e.g. final node ok status:
    jq -e 'select(.event.type == "run_finished") | .event.ok' /tmp/replay-events.ndjson
```

Check the bundle in next to the pipeline. A code change that breaks the pipeline (renames a tool, changes the agent's surface) breaks the replay test. A change that doesn't affect agent behavior, like a style fix or a refactor, leaves it passing.

## Tape portability constraints

A bundle is a deterministic record of one specific (provider × model × orno build) combination. It is **not** a portable transcript you can replay across tooling boundaries. Concretely:

- **Per-provider.** The tool-tape key is `blake3(tool_name : call_id : args)`, and `call_id` values come from the LLM provider. Anthropic and OpenAI use different ID formats for the same logical call, so an Anthropic-recorded bundle replayed against an OpenAI-configured pipeline will surface cascading `ReplayMiss` errors. Re-record per provider instead of swapping.
- **Per-orno-version.** Bundles carry a `format_version` field in the `bundle_header` line (currently `2`). Each bump of `CURRENT_BUNDLE_VERSION` marks an incompatible change to the wire format or to the tape-key derivation. A bundle of any other version, including version 1, is rejected up front with `BundleError::IncompatibleVersion` naming both versions, so the contents are never silently misread. Re-record after upgrading orno across a format boundary.
- **Pipeline-fingerprint sensitivity.** The LLM tape key is `provider:model:` plus a BLAKE3 hash of the canonical JSON form of the request with secrets masked, including the tool list, the system prompt, and any sampling parameters. Any pipeline change that alters those fields invalidates every existing tape entry. That covers adding a tool, editing the system prompt, or switching to a different sampling temperature. The replay then surfaces a `replay_miss` failure on the first iteration that diverges.

The format-version gate is deliberate. It turns what would be a silent misreading into a structured error the operator can act on. Treat the bundle as a per-pipeline, per-build artifact, and re-record whenever you cross one of the boundaries above.

## What you've learned

- `orno run --record-bundle <file>` captures a run into a bundle.
- `orno replay <file>` re-executes the pipeline from the bundle with no LLM and no network.
- Tape misses are hard errors, not soft fallbacks.
- Replays differ from live runs only on `run_id` and timestamps. `kind: shell` nodes cannot be replayed.
- Bundles are useful for postmortems, regression tests, and audit trails.
- Bundles are per-provider, per-orno-version, and per-pipeline-fingerprint, so re-record across any of those boundaries.

## Next steps

- [Multi-agent PR review](multi-agent-pr-review.md): using `subagent.<name>` for delegation.
- [Pipeline YAML › record/replay flags](../reference/cli.md#orno-run): every flag of `orno run` and `orno replay`.
- [Strict agentic loops › Bounded non-determinism](../explanation/strict-agentic-loops.md#5-bounded-non-determinism): why replay is load-bearing for the contract.
