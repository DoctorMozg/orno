# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## About orno

Orno is a CI-native runner for **strict agentic loops** (Rust workspace) in pre-v0.1 skeleton state. Trait seams and module tree are in place; the agent loop, tool handlers, LLM transport wiring, MCP client, scheduler parallelism, budget enforcement, and record/replay are intentionally unimplemented.

"Multi-agent" in orno means recursive single-agent loops where a parent treats a child as a tool call (Claude Code-style, ADR 0006), not peer-to-peer messaging between agents.

Read `docs/adr/` before making architectural changes — nine committed ADRs constrain most design decisions. `docs/yaml-spec.md` is the target user-facing YAML shape; `docs/roadmap.md` phases the v0.1.0 build.

## Common commands

```bash
# Build and check
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check     # verify
cargo fmt --all             # fix

# Test
cargo test --workspace --all-targets                # everything
cargo test -p orno-cli                              # single crate
cargo test -p orno-cli --test cli                   # single test binary
cargo test -p orno-cli run_emits_lifecycle_events   # single test by name

# Run locally
cargo run -p orno-cli -- run examples/hello.yaml
cargo run -p orno-cli -- validate examples/hello.yaml
cargo run -p orno-cli -- schema > schemas/pipeline.schema.json
cargo run -p orno-cli -- completions bash
```

CI (`.github/workflows/ci.yml`) runs `fmt --check`, `clippy -D warnings`, and `cargo test` on ubuntu, plus a release-build matrix on macos-14 and windows-2022. Toolchain pinned by `rust-toolchain.toml` (1.95).

## Workspace shape

Two crates, enforced by ADR 0001:

- `crates/orno-core/` — library. Pipeline schema, agent loop, node trait, LLM transport trait, tool handlers, MCP client, event log, execution engine, config, budget, telemetry. The binary's `clap` and `tokio` dependencies do NOT live here.
- `crates/orno-cli/` — binary (`orno`). Subcommand dispatch, output formatting, clap derive. Depends on `orno-core` only.

Do not split further without a concrete consumer or a build-parallelism justification.

## Architectural seams

Seven traits constrain the architecture. Every executor path routes through one of these; nothing side-steps them. Additions are append-only.

**Existing in the skeleton:**

1. **`LlmTransport`** (`orno-core/src/llm/mod.rs`) — every LLM call. Concrete impl wraps `genai` (ADR 0002); record/replay lands as a decorator. Do NOT expose `genai` types on orno's public surface.
2. **`NodeExecutor`** (`orno-core/src/node/mod.rs`) — every node kind. Subprocess plugins return post-v0.1 as a `transport:` axis on the existing kinds (ADR 0017 §3 supersedes the earlier `NodeKind::External` stub from ADR 0004).
3. **`EventSink`** (`orno-core/src/events/sink.rs`) — every lifecycle event. `InMemorySink` today; feature-gated `SqliteSink` plugs in without scheduler changes.
4. **`EventEnvelope { schema_version, seq, timestamp, event }`** (`orno-core/src/events/mod.rs`) with `#[non_exhaustive]` on `Event` — versioned wire format. `timestamp` is RFC 3339 UTC (ADR 0018); `seq` stays the strict-ordering key for replay. `Event::NodeSkipped` is part of the wire format; its `reason` field is a `#[non_exhaustive]` `SkipReason` enum (ADR 0021).

**Planned by ADRs 0005–0008, not yet implemented:**

5. **`Agent`** — agent-loop trait; `LoopAgent` implements ADR 0005's five strictness dimensions.
6. **`ToolHandler`** — one impl per builtin tool (`BashHandler`, `ReadHandler`, `EditHandler`, `WriteHandler`, `WebFetchHandler`) plus `SubagentHandler` (ADR 0006) and `McpHandler` (ADR 0007). See ADR 0008 for the fixed tool set.
7. **`McpClient`** — MCP protocol client. Wraps `rmcp` (ADR 0007); swap without touching tool dispatch.

## The five strictness dimensions

Every `agent` node enforces these at runtime (ADR 0005). They are user-facing guarantees, not internal knobs:

1. **Bounded iteration** — `max_iterations` mandatory; overrun → `IterationLimitExceeded` → terminate.
2. **Bounded tool surface** — only explicitly-listed builtins + MCP tools are callable. Unknown tool → `UnknownToolCalled` → terminate.
3. **Bounded effects** — `allow_mutations` + `allow_network` booleans plus `allowed_domains` / `blocked_domains`. Each tool has a declared effect class (ADR 0008 table).
4. **Bounded resources** — `max_total_tokens`, `max_tool_calls`. Breach → `BudgetExceeded { kind }` → terminate. Wall-clock is handled separately by the universal node-level `timeout:` attribute (ADR 0017) which emits `NodeTimedOut` and applies to every kind, not only agents.
5. **Bounded non-determinism** — every LLM request and tool call recorded; replay reproduces bit-for-bit.

