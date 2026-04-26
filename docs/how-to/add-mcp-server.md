# How to add an MCP server

orno can spawn local MCP servers over stdio or connect to remote MCP servers over streamable-HTTP. Both are declared in the `mcp_servers:` block; tools become callable as `mcp.<server>.<tool>` from any agent that lists them in `allowed_tools`.

## When to use stdio vs. HTTP

| Transport | Use when                                                                  | Trade-off                                                  |
| --------- | ------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `stdio`   | The MCP server is a local executable (`npx`, a binary on `$PATH`).        | orno spawns and manages the process; one server per run.    |
| `http`    | The MCP server is hosted (a SaaS product, internal service, public demo). | orno makes HTTP requests; supports auth, custom headers.    |

The remote/local distinction is the dominant one. If the server is on the same machine as orno, prefer `stdio`. If it's across a network boundary, use `http`.

## Recipe 1 — stdio MCP server

```yaml
mcp_servers:
  filesystem:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-filesystem", "/workspace"]

agents:
  reader:
    model: openai/gpt-5
    provider: openrouter
    allowed_tools:
      - "mcp.filesystem.read_file"
      - "mcp.filesystem.list_directory"
    policy:
      max_iterations: 10
      max_total_tokens: 50000
      max_tool_calls: 20
      max_subagent_depth: 0
      allow_mutations: true       # required for any MCP call
      allow_network: true         # required for any MCP call
      on_parse_error: fail
```

What happens at run start:

1. orno spawns `npx @modelcontextprotocol/server-filesystem /workspace` as a child process with stdio piped.
2. orno performs the MCP handshake (`initialize`) and asks for `tools/list`.
3. orno expands `allowed_tools` against the advertised list and registers each handler.
4. The agent can now call `mcp.filesystem.read_file` etc. as if it were a builtin.
5. At run end, orno sends shutdown and reaps the child process.

Two requirements you cannot skip:

- **`allow_mutations: true` AND `allow_network: true`** — every MCP tool is classified as `MutationsAndNetwork` regardless of what the server advertises. orno cannot inspect a remote server's per-tool semantics, so it conservatively assumes worst case. The operator must explicitly grant both before any MCP call lands.
- **The `command:` array must be a real executable** — `npx`, a binary on `$PATH`, or an absolute path. orno does not interpret the string as a shell command; it spawns directly.

## Recipe 2 — streamable-HTTP MCP server

```yaml
mcp_servers:
  gitmcp:
    transport: http
    url: "https://gitmcp.io/modelcontextprotocol/python-sdk"
    auth:
      kind: none
    headers: {}

agents:
  doc_reader:
    model: anthropic/claude-haiku-4.5
    provider: openrouter
    allowed_tools:
      - "mcp.gitmcp.*"            # wildcard expansion
    policy:
      max_iterations: 4
      max_total_tokens: 8000
      max_tool_calls: 3
      max_subagent_depth: 0
      allow_mutations: true
      allow_network: true
      allowed_domains: ["gitmcp.io"]
      on_parse_error: fail
```

The wildcard `mcp.gitmcp.*` expands at run start against the server's advertised `tools/list`. Each advertised tool name becomes an entry like `mcp.gitmcp.fetch_modelcontextprotocol_python_sdk_documentation`. Useful when the server exposes many similar tools, or when tool names embed identifiers (like the GitMCP repo slug).

## Recipe 3 — HTTP server with bearer auth

When the server requires a bearer token, pull the token from a secret:

```yaml
secrets:
  - SCANNER_TOKEN

mcp_servers:
  scanner:
    transport: http
    url: "https://api.example.com/mcp"
    auth:
      kind: bearer
      token: "{{ secrets.SCANNER_TOKEN }}"
```

Then provide the secret at run time:

```bash
echo 'SCANNER_TOKEN=tok_...' > .env.secrets
orno run pipeline.yaml --secrets-file .env.secrets
```

The token is rendered into the `Authorization: Bearer <token>` header of MCP requests, redacted from every event body, and redacted from any recorded bundle. A replay does **not** re-fetch the token; the recorded bundle has it stripped.

## Recipe 4 — HTTP server with custom headers

Some MCP servers expect API keys in custom headers, not `Authorization`:

```yaml
secrets:
  - GITHUB_TOKEN

mcp_servers:
  github_mcp:
    transport: http
    url: "https://api.githubcopilot.com/mcp"
    auth:
      kind: none
    headers:
      X-GitHub-Token: "{{ secrets.GITHUB_TOKEN }}"
      X-Custom-Source: "orno-ci"
```

`headers:` values are template-rendered and pass through the redactor. Static values (no template) are preserved verbatim.

## Tool naming and wildcards

Tool names live in two namespaces:

- `tools/list` namespace (what the MCP server advertises) — uses dots, hyphens, and any character the server picks.
- `allowed_tools` namespace (what orno's policy gate matches) — uses underscores only, dotted only at the `mcp.<server>.<tool>` boundary.

orno translates the server's tool name to the policy form by replacing `.` and `-` with `_`. So a server-advertised tool `read.file-v2` is callable as `mcp.<server>.read_file_v2`.

For wildcards:

- `mcp.<server>.*` — every tool the server advertises, expanded at run start.
- `mcp.<server>.read_*` — every tool whose translated name starts with `read_`.

Wildcards are expanded once, at run start. A server that adds a tool mid-run will not have the new tool exposed; the next run will pick it up.

## Sandboxing and trust

You cannot trust a remote MCP server's claim that a tool is "read-only" or "non-mutating." orno classifies every MCP tool as `MutationsAndNetwork` as a defensive default. If you trust a server and want to deny all MCP calls except a narrow allowlist, do it at the `allowed_tools` level — list the specific tools, not a wildcard.

For high-trust scenarios (a server you control), the bearer-auth + domain-filter combination is the strictest:

```yaml
agents:
  trusted_caller:
    allowed_tools:
      - "mcp.internal.specific_tool_a"
      - "mcp.internal.specific_tool_b"
    policy:
      allow_mutations: true
      allow_network: true
      allowed_domains: ["internal-mcp.company.com"]
      blocked_domains: []
```

A model that emits a tool call to a different MCP tool gets `UnknownToolCalled` and the loop terminates. A model that somehow makes an unauthorized HTTP request to a different host (via a separate `WebFetch` call, say) gets `DomainBlocked`.

## Failure modes

| Failure                          | What it looks like in the event stream                                  |
| -------------------------------- | ----------------------------------------------------------------------- |
| Server fails to spawn (stdio)    | `mcp_server_failed` envelope with the spawn error.                       |
| Handshake fails                  | `mcp_server_failed` with the handshake error; pipeline terminates.       |
| Tool call fails                  | `tool_failed` for the specific call; loop continues.                     |
| Network error mid-run (http)     | `tool_failed` with the network error; loop continues.                    |
| Wildcard expansion finds zero    | `mcp_server_started` with `tools_advertised: 0`; agent has no MCP tools. |

## See also

- [Pipeline YAML › `mcp_servers`](../reference/pipeline-yaml.md#mcp_servers) — every field of the `mcp_servers` block.
- [Tools › Effect classes](../reference/tools.md#effect-classes) — why MCP tools are `MutationsAndNetwork`.
- [Security › MCP server trust](../security.md#mcp-server-trust) — threat model around external MCP servers.
- [`examples/mcp-http-demo`](../../examples/mcp-http-demo/) — runnable HTTP MCP example with record/replay.
