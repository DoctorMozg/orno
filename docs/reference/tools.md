# Tools reference

Every tool an agent can call routes through a `ToolHandler` impl. Each handler declares:

- A canonical **name** (the string used in `allowed_tools` and the LLM's tool-call request).
- A **description** the LLM sees.
- A **JSON Schema** for arguments, generated from the handler's `*Args` struct via `schemars`.
- A declared **effect class** that `LoopAgent` checks against the agent's `policy` before dispatch.

This page documents the six builtin tools, the synthetic `subagent.<name>` tool, and the wire form for MCP-advertised tools. The full effect-class semantics are covered in [pipeline-yaml.md](pipeline-yaml.md#effect-class-reference).

## Effect classes

| Class                 | Meaning                                                                                                        | Required policy fields                          |
| --------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| `ReadOnly`            | No filesystem, process, or network mutations. Pure side-effect-free reads.                                     | none                                            |
| `Mutations`           | Mutates local state (filesystem, processes). Does not initiate network requests.                                | `allow_mutations: true`                         |
| `Network`             | Issues network requests (read or write). Does not mutate local state.                                           | `allow_network: true` + domain rules            |
| `MutationsAndNetwork` | Both mutations and network. Used by `Bash` (a shell command can do anything) and by every MCP tool.             | both `allow_mutations` and `allow_network`      |
| `ContextSelf`         | Mutates `nodes.<self>.state.*` only. Does not imply external mutation.                                          | `allow_context_writes: true`                    |

A tool whose effect class is not satisfied by the active `AgentPolicy` is denied **before** dispatch — the handler's `invoke` is never called. The denial fires `Event::ToolDenied` and feeds a denial string back to the model as the tool's result; the loop continues. This is non-terminal by design — strict-mode termination is reserved for `UnknownToolCalled`, where the model called a name that was not in `allowed_tools` at all.

## Builtin tools

### `Bash`

Run a shell command via `/bin/sh -c`. Effect class **`MutationsAndNetwork`** — a shell can edit files and reach the network, so both flags must be on.

| Field          | Type    | Required | Default | Description                                   |
| -------------- | ------- | -------- | ------- | --------------------------------------------- |
| `command`      | string  | yes      | —       | Shell command to execute.                     |
| `timeout_secs` | integer | no       | `60`    | Per-call wall-clock cap.                      |
| `cwd`          | string  | no       | —       | Working directory. Pass-through to the child. |

Output is a single string: `exit_code: <n>\nstdout:\n<stdout>\nstderr:\n<stderr>`. A non-zero exit is **not** an error — it is reported in the tool result and the loop continues. A timeout returns `ToolError::Invocation` and terminates the call (non-zero from `tokio::time::timeout`).

```json
{ "command": "git diff --stat HEAD~1..HEAD", "timeout_secs": 10 }
```

### `Read`

Read a file's contents. Effect class **`ReadOnly`**.

| Field  | Type   | Required | Default | Description         |
| ------ | ------ | -------- | ------- | ------------------- |
| `path` | string | yes      | —       | File path to read.  |

Returns the file's text content as a UTF-8 string. A non-existent path returns `ToolError::Invocation`.

```json
{ "path": "src/main.rs" }
```

### `Edit`

Replace a unique substring in a file. Effect class **`Mutations`**.

| Field        | Type   | Required | Default | Description                |
| ------------ | ------ | -------- | ------- | -------------------------- |
| `path`       | string | yes      | —       | File path to edit.         |
| `old_string` | string | yes      | —       | Unique substring to find.  |
| `new_string` | string | yes      | —       | Replacement text.          |

`old_string` must occur **exactly once** in the file. Zero or multiple occurrences return `ToolError::InvalidArgs` (fed back to the model as a denial string; loop continues). Returns a confirmation string on success.

```json
{
  "path": "Cargo.toml",
  "old_string": "version = \"0.0.1\"",
  "new_string": "version = \"0.1.0\""
}
```

### `Write`

Write content to a file. Effect class **`Mutations`**.

| Field     | Type   | Required | Default | Description                                     |
| --------- | ------ | -------- | ------- | ----------------------------------------------- |
| `path`    | string | yes      | —       | File path. Parent directories created as needed. |
| `content` | string | yes      | —       | Content to write. Overwrites if file exists.     |

Returns `Wrote <N> bytes to <path>` on success.

```json
{ "path": "out/report.md", "content": "# Report\n..." }
```

### `WebFetch`

HTTP GET a URL. Effect class **`Network`**. Domain policy (`allowed_domains` / `blocked_domains`) is enforced by `LoopAgent` before dispatch.

| Field          | Type    | Required | Default | Description                              |
| -------------- | ------- | -------- | ------- | ---------------------------------------- |
| `url`          | string  | yes      | —       | Absolute URL to fetch.                   |
| `timeout_secs` | integer | no       | `30`    | Per-request cap.                         |

Returns `status: <code>\ncontent-type: <type>\n\n<body>`. The body is capped at 1 MiB; oversized responses get a trailing `\n[truncated at 1 MiB]` marker. The handler does not retry, follow only the platform `reqwest` defaults for redirects, and never executes JavaScript.

```json
{ "url": "https://api.github.com/repos/anthropics/orno", "timeout_secs": 10 }
```

### `SetState`

Write a single top-level key under `nodes.<self>.state.*`. Effect class **`ContextSelf`** — gated by `policy.allow_context_writes`, off by default.

| Field   | Type        | Required | Default | Description                                                                |
| ------- | ----------- | -------- | ------- | -------------------------------------------------------------------------- |
| `key`   | string      | yes      | —       | Identifier matching `^[A-Za-z_][A-Za-z0-9_]*$`. Single-level only.          |
| `value` | JSON value  | yes      | —       | Any JSON value. Replaces the prior value at this key wholesale.            |

Semantics:

- **Single-level keys only.** `key: "result"` is valid; `key: "deeply.nested"` is not (regex rejection at argument validation). Cross-node nesting is achieved by addressing the upstream node — `{{ nodes.review.state.result }}` reads from a different node, not from a dotted key.
- **Whole-value replacement.** A second `SetState` with the same `key` overwrites — there is no merge.
- **Bounded.** After every write, the entire `state` tree is re-serialized and compared against the engine's `max_output_bytes` cap. An oversized write returns `ToolError::StateTooLarge` and **rolls back** before the lock is released; the prior state survives intact.
- **Redacted on storage.** Secret-named leaves in the value payload are redacted to `***` before persistence. Downstream nodes never see the unredacted secret even if the writer's prompt contained it.
- **Confined to the writer.** Only the writing node can address its own state under `state.*`. Downstream readers reach for `nodes.<writer-id>.state.<key>`.

```json
{ "key": "verdict", "value": { "approved": true, "blockers": [] } }
```

## Synthetic tools

### `subagent.<name>`

Delegate a sub-task to a child agent loop. Effect class is derived from the **child's** declared effects (a read-only child handler has effect `ReadOnly`; a network-capable child has `Network`; etc.). Pipeline-load enforcement guarantees the child is no more permissive than the parent on `allow_mutations` / `allow_network`, so this composition is sound.

| Field    | Type   | Required | Default | Description                                                                                |
| -------- | ------ | -------- | ------- | ------------------------------------------------------------------------------------------ |
| `prompt` | string | yes      | —       | Initial user prompt for the child loop. Becomes the child's `initial_prompt`.              |

Wire-form name: `subagent_<name>` (dots collapsed to underscores for provider compatibility). YAML and event logs use the dotted form.

The child runs a fresh `LoopAgent` with its own configuration from `agents.<name>`. Recursion is bounded by `policy.max_subagent_depth` on the **parent** — a depth-exhausted dispatch fires `Event::SubagentDepthExceeded`, never enters the child, and feeds a denial string back to the parent.

```yaml
agents:
  reviewer:
    allowed_tools:
      - subagent.security_lens
      - subagent.style_lens
    policy:
      max_subagent_depth: 1
```

```json
{ "prompt": "Review the diff for SQL injection risks." }
```

### `mcp.<server>.<tool>`

Bridge to a tool advertised by an MCP server declared under `mcp_servers.<server>:`. Effect class is conservatively **`MutationsAndNetwork`** for every MCP tool — orno cannot inspect the server's per-tool semantics at registration time, and an MCP server is an arbitrary subprocess or remote service. Domain policy applies if the agent declares one; the MCP transport call itself is treated as network.

Argument schema is whatever the server returns from its `tools/list` response — orno forwards the schema verbatim to the model. Wire-form name on the LLM is `mcp_<server>_<tool>`.

A wildcard entry `mcp.<server>.*` in `allowed_tools` resolves to every tool the server advertises at handshake time. Listing a wildcard against a server that exists but lists zero tools is not an error — the agent simply has no MCP tools available from that server.

```yaml
mcp_servers:
  github:
    transport: stdio
    command: ["npx", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "{{ secrets.GITHUB_TOKEN }}"

agents:
  triager:
    allowed_tools:
      - mcp.github.search_issues
      - mcp.github.create_issue
```

## Tool dispatch flow

For every model-emitted tool call, the loop runs this sequence:

1. **Name lookup.** Is the tool name in `allowed_tools`? Unknown name → `Event::UnknownToolCalled` → `AgentError::UnknownToolCalled` → terminate the node.
2. **Effect-class gate.** Does the tool's effect class match the active `AgentPolicy`? Mismatch → `Event::ToolDenied` with a reason → loop continues with the denial fed back to the model.
3. **Domain check** (network-capable tools only). Does the URL/host pass `allowed_domains` and `blocked_domains`? Mismatch → `Event::DomainBlocked` → loop continues.
4. **Argument parse.** Does the JSON match the tool's schema? Failure → `ToolError::InvalidArgs` → fed back as a denial; if `policy.on_parse_error: fail` and this exhausted the retry budget, terminate.
5. **Invoke.** Call `ToolHandler::invoke(inv, args)`.
6. **Record.** Emit `Event::ToolCallRecorded` with redacted, head-truncated input and output excerpts.

The handler itself does no policy check — `LoopAgent` pre-clears the call. A handler can assume it is authorized to act.

## See also

- [Pipeline YAML › `allowed_tools` grammar](pipeline-yaml.md#allowed_tools-grammar) — the exact strings accepted in agent policy.
- [Pipeline YAML › Effect-class reference](pipeline-yaml.md#effect-class-reference) — table of which policy fields gate which tools.
- [Events](events.md) — `ToolCallRecorded`, `ToolDenied`, `UnknownToolCalled`, and the MCP envelope variants.
- [Errors › `ToolError`](errors.md#toolerror) — `Invocation`, `InvalidArgs`, `StateTooLarge`, `NotImplemented`.