Parallel tool calls are executed **serially** in declaration order. Parallelism is a DAG-level concern, not an agent-internal one.

## Pipeline wire format

User YAML is defined by `orno_core::pipeline::Pipeline`. Top-level blocks (v0.1.0 target):

- `version: u32` — schema version.
- `vars: Map<String, Value>` — template variables (MiniJinja).
- `agents: Map<String, AgentConfig>` — named agent configurations.
- `mcp_servers: Map<String, McpServerConfig>` — MCP servers spawned at run start, shut down at run end (ADR 0007).
- `nodes: [Node]` — the DAG.

Two distinct serde tag conventions are intentional:

- `Event` uses `#[serde(tag = "type")]` — lifecycle events.
- `NodeKind` / `NodeRequest` use `#[serde(tag = "kind")]` — pipeline node discriminator.

Both are `#[non_exhaustive]`. v0.1.0 node kinds are `agent` and `shell` (ADR 0009 collapsed `llm` into `agent`; ADR 0017 §1 removed the former `external` variant entirely — it returns post-v0.1 as a `transport:` axis on the existing kinds, not as a sibling kind). `http`/`parse`/`assert` are not separate kinds — their work happens inside agent tooling. After adding a variant that affects user pipelines, regenerate `schemas/pipeline.schema.json` via `cargo run -p orno-cli -- schema`.

Execution model: `Engine::run` drives a generator-style `DagWalker` (`execution::walker`) over the pipeline's DAG, dispatches each ready node through `NodeRegistry::get(kind_str).execute(id, req)` against a per-node `Context` (`execution::context`), and on node failure emits `NodeSkipped { reason: SkipReason::DependencyFailed { upstream } }` for every transitively-dependent node (ADR 0021). Disjoint branches keep running.

Stream separation in `orno run`:

- **stdout**: `EventEnvelope` NDJSON (consumed by downstream tools).
- **stderr**: `tracing` JSON logs (consumed by log pipelines).

Do not cross the streams. `init_tracing` in `orno-cli/src/main.rs` enforces the split. Both streams use the same RFC 3339 UTC timestamp format so a run's stdout and stderr are trivially joinable on wall clock (ADR 0018).

## Dependency discipline

Set on day 1, must be preserved:

- `default-features = false` on `reqwest`, `tokio`, `tracing-subscriber`, `minijinja`, `figment`. Feature lists are enumerated explicitly in root `Cargo.toml` under `[workspace.dependencies]`. Do not add `tokio` with `features = ["full"]`.
- YAML parser: **`serde_yaml_ng`** only. `serde_yaml` is archived; `serde_yml` carries RUSTSEC-2025-0068 and must never enter the tree.
- LLM stack: `genai` (ADR 0002), accessed only through `LlmTransport`.
- MCP stack: `rmcp` (ADR 0007), accessed only through `McpClient`.
- `unsafe_code = "forbid"` at the crate level in both crates.

Pedantic clippy is `warn` in both crates with a small documented allow list. When a new pedantic lint fires on intentional design, add a targeted allow with a one-line rationale in the same `[lints.clippy]` block rather than suppressing inline.

## Error conventions

One `thiserror` enum per subsystem (`CoreError`, `PipelineError`, `NodeError`, `AgentError`, `ToolError`, `LlmError`, `McpError`) in `orno-core/src/error.rs`. `#[from]` only when the conversion is unambiguous and the variant carries no extra context; otherwise use an explicit struct variant with `#[source]`. `anyhow::Result` is used exclusively in `orno-cli` at dispatch boundaries.

Use `#[error(transparent)]` on pass-through variants that wrap a foreign error (`std::io::Error`, `serde_json::Error`) without adding orno context — it forwards `Display` and `source()` cleanly. Reserve named struct variants with `#[source]` for errors that carry orno context (a pipeline path, a node id, a stage name). Every error enum is `#[non_exhaustive]` so new variants do not break downstream matches.

## Rust idioms

