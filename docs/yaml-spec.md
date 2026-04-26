# Pipeline YAML

This document specifies the user-facing YAML shape orno accepts. The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated from the in-tree types via `cargo run -p orno-cli -- schema`. When the two disagree, the generated schema wins at runtime.

## Top-level structure

```yaml
version: 1                     # required; current is 1
vars: { ... }                  # optional; template variables
pass_env: [ ... ]              # optional; names to pull from process env into env.*
secrets: [ ... ]               # optional; names to classify as secrets.*
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

## Environment and secrets

Pipelines reference two external-value namespaces from templates — `env.*` for visible inputs and `secrets.*` for redacted credentials.

### `env.*` — pipeline inputs, opt-in

Sources, highest precedence wins:

1. `-e KEY=VAL` CLI flag (repeatable).
2. `--env-file <path>` (repeatable; later files shadow earlier ones; dotenv format).
3. `pass_env: [NAMES]` top-level YAML block — pulls the named keys from the process env at run start.

```yaml
pass_env:
  - PR_NUMBER
  - CI_BUILD_ID
```

The process environment does **not** auto-populate `env.*`. A name not listed via one of the three sources above is a hard template-render error (`TemplateError::UnknownVariable`), never a silent empty string. This keeps runs reproducible across machines: nothing flows into template context without an explicit declaration.

Values in `env.*` are visible:

- Expand into prompts, shell args, `initial_prompt`s, and the `command` / `args` / `env` / `url` fields on MCP server configs.
- Appear verbatim in emitted events.
- Logged at any tracing level without redaction.

### `secrets.*` — credentials, ambient + file override

Sources, highest precedence wins:

1. **Process env** for two populations:
   - **Provider-known names** — auto-pulled when a pipeline references the provider (`OPENROUTER_API_KEY` for `provider: openrouter`, `ANTHROPIC_API_KEY` for `provider: anthropic`, etc.). No YAML declaration required.
   - **User-declared names** — `secrets: [NAMES]` at the top level adds names to the set. Typical use is MCP server `env:` blocks that pass a token through to the server subprocess.
2. **`--secrets-file <path>`** (repeatable; later files shadow earlier ones; dotenv format).

```yaml
secrets:
  - GITHUB_TOKEN
  - SLACK_BOT_TOKEN
```

There is no CLI flag for individual secret values — `argv` leaks into shell history, `ps aux`, and some exec-tracing facilities. Use `--secrets-file` for ad-hoc overrides.

Classification follows the **name**, not the source. If someone writes `OPENROUTER_API_KEY=sk-...` into `.env.inputs`, orno routes that binding into `secrets.*` anyway — a name's sensitivity cannot be downgraded by choice of source file.

Values in `secrets.*` are protected:

- Expand into templated strings only when the template explicitly references `{{ secrets.FOO }}`.
- Value-redacted to `***` in every event body, tracing line, and replay tape where they appear.
- The internal secrets handle is not `Debug`-printable and does not implement `Serialize`, so accidental inclusion in a diagnostic artifact fails at compile time.

### CLI surface

```
orno run pipeline.yaml \
  -e PR_NUMBER=482 \
  -e RUN_LABEL=nightly \
  --env-file .env.inputs \
  --secrets-file .env.secrets
```

All three flags are repeatable. Missing files are a hard error — no silent fallback to default paths.

## `agents`

Map of agent name to `AgentConfig`. Each named agent can be referenced by:

- A node: `nodes[*].agent: <name>`.
- Another agent's subagent tool: `subagent.<agent-name>` in `allowed_tools`.

### Default provider

`openrouter` is the default provider. OpenRouter exposes every upstream vendor behind a single OpenAI-compatible endpoint, so a single `OPENROUTER_API_KEY` unlocks OpenAI, Anthropic, Google, and open-weight models without per-vendor plumbing. Agents select the upstream by giving the OpenRouter route as `model:` (e.g. `openai/gpt-5`, `anthropic/claude-sonnet-4.5`, `google/gemini-2.5-pro`). Direct-vendor `provider: openai` / `provider: anthropic` remain valid identifiers but require the matching vendor key.

### `AgentConfig`

```yaml
my_agent:
  model: openai/gpt-5          # required; slash-prefixed route for the default (OpenRouter) provider
  provider: openrouter         # required; matches an LlmTransport provider. Default: openrouter
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
    max_subagent_depth: 3
    allow_mutations: false
    allow_network: false
    allow_context_writes: false  # opt-in for SetState
    allowed_domains: []
    blocked_domains: []
    on_parse_error: fail       # fail | retry_once
  # Wall-clock deadline is a node-level attribute (`timeout: 10m`),
  # not an agent-policy field.
