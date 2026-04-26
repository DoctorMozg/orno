# Example pipelines

Each subdirectory holds one runnable orno pipeline plus any artifacts (bundles, scripts, READMEs) that example needs. Pipelines are named `pipeline.yaml` so the path stays uniform: `examples/<name>/pipeline.yaml`.

| Example                                              | Surface exercised                                                                                                            | LLM-key required to run live |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ---------------------------- |
| [`hello/`](hello/)                                   | Smallest working pipeline. One agent node, one iteration, no tools. Suitable for `--replay-tape` smoke tests.                | yes (or dummy transport)     |
| [`pr-review/`](pr-review/)                           | Multi-agent: parent reviewer delegates to three read-only lens subagents and synthesizes a single verdict.                   | yes                          |
| [`flaky-test-triage/`](flaky-test-triage/)           | Single agent with a broad tool surface (Bash, Read, Edit, Write, WebFetch, MCP filesystem + GitHub) and full effect grants.   | yes                          |
| [`release-notes/`](release-notes/)                   | Three-node DAG: shell collector → networked enricher (WebFetch, allow-listed domains) → mutating synthesizer.                 | yes                          |
| [`commit-digest/`](commit-digest/)                   | Exercises every builtin tool — Bash, Read, WebFetch, Edit, Write, plus a `subagent.contributor_vibes` delegation.            | yes                          |
| [`scoped-state/`](scoped-state/)                     | Demonstrates `SetState` + `policy.allow_context_writes` + downstream `nodes.<id>.state.<key>` reads.                         | yes                          |
| [`mcp-http-demo/`](mcp-http-demo/)                   | Streamable-HTTP MCP transport via the public, no-auth GitMCP server. Bundle + record/replay walkthrough script included.      | yes (record-only)            |

## Running an example

Most examples assume `OPENROUTER_API_KEY` is set. The simplest setup is a `.env.secrets` file at the repo root:

```
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
cargo run -p orno-cli -- run examples/hello/pipeline.yaml --secrets-file .env.secrets
```

For a no-key smoke test, use the dummy transport:

```
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

Several examples consume environment inputs via `pass_env:` — `pr-review/` wants `PR_NUMBER`, `flaky-test-triage/` wants `TEST_PATH`, `CI_LOG_URL`, and `GITHUB_REPO`, and so on. Their per-example READMEs spell out the inputs.

## Layout convention

```
examples/
  hello/
    pipeline.yaml
    README.md
  mcp-http-demo/
    pipeline.yaml
    README.md
    record-replay.sh
    bundle.ndjson
```

When adding a new example, drop a folder here, name the pipeline `pipeline.yaml`, write a short README with the surface exercised and the inputs needed, and add a row to the table above.