- **`async-trait` on every seam.** `orno-core` passes its trait objects as `Arc<dyn Trait>` (`LlmTransport`, `NodeExecutor`, `EventSink`, plus the planned `Agent`, `ToolHandler`, `McpClient`). Native `async fn` in traits (stable since 1.75) is not dyn-compatible, so `#[async_trait]` stays on every seam. Dropping it breaks trait-object dispatch with an opaque lifetime error — the macro is not optional here.
- **`#[non_exhaustive]` on every public enum.** Already on `Event`, `NodeKind`, `NodeRequest`, and each error enum. Adding a variant must stay non-breaking. Internal-only enums may skip it; enums reachable through serde or the public API must carry it.
- **Map-shaped variants only on tag-serialized enums.** Serde internal tagging (`tag = "type"`, `tag = "kind"`) needs each variant to serialize as a map. Named-field struct variants (`RunStarted { run_id }`) and newtype variants wrapping a struct (`Agent(AgentNode)`) both qualify; plain multi-field tuple variants do not. `Event` uses the first form, `NodeKind` / `NodeRequest` use the second.
- **Borrow in parameters, own in fields.** `&str` / `&Path` in function signatures, `String` / `PathBuf` in storage. Tool-handler return types are the exception: they own their output strings because they cross async boundaries.
- **Keep transport-library types off the public surface.** `genai::*` and `rmcp::*` live behind `LlmTransport` and `McpClient`. If a helper needs them, make it `pub(crate)` and translate at the trait boundary. This is the load-bearing rule behind ADRs 0002 and 0007 — breaking it forces every downstream crate to track `genai`/`rmcp` versions.
- **Traits live in files separate from their non-trivial implementations.** A trait file holds the contract plus the request/response types at its boundary. Concrete impls with real logic go in sibling files (`llm/dummy.rs`, `events/in_memory_sink.rs`). Zero-logic placeholders — `NoopEnforcer`-style impls whose methods return `Ok(())` or are empty — may stay alongside the trait because reading them does not distract from the contract. Re-export the moved type from the parent `mod.rs` so consumer paths don't break.
- **`#[must_use]` on constructors returning `Result`** and on builder methods that consume state. Catches silently-dropped errors and half-built configs at compile time.

## Tracing and logging

Stream discipline is already fixed: stdout = NDJSON events, stderr = tracing JSON. `init_tracing` in `orno-cli/src/main.rs` is the single setup site — do not call `tracing_subscriber::fmt()` from anywhere else.

- **Structured fields, not format strings.** `info!(node.id = %id, attempt = i, "retrying")` — never `info!("retrying node {id}")`. Fields preserve types for downstream log pipelines.
- **`%` for Display, `?` for Debug.** Prefer `%` on human-readable types (ids, paths, model names); `?` on structured values. Never `?` a struct with many fields — pick the fields you want.
- **Field names are `snake_case` with dot namespaces.** `pipeline.run_id`, `node.id`, `node.kind`, `tool.name`, `llm.model`, `llm.provider`, `http.status_code`. Matches OpenTelemetry semantic conventions so filters compose across tools.
- **`#[instrument]` on every seam-crossing async function.** Transport calls, node execute, tool invoke, sink write. Use `skip(self, …)` to keep large fields out of the span, `name = "…"` when the function name is too generic, and `fields(node.id = %id)` to hoist parameters.
- **Attach async work with `.instrument(span)`.** Never `let _g = span.enter()` across an `.await` — entering a span across yields corrupts the active-span stack.
- **No secrets in logs above `debug!`.** API keys, token-bearing headers, and MCP server env blocks are redacted before emission. Tool outputs may contain user data — never log them at `info!`.
- **Events ≠ tracing.** `EventSink::emit` is the user-facing log (stdout, versioned envelope, schema guaranteed). `tracing` is internal observability (stderr, no schema). Do not emit user-facing state via `tracing`, and do not emit diagnostic noise via `EventSink`.

## Comments

- **WHY, never WHAT.** If a block needs a narrator, rename the function instead.
- **`pub` items in `orno-core` carry a doc comment, either item-level `///` or a module-level `//!` that covers them.** When a module has a single dominant symbol (e.g. `pipeline::template::TemplateEngine`), the `//!` header is enough; when a file exposes multiple distinct symbols, each gets its own `///`. Trait-method doc comments describe the contract (preconditions, postconditions, panics); implementation notes belong inside the method body.
- **No TODO comments in committed code.** Open an issue or add to `docs/roadmap.md`. A rare exception takes the form `// TODO(user, 2026-Q?): <concrete removal trigger>`.
- **No section-header comments.** If a file wants `// === parsing ===` dividers, it wants splitting instead.
- **No autobiographical comments.** `// added to fix bug #47`, `// this used to use X`, `// refactored from the old module` — all of these belong in commit messages, not source.

## Testing patterns

