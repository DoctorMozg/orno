# Orno YAML Spec (v0.1.0 target shape)

This document specifies the full user-facing YAML shape orno accepts at v0.1.0 launch. The current skeleton implements a subset; new `examples/*.yaml` conform to this target shape and will become executable as the roadmap (`docs/roadmap.md`) phases land.

The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated from `orno_core::pipeline::Pipeline` via `cargo run -p orno-cli -- schema`. This document is the prose counterpart; when the two disagree, the generated schema wins at runtime — but this document is authoritative for the *design*.

## Top-level structure

```yaml
version: 1                     # required; current is 1
vars: { ... }                  # optional; template variables
agents: { ... }                # optional; named agent configurations
mcp_servers: { ... }           # optional; MCP server declarations
nodes: [ ... ]                 # required; the DAG
```

## `version`

Integer. Currently fixed at `1`. Orno rejects unknown versions at load time.

## `vars`

Map of string to JSON value. Available in templates as `{{ vars.<name> }}`. Evaluated once per run. Common idioms:

```yaml
vars:
  target_branch: main
  project: orno
  max_age_days: 7
```

## `agents`

Map of agent name to `AgentConfig`. Each named agent can be referenced by:

- A node: `nodes[*].agent: <name>`.
- Another agent's subagent tool: `handler: { kind: subagent, agent: <name> }`.

### `AgentConfig`

```yaml
my_agent:
  model: gpt-5                 # required
  provider: openai             # required; matches an LlmTransport provider
  system: "You are..."         # optional system prompt
  allowed_tools:               # required; may be empty
    - Bash
    - Read
    - Edit
    - Write
    - WebFetch
    - "mcp.github.*"
  policy:                      # required
    max_iterations: 10
    max_total_tokens: 50000
    max_tool_calls: 20
    max_wall_clock: 10m
    max_subagent_depth: 3
    allow_mutations: false
    allow_network: false
    allowed_domains: []
    blocked_domains: []
    on_parse_error: fail       # fail | retry_once
```

### `allowed_tools` grammar

Each entry matches one of:

- A builtin name: `Bash`, `Read`, `Edit`, `Write`, `WebFetch` (ADR 0008 table).
- A specific MCP tool: `"mcp.<server>.<tool>"` where `<server>` is a key in the top-level `mcp_servers:` map and `<tool>` is a tool advertised by that server.
- An MCP server wildcard: `"mcp.<server>.*"` — every tool the server advertises.
- A subagent reference: `"subagent.<agent-name>"` where `<agent-name>` is a key in `agents:`. The tool takes `{ prompt: string }`; the parent's emitted prompt becomes the child's `initial_prompt` for a fresh agent run (ADR 0006). Requires `max_subagent_depth > 0`.

Wildcards on builtins and on subagents are disallowed. Listing a non-existent MCP server, MCP tool, or agent fails at `orno validate`.

#### Wire-form tool names

YAML uses dots as readable separators in `mcp.<server>.<tool>` and `subagent.<agent-name>`. Provider function-calling schemas usually disallow dots in tool names; orno rewrites dots to underscores when building the LLM request (`mcp.github.search_issues` → `mcp_github_search_issues`; `subagent.security_lens` → `subagent_security_lens`). Users write dots in YAML; the wire sees underscores. Validation and event-log messages use the dotted YAML form.

### `policy` semantics

See ADR 0005 for full definitions. Brief reference:

- `max_iterations` — agent-loop cap. Overrun → `IterationLimitExceeded` → terminate node.
- `max_total_tokens` — sum across all LLM calls in this agent's loop (not including subagent tokens; subagents have their own caps, bounded from above by the parent's remaining budget).
- `max_tool_calls` — counts every attempted tool call including blocked and subagent calls.
- `max_wall_clock` — duration string (`30s`, `5m`, `1h`). Clock starts at `AgentStarted`.
- `max_subagent_depth` — 0 disables subagents entirely.
- `allow_mutations` / `allow_network` — gate tool calls by declared effect class (ADR 0008).
- `allowed_domains` / `blocked_domains` — domain name list for `WebFetch` and network-capable MCP tools. Blocklist wins on overlap. Subdomain matching: `"api.github.com"` matches exactly; `".github.com"` matches any subdomain; `"github.com"` matches both the bare host and any subdomain.
- `on_parse_error` — what to do when the model returns malformed JSON for a tool call's arguments. `fail` terminates; `retry_once` feeds the parse error back as a tool-result message and loops once more.

Child-agent policy rules (ADR 0006): a child cannot be **less** strict than its parent on `allow_mutations` or `allow_network`. A read-only parent cannot delegate to a mutating child. Enforced at pipeline load.

## `mcp_servers`

Map of server name to `McpServerConfig`. Each server is spawned at run start and shut down at run end (ADR 0007). Naming convention: lowercase with underscores.

```yaml
mcp_servers:
  github:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "{{ env.GITHUB_TOKEN }}"

  filesystem:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-filesystem", "/workspace"]

  remote_scanner:
    transport: http
    url: "https://scanner.internal/mcp"
    auth:
      kind: bearer
      token: "{{ secrets.SCANNER_TOKEN }}"
```

### Stdio transport

- `transport: stdio` — required discriminator.
- `command: [string]` — argv. **Not** passed through a shell.
- `env: { string: string }` — environment additions.

### HTTP transport

- `transport: http` — required discriminator.
- `url: string` — server endpoint.
- `auth: AuthConfig` — optional. `kind: bearer | basic | none`.
- `headers: { string: string }` — optional extra headers.

## `nodes`

Array of `Node` entries. Each node:

