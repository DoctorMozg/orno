# ADR 0008 — Builtin tool set, no user-authored tool schemas

- Status: accepted
- Date: 2026-04-21

## Context

`docs/chat.md` §"Architecture A" proposes user-declared tools in YAML:
each tool gets a JSON Schema and a handler (shell, http, mcp, …) and
orno renders the tool surface into provider format, validates the
model's arguments, and dispatches. This is the Terraform-for-agents
positioning at maximum generality.

It is also the hardest thing to get right. Tool-schema validation,
argument coercion, per-handler argument templating, and the security
review burden of "arbitrary user-declared commands plus LLM-supplied
arguments" dominate the implementation cost. Claude Code's model —
a fixed, well-known toolset with MCP as the extension seam — is
strictly simpler, strictly auditable, and covers the majority of CI
use cases with no loss of expressive power because MCP is Turing-
complete at the tool-provider level.

## Decision

v0.1.0 ships a fixed builtin toolset. Extension happens only through
MCP (ADR 0007). Users do not author tool JSON schemas.

### Builtin toolset

| Tool       | Effect class         | Primary args              | Notes                                                           |
| ---------- | -------------------- | ------------------------- | --------------------------------------------------------------- |
| `Bash`     | shell (mut + net)    | `cmd`, `timeout`, `cwd`   | Requires both `allow_mutations` and `allow_network`.            |
| `Read`     | local_read           | `path`                    | File only; directory listings go through `Bash("ls")`.          |
| `Edit`     | local_write          | `path`, `old_string`, `new_string` | Requires `allow_mutations`. Exact-string replace.         |
| `Write`    | local_write          | `path`, `content`         | Requires `allow_mutations`. Creates parent dirs implicitly.     |
| `WebFetch` | network_read         | `url`                     | Requires `allow_network`. Raw text + `content-type`, no HTML simplification. |
| `mcp.<server>.<tool>` | declared per server | passthrough    | See ADR 0007. Effect class inherited from MCP tool declaration. |

Each builtin has a typed args struct; the Rust type system is the
schema. No runtime JSON-Schema validation for builtin tools. The
JSON surface presented to the LLM is derived from the struct via
`schemars`.

### Agent selection

Agents enable builtins through `allowed_tools`:

```yaml
allowed_tools:
  - Bash
  - Read
  - Edit
  - WebFetch
  - "mcp.github.*"
```

Wildcards (`mcp.<server>.*`) are permitted for MCP only, to let an
agent use every tool an MCP server exposes without listing them one
by one. Wildcards on builtins are disallowed — the builtin set is
small enough that explicit listing is always practical.

### Effect model (feeds ADR 0005 dimension 3)

Two orthogonal booleans on each agent:

- `allow_mutations` — gates `Edit`, `Write`, mutating MCP tools.
- `allow_network` — gates `WebFetch`, network MCP tools.
- `Bash` requires both (it can do either).
- `Read` needs neither.

Network tools further honor `allowed_domains: [...]` /
`blocked_domains: [...]` on the agent; blocklist wins on overlap.

### Deferred / explicitly out of scope for v0.1.0

- `WebSearch` — needs a configured provider (Tavily, Brave, etc.)
  and its own trait. Slated for post-v0.1.0.
- Generic `HttpHandler` — use MCP if you need HTTP-backed tools.
- User-authored tool schemas (full Architecture A) — revisited
  only if MCP proves insufficient.

## Consequences

- Each builtin is one concrete `ToolHandler` impl with a typed
  args struct. ~6 impls total at v0.1.0, modulo MCP.
- Tool-call events carry a static effect class string — the audit
  trail is an enumerable set, not an open-world set.
- Adding a new builtin requires a new ADR plus a code change. This
  is the feature, not a bug — auditability by design.
- Agents that need an HTTP API either bring an MCP server, shell
  out via `Bash`, or wait for a specific builtin to be added. The
  friction is intentional.
- The pipeline schema regeneration after adding `agents:`,
  `mcp_servers:`, `allowed_tools`, and `AgentPolicy` is a single
  step; it is still a `schemars` derive pass, not a hand-written
  schema.
- Security review for v0.1.0 is tractable: seven tool shapes,
  five strictness dimensions (ADR 0005), one MCP wrapper
  (ADR 0007).
