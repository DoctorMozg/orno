# ADR 0004 — Defer the plugin protocol; design the wire format now

- Status: accepted; node-kind set and extension model clarified by ADRs 0008–0009
- Date: 2026-04-21

## Context

Plugin systems are the single biggest trap for solo Rust tools — design
takes weeks, maintenance is forever, and the first user's feedback rewrites
the API anyway. WASM/WASI is a bad fit for orno's plugin domain: plugins
want to shell out, hit HTTPS, read files, spawn subprocesses. Dynamic
loading (`libloading`, `abi_stable`, `stabby`) has no stable Rust ABI and
inflicts version-skew debugging.

## Decision

- No plugin loader ships in v0.1.0.
- A versioned wire format exists from commit 1:
  `NodeRequest` and `NodeResponse` are `#[serde(tag = "kind")]` enums with
  `#[non_exhaustive]` and a `schema_version: u32` header.
- `NodeKind::External { command: String, args: Vec<String>, timeout: Option<Duration> }`
  is a stub variant on the internal `NodeKind` enum with no executor
  registered.
- Plugins, when they land post-v0.1, will be subprocesses spoken to via
  JSON over stdin/stdout (the `terraform-plugin` pattern, minus gRPC).
  Built-in node executors implement the same `NodeExecutor` trait the
  subprocess transport will implement.
- WASM plugins are explicitly out of scope for v1.x.

## Consequences

- v0.1.0 is fixed in node kind set: `llm`, `shell`, `http`, `parse`,
  `assert`. Users wanting more wait for real plugin support.
- Any plugin work done post-v0.1 reuses the `NodeExecutor` trait and
  `NodeRequest`/`NodeResponse` serde contract — the orchestrator core
  never needs to learn about subprocess transports specifically.
- If real user demand points at in-process plugins (hot-path tokenizers,
  custom budget enforcers), this ADR is revisited — but in-process plugins
  without a stable Rust ABI are rejected by default.

## Amendments

ADRs 0008 (builtin tool set) and 0009 (single agent node kind) clarify
the v0.1.0 node-kind set and what "extensibility" means in practice.

- The v0.1.0 node-kind set is now **`agent`, `shell`, `external`**.
  `llm` has been collapsed into `agent` (ADR 0009). `http`, `parse`,
  and `assert` are not v0.1.0 node kinds — their former
  responsibilities live inside agent tooling (`WebFetch`, `Bash`
  pipelines, agent assertion prompts).
- `ToolHandler`, `SubagentHandler`, and `McpHandler` (ADRs 0006–0008)
  are **not plugins**. They live in-process in `orno-core` and
  implement builtin behavior. Extending tools at v0.1.0 happens by
  authoring an MCP server (ADR 0007), not by loading a node-kind
  plugin. The whole-node-plugin deferral in the original decision
  stands; the extension-via-tool-handler path in ADRs 0006–0008 is
  not a plugin path.
- `NodeKind::External` remains a stub and still reserves the
  subprocess-plugin slot for post-v0.1. The wire format for that
  slot is unchanged.
