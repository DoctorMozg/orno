# `flaky-test-triage` — broad tool surface with full effect grants

A single agent equipped with the full builtin set plus two MCP servers. Demonstrates `allow_mutations: true` + `allow_network: true` + an explicit `allowed_domains` allowlist on a single agent.

## Surface exercised

- Every effect class in one agent: `Read`, `Edit`, `Write`, `Bash`, `WebFetch`, plus `mcp.filesystem.*` and selected `mcp.github.*` tools.
- Domain allowlist enforcement on `WebFetch` and network-capable MCP calls.
- Stdio MCP server with a templated env block (`GITHUB_TOKEN: "{{ secrets.GITHUB_TOKEN }}"`).
- `pass_env:` for visible inputs and `secrets:` for credential routing.
- A shell node fetching the failing CI log, then an agent node consuming it.

## Inputs

- `TEST_PATH` — path to the failing test (`pass_env`).
- `CI_LOG_URL` — URL of the failing CI log (`pass_env`). Curl-fetched by the shell node.
- `GITHUB_REPO` — `owner/repo` slug used in the prompt and `mcp.github.*` calls (`pass_env`).
- `GITHUB_TOKEN` — repo-scoped token, declared under `secrets:`. Routed into the GitHub MCP server's env via templating; redacted from every event and tape.

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
export TEST_PATH=tests/integration/test_payments.py::test_refund_idempotent
export CI_LOG_URL=https://ci.example.com/builds/12345/log
export GITHUB_REPO=acme/widgets
echo 'GITHUB_TOKEN=ghp_...' > .env.secrets
cargo run -p orno-cli -- run examples/flaky-test-triage/pipeline.yaml \
  --secrets-file .env.secrets
```

## Notes

- The agent's mutating reach is small by design: it may write to `reports/` and add log lines to the test under triage. It cannot delete files, push to remotes, or call domains outside the allowlist.
- The MCP filesystem server is mounted at `/workspace`; adjust the `command:` to match your local layout.
