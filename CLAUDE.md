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
2. **`NodeExecutor`** (`orno-core/src/node/mod.rs`) — every node kind. Subprocess plugins (ADR 0004) will implement this via `NodeKind::External`.
3. **`EventSink`** (`orno-core/src/events/sink.rs`) — every lifecycle event. `InMemorySink` today; feature-gated `SqliteSink` plugs in without scheduler changes.
4. **`EventEnvelope { schema_version, seq, event }`** (`orno-core/src/events/mod.rs`) with `#[non_exhaustive]` on `Event` — versioned wire format.

**Planned by ADRs 0005–0008, not yet implemented:**

5. **`Agent`** — agent-loop trait; `LoopAgent` implements ADR 0005's five strictness dimensions.
6. **`ToolHandler`** — one impl per builtin tool (`BashHandler`, `ReadHandler`, `EditHandler`, `WriteHandler`, `WebFetchHandler`) plus `SubagentHandler` (ADR 0006) and `McpHandler` (ADR 0007). See ADR 0008 for the fixed tool set.
7. **`McpClient`** — MCP protocol client. Wraps `rmcp` (ADR 0007); swap without touching tool dispatch.

## The five strictness dimensions

Every `agent` node enforces these at runtime (ADR 0005). They are user-facing guarantees, not internal knobs:

1. **Bounded iteration** — `max_iterations` mandatory; overrun → `IterationLimitExceeded` → terminate.
2. **Bounded tool surface** — only explicitly-listed builtins + MCP tools are callable. Unknown tool → `UnknownToolCalled` → terminate.
3. **Bounded effects** — `allow_mutations` + `allow_network` booleans plus `allowed_domains` / `blocked_domains`. Each tool has a declared effect class (ADR 0008 table).
4. **Bounded resources** — `max_total_tokens`, `max_tool_calls`, `max_wall_clock`. Breach → `BudgetExceeded { kind }` → terminate.
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

Both are `#[non_exhaustive]`. v0.1.0 node kinds are `agent`, `shell`, `external` (ADRs 0008, 0009; `llm` was collapsed into `agent`; `http`/`parse`/`assert` are not separate kinds — their work happens inside agent tooling). After adding a variant that affects user pipelines, regenerate `schemas/pipeline.schema.json` via `cargo run -p orno-cli -- schema`.

Stream separation in `orno run`:

- **stdout**: `EventEnvelope` NDJSON (consumed by downstream tools).
- **stderr**: `tracing` JSON logs (consumed by log pipelines).

Do not cross the streams. `init_tracing` in `orno-cli/src/main.rs` enforces the split.

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

Never revise an accepted ADR. Add an `## Amendments` section pointing to a newer ADR, or supersede with a new ADR. Historical decisions must remain readable.

## Roadmap and YAML spec

- `docs/roadmap.md` — phased plan for v0.1.0 strict-agentic-MVP (8–10 weeks), deferrals for v0.2.0+.
- `docs/yaml-spec.md` — full v0.1.0 user-facing YAML shape. Examples in `examples/` conform to this shape (not to the current skeleton, which implements a subset).

## Research context

`docs/initial_research.md` (market landscape), `docs/implementation_toolset_research.md` (library selection), and `docs/chat.md` (agentic-loop architecture discussion) predate the ADRs. They are frozen reference documents — when their recommendations were overridden, the override is captured in an ADR (see 0002 for `genai`, 0008 for Architecture A).