```

### `allowed_tools` grammar

Each entry matches one of:

- A builtin name: `Bash`, `Read`, `Edit`, `Write`, `WebFetch`, `SetState`.
- A specific MCP tool: `"mcp.<server>.<tool>"` where `<server>` is a key in the top-level `mcp_servers:` map and `<tool>` is a tool advertised by that server.
- An MCP server wildcard: `"mcp.<server>.*"` — every tool the server advertises.
- A subagent reference: `"subagent.<agent-name>"` where `<agent-name>` is a key in `agents:`. The tool takes `{ prompt: string }`; the parent's emitted prompt becomes the child's `initial_prompt` for a fresh agent run. Requires `max_subagent_depth > 0`.

Wildcards on builtins and on subagents are disallowed. Listing a non-existent MCP server, MCP tool, or agent fails at `orno validate`.

#### Wire-form tool names

YAML uses dots as readable separators in `mcp.<server>.<tool>` and `subagent.<agent-name>`. Provider function-calling schemas usually disallow dots in tool names; orno rewrites dots to underscores when building the LLM request (`mcp.github.search_issues` → `mcp_github_search_issues`; `subagent.security_lens` → `subagent_security_lens`). Users write dots in YAML; the wire sees underscores. Validation and event-log messages use the dotted YAML form.

### `policy` semantics

- `max_iterations` — agent-loop cap. Overrun → `IterationLimitExceeded` → terminate node.
- `max_total_tokens` — sum across all LLM calls in this agent's loop (not including subagent tokens; subagents have their own caps, bounded from above by the parent's remaining budget).
- `max_tool_calls` — counts every attempted tool call including blocked and subagent calls.
- `max_subagent_depth` — 0 disables subagents entirely.
- `allow_mutations` / `allow_network` — gate tool calls by declared effect class.
- `allow_context_writes` — gate `SetState` and other context-self tools. Off by default; an agent that never writes scoped state has no reason to opt in. Refusal feeds back to the model as a denial string; the loop continues.
- `allowed_domains` / `blocked_domains` — domain name list for `WebFetch` and network-capable MCP tools. Blocklist wins on overlap. Subdomain matching: `"api.github.com"` matches exactly; `".github.com"` matches any subdomain; `"github.com"` matches both the bare host and any subdomain.
- `on_parse_error` — what to do when the model returns malformed JSON for a tool call's arguments. `fail` terminates; `retry_once` feeds the parse error back as a tool-result message and loops once more.

Child-agent policy rules: a child cannot be **less** strict than its parent on `allow_mutations` or `allow_network`. A read-only parent cannot delegate to a mutating child. Enforced at pipeline load.

## `mcp_servers`

Map of server name to `McpServerConfig`. Each server is spawned at run start and shut down at run end. Naming convention: lowercase with underscores.

```yaml
mcp_servers:
  github:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "{{ secrets.GITHUB_TOKEN }}"

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
- `auth: AuthConfig` — optional. `kind: bearer | basic | none`. Caveat: only `bearer` and `none` connect; `basic` is parsed and validated but returns `UnsupportedTransport` at run start so misconfiguration surfaces loudly (use `kind: bearer` or supply an explicit `Authorization` header in `headers:` instead).
- `headers: { string: string }` — optional extra headers. Forwarded as request headers on every MCP call. Header names that violate RFC 7230 (e.g. spaces, control characters) fail at run start with `HandshakeFailed`.

## `nodes`

Array of `Node` entries. Each node:

- `id: string` — required; unique within the pipeline.
- `kind: agent | shell` — required; determines which fields follow.
- `needs: [string]` — optional; IDs of nodes this node depends on. Drives DAG scheduling.

### `kind: agent`

Agents run the strict loop. A node references an agent defined in the top-level `agents:` block.

```yaml
- id: review
  kind: agent
  agent: reviewer              # references agents.reviewer
  initial_prompt: "Review PR #{{ env.PR_NUMBER }}."
  needs: [fetch]               # optional
```

Required fields: `id`, `kind: agent`, `agent`, `initial_prompt`. Optional: `needs`.

Inline agent config at the node level (defining `model`, `provider`, `policy`, etc. directly on the node) is not accepted. Every agent configuration lives under `agents.*` so the agent shape is reviewable in one block.

### `kind: shell`

Non-agentic subprocess invocation. Not subject to agent policy.

