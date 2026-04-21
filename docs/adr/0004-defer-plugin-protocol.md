# ADR 0004 — Defer the plugin protocol; design the wire format now

- Status: accepted
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
