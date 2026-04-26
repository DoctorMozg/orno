# `mcp-http-demo` — streamable-HTTP MCP, plus a record/replay walkthrough

One agent that talks to a public, no-auth MCP HTTP server (GitMCP) and produces a one-paragraph summary of a public documentation surface. The folder also ships a three-phase shell script that exercises **live**, **record**, and **replay** in order.

## Surface exercised

- `mcp_servers.*.transport: http` with `auth.kind: none` — the streamable-HTTP transport path.
- Wildcard-form `allowed_tools: ["mcp.gitmcp.*"]` expanded against the server's advertised tool list at run start.
- Record-and-replay loop: live → recorded bundle → offline replay producing the same event sequence.
- `--secrets-file` integration with the per-run redactor (the OpenRouter key is stripped from the bundle).

## Inputs

- `OPENROUTER_API_KEY` — provider credential, expected at `.env.secrets` for the script. Required for phases 1 and 2 (live, record); not required for phase 3 (replay).

## Run live

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
cargo run -p orno-cli -- run examples/mcp-http-demo/pipeline.yaml \
  --secrets-file .env.secrets
```

## Record + replay

The shipped script chains all three phases:

```bash
bash examples/mcp-http-demo/record-replay.sh
```

It runs:

1. Live (real OpenRouter LLM call + real GitMCP HTTP traffic).
2. Record (same execution, plus a bundle file at `examples/mcp-http-demo/bundle.ndjson`).
3. Replay (drives the agent loop entirely from the bundle — no LLM cost, no network).

After the walkthrough, replay the bundle by itself any time:

```bash
cargo run -p orno-cli -- replay examples/mcp-http-demo/bundle.ndjson
```

## Files in this folder

- `pipeline.yaml` — the agent + MCP server config.
- `record-replay.sh` — three-phase walkthrough.
- `bundle.ndjson` — pre-recorded bundle (safe to commit; GitMCP needs no auth header).
- `README.md` — this file.

## Notes

- The agent's `allow_mutations: true` is required because orno classifies every MCP tool as both mutating and networked (it cannot introspect a remote server's per-tool semantics at registration time). GitMCP is read-only by construction, so this grant is safe for this demo.
- To swap servers, change `mcp_servers.gitmcp.url` to a different `https://gitmcp.io/<owner>/<repo>` and update `allowed_tools` if the new repo's tool surface is materially different. Authenticated servers can replace `auth.kind: none` with `kind: bearer` and pull the token from `secrets.<NAME>`.
