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

A tool whose effect class is not satisfied by the active `AgentPolicy` is denied **before** dispatch — the handler's `invoke` is never called. The denial fires `Event::ToolDenied` and feeds a `denied: ...` string back to the model as the tool's result; the loop continues. This is non-terminal by design — strict-mode termination is reserved for `AgentError::UnknownToolCalled`, where the model called a name that was not in `allowed_tools` at all.

## Limits at a glance

| Tool       | Limits                                                                                                                       |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `Bash`     | `timeout_secs` 1 to 600 (default 60); 256 KiB captured per stream (stdout, stderr); `cwd` only with non-empty `roots`.        |
| `Read`     | 10 MiB per file; requires non-empty `roots`.                                                                                  |
| `Edit`, `Write` | Require non-empty `roots`; keep the target's permissions.                                                                |
| `WebFetch` | `timeout_secs` default 30; 1 MiB response body; at most 5 redirect hops.                                                      |
| `SetState` | Serialized `state` at most `EngineConfig.max_output_bytes`.                                                                   |
| every tool | Output entering the conversation is capped by `policy.max_tool_output_bytes` (default 256 KiB), head-truncated with an ellipsis. |

## Recoverable and terminal tool errors

A tool failure the model can act on is **recoverable**: the loop feeds it back as a tool result of the form `error: <message>` and keeps going. Everything else is **terminal** and ends the node with `AgentError::Tool`.

| Recoverable (returned to the model)                                              | Terminal (node fails)                                                              |
| -------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- |
| A missing file (`not_found`) or non-UTF-8 content a tool cannot use (`invalid_data`) | Path-jail and policy denials from inside a handler (`ToolError::Denied`)       |
| `Bash` or `WebFetch` timeout (`timeout`); the `Bash` message includes partial output | Internal failures (`ToolError::Invocation`): spawn errors, WebFetch URL, redirect and address denials |
| `Edit` with zero or several matches (`no_match`)                                  | Oversize `SetState` (`ToolError::StateTooLarge`)                                    |
| MCP result with `isError: true` (`tool_reported`)                                 | Malformed arguments (`ToolError::InvalidArgs`), unless `on_parse_error: retry_once` feeds the first one back |

