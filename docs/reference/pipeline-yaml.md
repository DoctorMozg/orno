# Pipeline YAML

The full grammar of the YAML files orno accepts. The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated from the in-tree types via `cargo run -p orno-cli -- schema`. When the two disagree, the generated schema is the source of truth.

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

Integer. Currently `1`. Orno rejects unknown versions at load time.

## `vars`

Map of string to JSON value. Available in templates as `{{ vars.<name> }}`. Evaluated once per run.

```yaml
vars:
  target_branch: main
  project: orno
  max_age_days: 7
```

### Var rendering against env and secrets

String values inside `vars` are themselves templated — they are rendered once at engine
entry against the `{ env, secrets }` namespace before any node is dispatched, so a value
of the form `"{{ env.RELEASE_TAG }}"` resolves to the env binding rather than carrying
the literal template source through to downstream nodes:

```yaml
vars:
  tag: "{{ env.RELEASE_TAG }}"        # → resolves to env.RELEASE_TAG at run start
  range: "{{ env.PREV }}..{{ env.CUR }}"
```

Numbers, booleans, and nulls pass through unchanged. Arrays and objects recurse, so
`vars.commits: ["{{ env.SHA }}"]` and `vars.opts: { tag: "{{ env.TAG }}" }` both expand
element-by-element.

Cross-var references (`vars.b: "{{ vars.a }}"`) are intentionally **not** supported in
v0.1.x — the rendering context exposes only `env` and `secrets`. A `{{ vars.x }}` lookup
inside another `vars` entry will fail with `PipelineError::Template { name: "vars.b", … }`.

## Environment and secrets

Pipelines reference two external-value namespaces from templates — `env.*` for visible inputs and `secrets.*` for redacted credentials. See [env-vars.md](env-vars.md) for the runtime-boundary surface.

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

The process environment does **not** auto-populate `env.*`. A name not listed via one of the three sources above is a hard template-render error (`PipelineError::Template`), never a silent empty string. This keeps runs reproducible across machines.

Values in `env.*` are visible:

- Expand into prompts, shell args and shell `env` values, `initial_prompt`s, `policy.roots`, and every string in an `mcp_servers.*` entry.
- Appear verbatim in emitted events.
- Logged at any tracing level without redaction.

### `secrets.*` — credentials, ambient + file override

Sources, highest precedence wins:

1. **Process env** for the names listed in `secrets: [NAMES]` at the top level. Typical use is MCP server `env:` blocks that pass a token through to the server subprocess.
2. **`--secrets-file <path>`** (repeatable; later files shadow earlier ones; dotenv format). Every key in the file must be declared under `secrets:`; an undeclared key is an error.