- `id: string` — required; unique within the pipeline.
- `kind: agent | shell | external` — required; determines which fields follow.
- `needs: [string]` — optional; IDs of nodes this node depends on. Drives DAG scheduling.

(`llm` was collapsed into `agent` per ADR 0009. `external` is reserved for post-v0.1 subprocess plugins — ADR 0004 — and is rejected by `orno validate` in v0.1.0.)

### `kind: agent`

Agents run the loop from ADR 0005. A node references an agent defined in the top-level `agents:` block.

```yaml
- id: review
  kind: agent
  agent: reviewer              # references agents.reviewer
  initial_prompt: "Review PR #{{ env.PR_NUMBER }}."
  needs: [fetch]               # optional
```

Required fields: `id`, `kind: agent`, `agent`, `initial_prompt`.
Optional: `needs`.

**Inline agent config** at the node level (defining `model`, `provider`, `policy`, etc. directly on the node) is a v0.2.0+ convenience and is not in the v0.1.0 schema. Every agent configuration lives under `agents.*` so the agent shape is reviewable in one block.

### `kind: shell`

Non-agentic subprocess invocation. Not subject to agent policy.

```yaml
- id: fetch_diff
  kind: shell
  command: "git"
  args: ["diff", "--stat", "HEAD~1..HEAD"]
```

For subprocess invocations **inside** an agent loop, use the `Bash` tool (which *is* policy-gated). `kind: shell` exists for deterministic pipeline steps that don't need a model.

### `kind: external`

Stub. Not implemented in v0.1.0. Reserved for ADR 0004 subprocess plugins; validation rejects `kind: external` at load.

## Templates

MiniJinja (auto-escape disabled) renders the following strings:

- `agents.*.system`
- `agents.*.allowed_tools[*]` (for MCP tool names with `{{ vars.* }}` interpolation)
- `nodes[*].initial_prompt`
- `nodes[*].command` / `args` (shell nodes)
- `mcp_servers.*.command` / `url` / `env.*` / `auth.token`

Template context:

- `vars.<name>` — values from the top-level `vars:` block.
- `env.<NAME>` — environment variables (must be listed in a top-level `env_passthrough:` allowlist; undeclared env → template error; allowlist syntax finalized in Phase 5).
- `secrets.<name>` — secrets loaded from a side-file (v0.1.0 treats these identically to env vars).
- `nodes.<id>.output` — output of a completed upstream node. Available in nodes whose `needs:` includes `<id>`.

## Effect-class reference

| Tool       | Effect class            | Requires                                  |
| ---------- | ----------------------- | ----------------------------------------- |
| `Read`     | local_read              | —                                         |
| `Edit`     | local_write             | `allow_mutations: true`                   |
| `Write`    | local_write             | `allow_mutations: true`                   |
| `Bash`     | shell (mut + net)       | both `allow_mutations` and `allow_network` |
| `WebFetch` | network_read            | `allow_network: true` + domain rules      |
| `mcp.*`    | declared by the server  | matches the advertised effect             |

Blocking behavior:

- Violation of a precondition on a builtin → emit `MutatingCallBlocked` / `NetworkBlocked` / `DomainBlocked` and feed the failure back to the model as a tool-call error (per ADR 0005 dimension 3).
- Calling a tool not in `allowed_tools` → emit `UnknownToolCalled` and **terminate the node** (strict; no retry).

## Events emitted per agent node

See ADR 0003 for the event log structure. Per-agent events include:

- `AgentStarted { node_id, agent_name, depth }`
- `LlmRequestStarted { request_id, iteration }`
- `LlmResponseReceived { request_id, usage, finish_reason }`
- `ToolCallStarted { tool_call_id, name, args }`
- `ToolCallCompleted { tool_call_id, output_size, duration }`
- `ToolCallFailed { tool_call_id, error }`
- `SubagentStarted { parent_id, child_agent, depth }`
- `SubagentCompleted { child_agent, tokens_used, iterations }`
- `SubagentFailed { child_agent, error }`
- `SubagentDepthExceeded { limit }`
- `IterationLimitExceeded { iteration, limit }`
- `BudgetExceeded { kind, used, limit }`
- `UnknownToolCalled { name }`
- `MutatingCallBlocked { tool }`
- `NetworkBlocked { tool, url }`
- `DomainBlocked { url }`
- `McpServerStarting { server }` / `McpServerHandshaked` / `McpToolCallSent` / `McpToolCallCompleted` / `McpServerExited` / `McpServerCrashed`
- `AgentCompleted { node_id, iterations, total_tokens }`

This list is not exhaustive; `Event` is `#[non_exhaustive]`. Replay consumers must tolerate unknown variants.

## Minimal pipeline

```yaml
version: 1

agents:
  greeter:
    model: gpt-5
    provider: openai
    system: "You are friendly."
    allowed_tools: []
    policy:
      max_iterations: 1
      max_total_tokens: 1000
      max_tool_calls: 0
      max_wall_clock: 30s
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      on_parse_error: fail

nodes:
  - id: greet
    kind: agent
    agent: greeter
    initial_prompt: "Say hello in one sentence."
```

See `examples/pr-review.yaml`, `examples/flaky-test-triage.yaml`, and `examples/release-notes.yaml` for functionality-heavy samples.

## What v0.1.0 does not ship

- `WebSearch` tool (ADR 0008 deferral).
- Generic HTTP tool handler (use MCP instead).
- User-authored tool JSON Schemas (Architecture A from `docs/chat.md` is not a v0.1.0 feature).
- `kind: external` node execution (ADR 0004).
- Inline agent config at the node level.
- Streaming LLM responses.
- `EventSink` impls beyond `InMemorySink` (SQLite is planned, not shipped).
- Auto-inferred `needs` from template references — specify both explicitly.