Effect-class and domain denials by the loop itself are a third path: a `denied: ...` result plus `tool_denied`, never terminal. See [errors.md](errors.md#toolerror) for the types.

## Builtin tools

### `Bash`

Run a shell command via `/bin/sh -c`. Effect class **`MutationsAndNetwork`** — a shell can edit files and reach the network, so both flags must be on.

| Field          | Type    | Required | Default | Description                                                                                   |
| -------------- | ------- | -------- | ------- | --------------------------------------------------------------------------------------------- |
| `command`      | string  | yes      | —       | Shell command to execute.                                                                     |
| `timeout_secs` | integer | no       | `60`    | Per-call wall-clock cap, 1 to 600. A value outside the range is a terminal `ToolError::Invocation`. |
| `cwd`          | string  | no       | —       | Working directory, resolved inside `policy.roots[0]`. Denied when `roots` is empty.           |

Output is a single string: `exit_code: <n>\nstdout:\n<stdout>\nstderr:\n<stderr>`. A child killed by a signal reports `exit_code: signal (signal: <n>)`. Each stream is capped at 256 KiB; a `[stdout truncated at 262144 bytes]` line (likewise for stderr) is prepended when a cap fires. A non-zero exit is **not** an error — it is reported in the tool result and the loop continues. A timeout is recoverable: the model gets `error: command timed out after <n>s` followed by the partial stdout and stderr.

The shell starts with a cleared environment holding only `PATH`, `HOME`, `USER`, `TMPDIR`, `LANG`, `LC_ALL` and `TERM`, so provider keys are not visible to it. It runs in its own process group, and the group is killed when the command exits, times out, or is cancelled: a background process the command started does not outlive it.

A shell reaches any host and path, so `allowed_domains` and `roots` do not bind it. Validation rejects an agent that lists `Bash` with either set unless `policy.unconfined_shell: true` acknowledges that.

```json
{ "command": "git diff --stat HEAD~1..HEAD", "timeout_secs": 10 }
```

### `Read`

Read a file's contents. Effect class **`ReadOnly`**. Jailed to `policy.roots` (required, non-empty); a path outside the root, or a symlink leading out of it, is denied.

| Field  | Type   | Required | Default | Description         |
| ------ | ------ | -------- | ------- | ------------------- |
| `path` | string | yes      | —       | File path to read.  |

Returns the file's text, decoded lossily so invalid UTF-8 never fails the call. Files over 10 MiB return the first 10 MiB plus a `[truncated: file is <n> bytes, showing first 10485760 bytes]` marker. A missing path is a recoverable `not_found` error. FIFOs, devices and directories are rejected instead of read.

```json
{ "path": "src/main.rs" }
```

### `Edit`

Replace a unique substring in a file. Effect class **`Mutations`**. Jailed to `policy.roots`.

| Field        | Type   | Required | Default | Description                |
| ------------ | ------ | -------- | ------- | -------------------------- |
| `path`       | string | yes      | —       | File path to edit.         |
| `old_string` | string | yes      | —       | Unique substring to find.  |
| `new_string` | string | yes      | —       | Replacement text.          |

`old_string` must occur **exactly once** in the file. Zero or multiple occurrences are a recoverable `no_match` error, returned to the model as `error: ...` while the loop continues. The file's permissions are preserved. Returns a confirmation string on success.

```json
{
  "path": "Cargo.toml",
  "old_string": "version = \"0.0.1\"",
  "new_string": "version = \"0.1.0\""
}
```

### `Write`

Write content to a file. Effect class **`Mutations`**. Jailed to `policy.roots`; parent directories are created only inside the root.

| Field     | Type   | Required | Default | Description                                     |
| --------- | ------ | -------- | ------- | ----------------------------------------------- |
| `path`    | string | yes      | —       | File path. Parent directories created as needed. |
| `content` | string | yes      | —       | Content to write. Overwrites if file exists.     |

Returns `Wrote <N> bytes to <path>` on success. An existing target keeps its permissions.

```json
{ "path": "out/report.md", "content": "# Report\n..." }
```

### `WebFetch`

HTTP GET a URL. Effect class **`Network`**. Domain policy (`allowed_domains` / `blocked_domains`) is checked by `LoopAgent` before dispatch and again inside the handler.

| Field          | Type    | Required | Default | Description                              |
| -------------- | ------- | -------- | ------- | ---------------------------------------- |
| `url`          | string  | yes      | —       | Absolute `http` or `https` URL to fetch. |
| `timeout_secs` | integer | no       | `30`    | Per-request cap.                         |

Returns `status: <code>\ncontent-type: <type>\n\n<body>`. The body is capped at 1 MiB; oversized responses get a trailing `\n[truncated at 1 MiB]` marker. A timeout is a recoverable `timeout` error. The handler does not retry and never executes JavaScript.

Redirects are followed by a manual loop of at most 5 hops. Every hop, not just the first URL, is re-checked against the scheme, the domain lists and the blocked-address ranges, and a denied hop fails the call as a terminal error. The client ignores `HTTP_PROXY` / `HTTPS_PROXY`, and DNS answers are filtered at connect time, so a name that resolves to a loopback, private, link-local or metadata address cannot be dialed even after an allowlist match.

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

The child runs a fresh `LoopAgent` with its own configuration from `agents.<name>`. Recursion is bounded by `policy.max_subagent_depth` on the **parent** — a depth-exhausted dispatch fires `Event::SubagentDepthExceeded`, never enters the child, and feeds a denial string back to the parent. `max_total_tokens` and `max_tool_calls` cover the whole tree: the child's spend and tool calls count against the root and every ancestor. Iterations stay per loop. The call itself must be listed in the parent's `allowed_tools` like any other tool.

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

1. **Budget.** Charge one call against `max_tool_calls` (tree-wide). Over budget → `AgentError::BudgetExceeded` → terminate the node.
2. **Name lookup.** Is the tool name in `allowed_tools`? Unknown name → `AgentError::UnknownToolCalled` → terminate the node.
3. **Subagent depth.** A `subagent.<name>` call past `max_subagent_depth` → `Event::SubagentDepthExceeded` and a denial result; the loop continues.
4. **Effect-class gate.** Does the tool's effect class match the active `AgentPolicy`? Mismatch → `Event::ToolDenied` with a reason → loop continues with the denial fed back to the model.
5. **Domain check** (network-capable tools only, including MCP tools). Do the URLs in the arguments pass the scheme, `allowed_domains`, `blocked_domains` and blocked-address checks? Failure → `Event::ToolDenied` with the reason → loop continues.
6. **Invoke.** Call `ToolHandler::invoke(inv, args)`. Argument-parse failures (`ToolError::InvalidArgs`) end the node as `AgentError::ParseFailed`, except that `on_parse_error: retry_once` feeds the first one per call back to the model. A recoverable error becomes an `error: ...` result; any other error terminates the node.
7. **Record.** Emit `Event::ToolCallRecorded` with redacted, head-truncated excerpts. The result is then redacted and truncated to `max_tool_output_bytes` before it joins the conversation.

`LoopAgent` pre-clears the effect class and domains, but handlers still enforce their own boundaries: the file tools check the root jail, `WebFetch` re-checks every redirect hop, and `Bash` confines `cwd`.

## See also

- [Pipeline YAML › `allowed_tools` grammar](pipeline-yaml.md#allowed_tools-grammar) — the exact strings accepted in agent policy.
- [Pipeline YAML › Effect-class reference](pipeline-yaml.md#effect-class-reference) — table of which policy fields gate which tools.
- [Events](events.md) — `tool_call_recorded`, `tool_denied`, and the MCP envelope variants.
- [Errors › `ToolError`](errors.md#toolerror) — `Recoverable`, `Invocation`, `InvalidArgs`, `StateTooLarge`, `NotImplemented`, `Denied`.