```yaml
- id: fetch_diff
  kind: shell
  command: "git"
  args: ["diff", "--stat", "HEAD~1..HEAD"]
```

Fields:

- `command: string` — program name (argv[0]). Not passed through a shell.
- `args: [string]` — argv entries. Each entry is rendered through the template engine.
- `stdin: string` (optional) — content piped into the child's stdin. Rendered through the same template context as `command` and `args`. When omitted, stdin is closed (`Stdio::null()`) — a child that reads stdin sees EOF immediately. Use `stdin:` to hand untrusted or multi-line content to a subprocess without the shell-escaping hazards of embedding it in `args`.

```yaml
- id: save_plan
  kind: shell
  needs: [plan]
  command: sh
  args: ["-c", "mkdir -p {{ vars.output_dir }} && cat > {{ vars.output_path }}"]
  stdin: "{{ nodes.plan.output }}"
```

For subprocess invocations **inside** an agent loop, use the `Bash` tool (which *is* policy-gated). `kind: shell` exists for deterministic pipeline steps that don't need a model.

## Templates

MiniJinja (auto-escape disabled) renders the following strings:

- `agents.*.system`
- `agents.*.allowed_tools[*]` (for MCP tool names with `{{ vars.* }}` interpolation)
- `nodes[*].initial_prompt`
- `nodes[*].command` / `args` / `stdin` (shell nodes)
- `mcp_servers.*.command` / `url` / `env.*` / `auth.token`

Template context:

- `vars.<name>` — values from the top-level `vars:` block.
- `env.<NAME>` — pipeline inputs. See [Environment and secrets](#environment-and-secrets); undeclared names are a template-render error, never a silent empty string.
- `secrets.<NAME>` — redacted credentials. See [Environment and secrets](#environment-and-secrets); values are replaced with `***` in every event and trace.
- `nodes.<id>.output` — final assistant message from a completed upstream `kind: agent` node. Available in nodes whose `needs:` includes `<id>`.
- `nodes.<id>.state.<key>` — scoped key written by the upstream agent's `SetState` tool. Only present when the agent made at least one `SetState` call; referencing an absent `state.<key>` is a template-render error. Keys are single-level identifiers (`[A-Za-z_][A-Za-z0-9_]*`), not dotted paths.
- `nodes.<id>.stdout` / `.stderr` / `.exit_code` — per-channel results from a completed upstream `kind: shell` node. Shell nodes do **not** expose `.output`; referencing it is a template-render error.
- `nodes.<id>.status` — terminal `NodeStatus` for any completed upstream node (`completed | failed | timed_out | skipped`).

## Effect-class reference

| Tool       | Effect class            | Requires                                  |
| ---------- | ----------------------- | ----------------------------------------- |
| `Read`     | local_read              | —                                         |
| `Edit`     | local_write             | `allow_mutations: true`                   |
| `Write`    | local_write             | `allow_mutations: true`                   |
| `Bash`     | shell (mut + net)       | both `allow_mutations` and `allow_network` |
| `WebFetch` | network_read            | `allow_network: true` + domain rules      |
| `SetState` | context_self            | `allow_context_writes: true`              |
| `mcp.*`    | declared by the server  | matches the advertised effect             |

The `SetState` builtin writes a single top-level key under `nodes.<self>.state.*`. Arguments: `{ key: string, value: <json> }`. `key` matches `^[A-Za-z_][A-Za-z0-9_]*$` — single-level only, no dotted paths. A second call with the same key overwrites the prior value wholesale; the whole state tree is size-capped at the engine's `max_output_bytes`. Secret-valued leaves are redacted before storage. State writes affect only the current node; downstream nodes that `needs:` this one read through `nodes.<id>.state.<key>`.

Blocking behavior:

- Violation of a precondition on a builtin → emit `MutatingCallBlocked` / `NetworkBlocked` / `DomainBlocked` and feed the failure back to the model as a tool-call error (the loop continues with the denial in context, so the model can recover or ask the operator).
- Calling a tool not in `allowed_tools` → emit `UnknownToolCalled` and **terminate the node** (strict; no retry).

## Events emitted per agent node

Per-agent events include:

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
    model: openai/gpt-5
    provider: openrouter
    system: "You are friendly."
    allowed_tools: []
    policy:
      max_iterations: 1
      max_total_tokens: 1000
      max_tool_calls: 0
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

See `examples/pr-review/`, `examples/flaky-test-triage/`, and `examples/release-notes/` for functionality-heavy samples.