Provider API keys (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) are separate: the LLM transport reads them from the process env, and they are redacted automatically for the providers a pipeline uses. They are in `secrets.*` only when declared here. See [env-vars.md](env-vars.md#provider-credentials).

```yaml
secrets:
  - GITHUB_TOKEN
  - SLACK_BOT_TOKEN
```

There is no CLI flag for individual secret values — `argv` leaks into shell history, `ps aux`, and some exec-tracing facilities. Use `--secrets-file` for ad-hoc overrides.

Classification follows the **name**, not the source. If a name is declared under `secrets:` and someone writes it into `--env-file` input, orno routes that binding into `secrets.*` — a name's sensitivity cannot be downgraded by choice of source file. A name listed in both `pass_env` and `secrets` fails validation.

Values in `secrets.*` are protected:

- Expand into templated strings only when the template explicitly references `{{ secrets.FOO }}`.
- Value-redacted to `***` in every event body, tracing line, and replay tape where they appear. Literal MCP credentials in a `--record-bundle` pipeline copy are replaced with `<REDACTED>` instead.
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

### Providers

`provider:` is required and must be one of `openai`, `anthropic`, `ollama`, `openrouter`. OpenRouter exposes every upstream vendor behind one OpenAI-compatible endpoint, so a single `OPENROUTER_API_KEY` reaches OpenAI, Anthropic, Google, and open-weight models; the `model:` is then the OpenRouter route (e.g. `openai/gpt-5`, `anthropic/claude-sonnet-4.5`). Direct `openai` / `anthropic` need the matching vendor key; `ollama` needs none and reads `OLLAMA_HOST`. See [env-vars.md](env-vars.md#provider-credentials).

### `AgentConfig`

```yaml
my_agent:
  model: openai/gpt-5          # required; slash-prefixed route for the default (OpenRouter) provider
  provider: openrouter         # required; openai | anthropic | ollama | openrouter
  system: "You are..."         # optional system prompt
  allowed_tools:               # required; may be empty
    - Bash
    - Read
    - Edit
    - Write
    - WebFetch
    - SetState
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
    roots: ["{{ env.WORKSPACE }}"]  # required with Read/Write/Edit; see below
    max_tool_output_bytes: 262144   # optional; default 256 KiB
    max_message_history_bytes: 4194304  # optional; default 4 MiB, null disables
    unconfined_shell: false    # see below
  # Wall-clock deadline is a node-level attribute (`timeout: 600`),
  # not an agent-policy field.
```

### `allowed_tools` grammar

Each entry matches one of:

- A builtin name: `Bash`, `Read`, `Edit`, `Write`, `WebFetch`, `SetState`.
- A specific MCP tool: `"mcp.<server>.<tool>"` where `<server>` is a key in the top-level `mcp_servers:` map and `<tool>` is a tool advertised by that server.
- An MCP server wildcard: `"mcp.<server>.*"` — every tool the server advertises at handshake.
- A subagent reference: `"subagent.<agent-name>"` where `<agent-name>` is a key in `agents:`. The tool takes `{ prompt: string }`; the parent's emitted prompt becomes the child's `initial_prompt` for a fresh agent run. Requires `max_subagent_depth > 0`.

Wildcards on builtins and on subagents are disallowed. Listing a non-existent MCP server, MCP tool, or agent fails at `orno validate`.

#### Wire-form tool names

YAML uses dots as readable separators in `mcp.<server>.<tool>` and `subagent.<agent-name>`. Provider function-calling schemas usually disallow dots in tool names; orno rewrites dots to underscores when building the LLM request (`mcp.github.search_issues` → `mcp_github_search_issues`; `subagent.security_lens` → `subagent_security_lens`). Users write dots in YAML; the wire sees underscores. Validation and event-log messages use the dotted YAML form.

### `policy` semantics

| Field                    | Type                       | Meaning                                                                                                                                                                                                                                            |
| ------------------------ | -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_iterations`         | u32                        | Agent-loop cap. Overrun → `IterationLimitExceeded` → terminate node.                                                                                                                                                                                 |
| `max_total_tokens`       | u64                        | Sum across all LLM calls in the whole subagent tree: a grandchild's spend counts against the root, and each request is capped to what is left on the tightest limit up the chain. Iterations stay per loop.                                       |
| `max_tool_calls`         | u32                        | Counts every attempted tool call including blocked and subagent calls. A child's tool calls also count against each ancestor.                                                                                                                        |
| `max_subagent_depth`     | u32                        | `0` disables subagents entirely. A child whose dispatch would exceed the parent's depth is denied; `Event::SubagentDepthExceeded` fires and the parent's loop receives a denial-style result.                                                       |
| `allow_mutations`        | bool                       | Gate `Mutations` and `MutationsAndNetwork` tools.                                                                                                                                                                                                    |
| `allow_network`          | bool                       | Gate `Network` and `MutationsAndNetwork` tools.                                                                                                                                                                                                      |
| `allow_context_writes`   | bool (default `false`)     | Gate `ContextSelf` (the `SetState` builtin). Off by default — an agent that never writes scoped state has no reason to opt in.                                                                                                                       |
| `allowed_domains`        | array of string            | Allowlist for `WebFetch` and network-capable MCP tools. Empty list means no allowlist enforced.                                                                                                                                                      |
| `blocked_domains`        | array of string            | Blocklist. Wins on overlap with `allowed_domains`. Subdomain matching: `"api.github.com"` matches exactly; `".github.com"` matches any subdomain; `"github.com"` matches both the bare host and any subdomain.                                       |
| `roots`                  | array of path              | Directories the `Read`, `Write` and `Edit` builtins are jailed to, and the base for Bash `cwd`. Required (non-empty) when `allowed_tools` lists `Read`, `Write` or `Edit`; the loop fails with `InvalidPolicy` at run start otherwise, and Bash `cwd` without `roots` is denied. Only `roots[0]` is consulted today. `..` components are rejected at validation. Entries are templated. Relative paths resolve against the root, not the process cwd. |
| `max_tool_output_bytes`  | integer (optional)         | Per-call cap on tool output before it enters the conversation. Default 256 KiB. Output over the cap is head-truncated with an ellipsis marker; the loop continues.                                                                                  |
| `max_message_history_bytes` | integer or `null` (default 4 MiB) | Soft cap on the conversation history. After each turn the loop drops the oldest whole turns, never splitting a tool call from its result and always keeping the most recent messages. `null` disables the cap.                          |
| `unconfined_shell`       | bool (default `false`)     | Required when `allowed_tools` has `Bash` and `allowed_domains` or `roots` is set; a shell reaches any host and path, so those limits do not bind it. |
| `on_parse_error`         | `fail \| retry_once`       | What the loop does when the model returns malformed JSON for a tool-call argument. `fail` terminates; `retry_once` feeds the parse error back as a tool-result message and loops once more. A second parse error always terminates.                  |

Child-agent policy rules, enforced at pipeline load: a child cannot be **less** strict than its parent on `allow_mutations`, `allow_network` or `allow_context_writes`; a child's `max_subagent_depth` must be at most the parent's minus one; and when the parent declares `roots`, every child root must sit inside one of them. A subagent's `system` and `roots` templates are rendered with the parent node's context, and a rendered root outside the parent's roots is denied at call time.

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

| Field        | Type                         | Description                                                |
| ------------ | ---------------------------- | ---------------------------------------------------------- |
| `transport`  | `"stdio"` (discriminator)    | Selects the stdio variant.                                 |
| `command`    | array of string              | argv vector. **Not** passed through a shell.               |
| `env`        | map of string to string      | Environment additions for the child process.               |
| `call_timeout_secs` | integer (optional)    | Per-call timeout in seconds. Default 30; `0` is rejected.  |

### HTTP transport

| Field        | Type                              | Description                                                                                                                                                                                                  |
| ------------ | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `transport`  | `"http"` (discriminator)          | Selects the HTTP variant.                                                                                                                                                                                    |
| `url`        | string                            | Server endpoint.                                                                                                                                                                                             |
| `auth`       | `AuthConfig?`                     | `kind: bearer \| basic \| none`. Caveat: only `bearer` and `none` connect today; `basic` is parsed and validated but returns `UnsupportedTransport` at run start. Use `bearer` or supply explicit headers.    |
| `headers`    | map of string to string           | Optional extra headers. Forwarded on every MCP call. Header names violating RFC 7230 fail at run start with `HandshakeFailed`.                                                                               |
| `call_timeout_secs` | integer (optional)         | Per-call timeout in seconds. Default 30; `0` is rejected. A call with no reply in time fails as `McpError::CallFailed`.                                                                                       |

The `url` must be `https://`, or `http://` to a loopback host, and an `https://` URL whose host is a literal IP in a blocked range (loopback, private, link-local, CGNAT, documentation, metadata) is rejected. A templated `url` is checked after rendering, right before the server is spawned. Every string in an `mcp_servers.*` entry (`command`, `env` values, `url`, `headers`, `auth`) is rendered with the `env` and `secrets` namespaces. MCP server names must not share an underscore-prefix relationship (`gh` and `gh_extra`), because the wire-form tool names would be ambiguous.

## `nodes`

Array of `Node` entries. Each node:

- `id: string` — required; unique within the pipeline and matching `[A-Za-z_][A-Za-z0-9_]*`, so `nodes.<id>` is writable in templates.
- `kind: agent | shell` — required; determines which fields follow.
- `needs: [string]` — optional; ids of nodes this node depends on. Drives DAG scheduling.
- `timeout: integer` — optional; per-node wall-clock cap in seconds, greater than zero (`timeout: 0` is rejected). This is the only wall-clock limit; there is none on the agent policy. When elapsed, the node is cancelled and `Event::NodeTimedOut` fires; `node_finished.failure` carries `kind: timed_out`.

### `kind: agent`

Agents run the strict loop. A node references an agent defined in the top-level `agents:` block.

```yaml
- id: review
  kind: agent
  agent: reviewer              # references agents.reviewer
  initial_prompt: "Review PR #{{ env.PR_NUMBER }}."
  needs: [fetch]               # optional
  timeout: 600                 # optional; 10-minute cap
```

Required fields: `id`, `kind: agent`, `agent`, `initial_prompt`. Optional: `needs`, `timeout`.

Inline agent config at the node level (defining `model`, `provider`, `policy`, etc. directly on the node) is not accepted. Every agent configuration lives under `agents.*` so the agent shape is reviewable in one block.

### `kind: shell`

Non-agentic subprocess invocation. Not subject to agent policy.

```yaml
- id: fetch_diff
  kind: shell
  command: "git"
  args: ["diff", "--stat", "HEAD~1..HEAD"]
```

| Field     | Type            | Description                                                                                                                                                                          |
| --------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `command` | string          | Program name (argv[0]). Not passed through a shell.                                                                                                                                  |
| `args`    | array of string | argv entries. Each entry is rendered through the template engine.                                                                                                                    |
| `stdin`   | string?         | Content piped into the child's stdin. Rendered through the same template context as `command` and `args`. When omitted, stdin is closed (the child sees EOF immediately on `read`). |
| `env`     | map of string to string | Environment additions for the child. Values are rendered through the template engine, so `{{ vars.* }}`, `{{ env.* }}` and `{{ secrets.* }}` work. |

The command must not be empty. The child starts with a cleared environment holding only `PATH`, `HOME`, `USER`, `TMPDIR`, `LANG`, `LC_ALL` and `TERM` from the orno process, plus the node's `env:` entries. The child runs in its own process group and the group is SIGKILLed when the command exits, times out, or is cancelled, so a background process it started does not outlive the node. Captured `stdout` and `stderr` are capped per stream by `--max-node-output-bytes` (default 8 MiB).

```yaml
- id: save_plan
  kind: shell
  needs: [plan]
  command: sh
  args: ["-c", "mkdir -p {{ vars.output_dir }} && cat > {{ vars.output_path }}"]
  stdin: "{{ nodes.plan.output }}"
```

For subprocess invocations **inside** an agent loop, use the `Bash` tool (which *is* policy-gated). `kind: shell` exists for deterministic pipeline steps that don't need a model.

> **Policy note.** `kind: shell` runs entirely outside the agent strictness sandbox. It does **not** consult any agent's `allow_mutations`, `allow_network`, `allowed_domains`, `blocked_domains`, or any other tool-effect gate — those policies apply only to tool calls dispatched by an `agent` node's loop. A shell node can read or write any path the orno process can reach, open arbitrary network sockets, and exec any program on `PATH`, subject only to the OS-level permissions of the orno process itself. Treat the `command` and `args` fields as fully privileged: render-time templating is the boundary, and a value reaching them must already be trusted (a literal, a `{{ vars.* }}` from a vetted pipeline, or a `{{ nodes.<id>.* }}` whose upstream output you control). For untrusted-input subprocesses, route them through an `agent` node with `Bash` in `allowed_tools` so the strictness contract applies.

## Templates

MiniJinja (auto-escape disabled) renders the following strings:

- `agents.*.system` (a subagent's is rendered with the parent node's context)
- `agents.*.policy.roots[*]`
- `agents.*.allowed_tools[*]` (for MCP tool names with `{{ vars.* }}` interpolation)
- `nodes[*].initial_prompt`
- `nodes[*].command` / `args` / `stdin` / `env.*` (shell nodes)
- every string in `mcp_servers.*` (`command`, `env.*`, `url`, `headers.*`, `auth.*`), rendered with the `env` and `secrets` namespaces only
- string values inside `vars` (see above)

Validation compiles every template and checks that each `nodes.<id>` read names a transitive `needs:` ancestor of the reading node.

Template context:

| Reference                          | Source                                                                                                                                       |
| ---------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `vars.<name>`                      | Top-level `vars:` block.                                                                                                                     |
| `env.<NAME>`                       | Pipeline inputs. Undeclared names are a template-render error, never a silent empty string.                                                  |
| `secrets.<NAME>`                   | Credentials declared under `secrets:`. Values appear as `***` in events and traces; the rendered string carries the real value into the LLM transport only. |
| `nodes.<id>.output`                | Final assistant message from a completed upstream `kind: agent` node.                                                                        |
| `nodes.<id>.finish_reason`, `nodes.<id>.usage` | The agent's last finish reason and token usage.                                                                                  |
| `nodes.<id>.state.<key>`           | Scoped key written by the upstream agent's `SetState` tool. Present only if the agent called `SetState`; referencing an absent key is a template-render error. |
| `nodes.<id>.stdout`                | stdout from a completed upstream `kind: shell` node.                                                                                         |
| `nodes.<id>.stderr`                | stderr from a completed upstream `kind: shell` node.                                                                                         |
| `nodes.<id>.exit_code`             | Exit code from a completed upstream `kind: shell` node (`null` when killed by a signal).                                                     |
| `nodes.<id>.signal`                | Signal number when the shell node was killed by a signal (Unix), else `null`.                                                                |

There is no `nodes.<id>.status`: a node whose upstream failed is skipped, never rendered.

Shell nodes do not expose `.output`; agent nodes do not expose `.stdout` / `.stderr` / `.exit_code`. Referencing the wrong channel is a template-render error.

## Effect-class reference

| Tool       | Effect class             | Required policy fields                          |
| ---------- | ------------------------ | ----------------------------------------------- |
| `Read`     | `ReadOnly`               | none                                            |
| `Edit`     | `Mutations`              | `allow_mutations: true`                         |
| `Write`    | `Mutations`              | `allow_mutations: true`                         |
| `Bash`     | `MutationsAndNetwork`    | both `allow_mutations` and `allow_network`      |
| `WebFetch` | `Network`                | `allow_network: true` + domain rules            |
| `SetState` | `ContextSelf`            | `allow_context_writes: true`                    |
| `mcp.*`    | `MutationsAndNetwork`    | both `allow_mutations` and `allow_network`      |
| `subagent.<name>` | derived from child policy | matches the child's declared effects          |

See [tools.md](tools.md#effect-classes) for the full effect-class semantics and per-tool argument schemas.

Blocking behavior:

- Violation of a precondition on a builtin (effect class, or a URL outside the domain lists) → emit `Event::ToolDenied` with a reason and feed a `denied: ...` result back to the model. The loop continues with the denial in context.
- Calling a tool not in `allowed_tools` → **terminate the node** with `AgentError::UnknownToolCalled` (strict; no retry). No dedicated event is emitted; the node ends with `node_finished { ok: false }`.

## Validation

`orno validate`, `orno run`, `orno plan` and `orno replay` all run the same validation before any node starts, and report every problem at once. It checks:

- The file parses against the schema. **Unknown keys are rejected** at every level (top level, nodes, agents, `policy`, `mcp_servers`), with the key and node id in the error.
- `version` equals the version this build supports.
- Node ids match `[A-Za-z_][A-Za-z0-9_]*`; `timeout: 0` and an empty shell `command` are rejected.
- Every `nodes[*].agent` references an entry in `agents:`.
- Every `subagent.<name>` in `allowed_tools` references an entry in `agents:`; every `mcp.<server>.<tool>` and `mcp.<server>.*` references a key in `mcp_servers:`; every other entry is a known builtin.
- Budgets: `max_iterations > 0`, `max_total_tokens > 0`, and `max_tool_calls > 0` when `allowed_tools` is non-empty.
- Effect gates: each builtin's required `allow_*` flag is on (see [effect classes](#effect-class-reference)).
- `Bash` together with a non-empty `allowed_domains` or `roots` requires `unconfined_shell: true`.
- `..` in `policy.roots` is rejected.
- A name may not be in both `pass_env` and `secrets`.
- MCP servers: non-empty stdio `command`, `call_timeout_secs > 0`, a valid `url` (https, or http to loopback), and no underscore-prefix name clashes.
- The DAG declared by `nodes[*].needs:` has no cycles (`InvalidGraph`), no self-loops, and no edges to undefined nodes.
- Child-agent policies compose down: no more permissive than their parents' on the `allow_*` flags, `max_subagent_depth` at most the parent's minus one, roots inside the parent's roots.
- Every template compiles, and `nodes.<id>` reads name a transitive `needs:` ancestor.

`Read`/`Write`/`Edit` with empty `roots` is not caught here; it fails at run start with `AgentError::InvalidPolicy`.

A failure surfaces as `PipelineError` and exits non-zero with a description on stderr. No events are emitted.

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

## See also

- [CLI](cli.md) — every subcommand and its flags.
- [Tools](tools.md) — per-tool argument schemas and effect classes.
- [Events](events.md) — the wire format the pipeline emits.
- [Errors](errors.md) — typed errors raised on invalid YAML or invalid graphs.
- [Environment variables](env-vars.md) — names orno reads at the runtime boundary.