- **CLI integration tests use `assert_cmd` + `predicates`.** They live in `crates/orno-cli/tests/` and invoke `orno` as a subprocess. `tests/cli.rs` is the template — copy its pattern rather than starting a parallel one.
- **Event-stream assertions use `insta` YAML snapshots.** Snapshots contain `run_<ULID>` ids (ADR 0019) and RFC 3339 timestamps (ADR 0018); redact with `insta::with_settings! { filters => vec![(regex, replacement)] }` alongside the test. The run_id redaction regex is `run_[0-9A-HJKMNP-TV-Z]{26}`; the timestamp regex matches the RFC 3339 form. Snapshots now also contain `node_skipped` envelopes (ADR 0021); `reason.upstream` is the originating failure, not the direct parent. Keep redactions next to the test, not in a shared helper — it makes the snapshot self-describing.
- **Parametric tests use `rstest`.** Each strictness dimension (iteration limit, unknown tool, mutation denied, network denied, budget exceeded) lands as an `#[rstest]` + `#[case]` table. Async cases combine `#[rstest]` with `#[tokio::test]` and mark fixture args with `#[future]`.
- **Hand-rolled fakes, not `mockall`.** The seam count is small enough that a fake struct in a `mod tests` block is clearer than derived mocks. `mockall` adds proc-macro latency and obscures intent.
- **One test per terminal `AgentError` variant.** Every strictness dimension must have at least one test that asserts termination with the exact expected variant. The loop's contract is "bounded" — only a test that checks the bound actually proves it.
- **`cargo insta review`** is the accept-new-snapshots workflow. Never edit `.snap` files by hand; run the review command or delete and re-run.

## ADRs

- `docs/adr/0001-workspace-split.md` — two-crate split rationale.
- `docs/adr/0002-llm-client-genai.md` — `genai` wrapped behind `LlmTransport`.
- `docs/adr/0003-event-log-from-day-one.md` — original four trait seams; extended by ADRs 0005–0008.
- `docs/adr/0004-defer-plugin-protocol.md` — no whole-node plugin loader until post-v0.1; clarified by ADRs 0008–0009.
- `docs/adr/0005-strict-agentic-loops.md` — five strictness dimensions as user-facing guarantees.
- `docs/adr/0006-subagent-as-tool-call.md` — recursive subagent, not peer-to-peer multi-agent.
- `docs/adr/0007-mcp-via-rmcp.md` — MCP via `rmcp`, wrapped behind `McpClient`.
- `docs/adr/0008-builtin-tool-set.md` — `Bash`/`Read`/`Edit`/`Write`/`WebFetch` + MCP; WebSearch deferred.
- `docs/adr/0009-single-agent-node-kind.md` — collapse `llm` into `agent`.
- `docs/adr/0017-node-attributes-over-new-kinds.md` — v0.1 `NodeKind` = `Agent, Shell` (no `External`); universal `retry:` / `timeout:` attributes; shell output splits to `.stdout` / `.stderr` / `.exit_code`.
- `docs/adr/0018-event-envelope-timestamp.md` — RFC 3339 UTC `timestamp` field on `EventEnvelope`; matching stderr tracing timer so both streams join on wall clock.
- `docs/adr/0019-run-id-ulid.md` — run identifiers are `run_<ULID>`; generator lives in `orno-core::execution::new_run_id`.
- `docs/adr/0020-env-and-secrets-namespaces.md` — two template namespaces (`env.*` opt-in inputs, `secrets.*` redacted credentials) with distinct precedence rules; CLI adds `-e`, `--env-file`, `--secrets-file`.
- `docs/adr/0021-dag-execution-model.md` — generator-style `DagWalker` with Kahn cycle detection; per-node `Context` with vars/env/nodes namespaces; transitive skip cascade via `NodeSkipped { reason: SkipReason::DependencyFailed { upstream } }` naming the originator; Engine drives walker + `NodeRegistry` serially.

Never revise an accepted ADR. Add an `## Amendments` section pointing to a newer ADR, or supersede with a new ADR. Historical decisions must remain readable.

## Roadmap and YAML spec

- `docs/roadmap.md` — phased plan for v0.1.0 strict-agentic-MVP (8–10 weeks), deferrals for v0.2.0+.
- `docs/yaml-spec.md` — full v0.1.0 user-facing YAML shape. Examples in `examples/` conform to this shape (not to the current skeleton, which implements a subset).

## Research context

`docs/initial_research.md` (market landscape), `docs/implementation_toolset_research.md` (library selection), and `docs/chat.md` (agentic-loop architecture discussion) predate the ADRs. They are frozen reference documents — when their recommendations were overridden, the override is captured in an ADR (see 0002 for `genai`, 0008 for Architecture A).
