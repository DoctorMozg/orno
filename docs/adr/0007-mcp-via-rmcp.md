# ADR 0007 — MCP via rmcp wrapped behind an `McpClient` trait

- Status: accepted
- Date: 2026-04-21

## Context

MCP (Model Context Protocol) is the one genuinely new abstraction since
tool-calling was standardized. For orno, MCP is how users extend the
fixed builtin toolset (ADR 0008) without convincing us to add a tool
to the core. Two implementation paths exist:

- Use `rmcp`, the official Anthropic-maintained Rust SDK. Handles
  protocol versioning, capability negotiation, stdio + SSE +
  streamable-http transports, tool/resource/prompt lifecycle. Real
  dependency with its own release churn; MCP spec had breaking
  changes through 2025.
- Hand-roll a JSON-RPC client for just what we need (initialize,
  tools/list, tools/call, maybe resources/read). ~300-400 lines for
  stdio, more for http. No dependency, full control, but we own
  protocol drift.

The same trade-off applies to the LLM transport (ADR 0002): library
dependency versus hand-rolled wire code. The resolution there — use
the library, wrap it behind a thin trait — applies identically here.

## Decision

- Depend on `rmcp` for v0.1.0.
- Wrap it behind `trait McpClient` in `orno-core`, with a minimal
  surface: `initialize`, `list_tools`, `call_tool`, `shutdown`. No
  `rmcp` types escape the trait.
- Concrete `RmcpClient` lives in its own module. Swapping to
  hand-rolled later requires touching one file.
- MCP servers are **lifecycle-managed by the run**, not by the
  node:
  - `mcp_servers:` is a top-level YAML block alongside `agents:`
    and `nodes:`. Each entry declares `transport` (`stdio` or
    `http`), command/url, env, and auth.
  - At run start, orno spawns each declared server, performs the
    MCP handshake, calls `tools/list`, and caches schemas.
  - Tool declarations in agents list MCP tools in their
    `allowed_tools` as `mcp.<server>.<tool>` or the server
    wildcard `mcp.<server>.*` (ADR 0008). Explicit — no auto-
    discovery implicit in listing a server. Matches ADR 0005's
    bounded tool-surface rule. Dots rewrite to underscores at the
    wire (`mcp_github_search_issues`) for provider naming.
  - At run end (success, failure, cancellation), each server gets
    a clean shutdown: `notifications/exit` for stdio, connection
    close for http, SIGTERM fallback after timeout if the server
    does not exit cleanly.
- Lifecycle events: `McpServerStarting`, `McpServerHandshaked`,
  `McpToolCallSent`, `McpToolCallCompleted`, `McpServerShuttingDown`,
  `McpServerExited`, `McpServerCrashed`.
- Schema sync: the server's advertised schema (via `tools/list`) is
  the authoritative version. If the server's schema does not match
  the model's expectations for the declared tool, that is a model
  problem, not a loader problem. Orno validates only that the
  declared `server.tool` pair exists at run start; if not, it
  fails loudly before any model call.
- Server crash mid-run is a typed event. v0.1.0 policy:
  terminate the owning agent with a `McpServerCrashed` tool-call
  failure. Restart policies deferred to a later ADR.

## Consequences

- Subprocess spawn cost is paid once per run, not per tool call.
- `rmcp` version churn is isolated behind `McpClient` — swap
  without touching tool dispatch or the agent loop.
- MCP lifecycle is visible in the event log at the same granularity
  as LLM calls, so replay reasons about MCP correctly.
- Budget 1–2 days per quarter for `rmcp`/MCP spec upgrades. This is
  the cost of not hand-rolling and it is the right trade at current
  scope.
- Tests must exercise at least one real MCP server (a filesystem or
  github server) so the wrapper is not "library works, wrapper
  half-broken."
- Auto-discovery is explicitly rejected for v0.1.0. Users list the
  tools they want exposed; SRE review of `mcp_servers:` + `tools:`
  is the audit trail.
