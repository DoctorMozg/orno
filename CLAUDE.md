# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## About orno

Orno is a CI-native multi-agent orchestrator (Rust workspace) in pre-v0.1 skeleton state. Trait seams and module tree are in place; most node executors, LLM transport wiring, scheduler parallelism, budget enforcement, and record/replay are intentionally unimplemented. Read `docs/adr/` before making architectural changes — the four committed ADRs constrain most design decisions.

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

- `crates/orno-core/` — library. Pipeline schema, node trait, LLM transport trait, event log, execution engine, config, budget, telemetry. The binary's `clap` and `tokio` dependencies do NOT live here.
- `crates/orno-cli/` — binary (`orno`). Subcommand dispatch, output formatting, clap derive. Depends on `orno-core` only.

Do not split further without a concrete consumer or a build-parallelism justification.

## The four architectural seams

These exist today as traits with dummy or no-op impls. All future work flows through them rather than around:

1. **`LlmTransport`** (`orno-core/src/llm/mod.rs`) — every LLM call goes through this trait. Concrete impl will wrap `genai` (ADR 0002); record/replay will land as a decorator. Do NOT expose `genai` types on orno's public surface.
2. **`NodeExecutor`** (`orno-core/src/node/mod.rs`) — every node kind implements this trait. Subprocess plugins (ADR 0004) will implement the same trait via `NodeKind::External`.
3. **`EventSink`** (`orno-core/src/events/sink.rs`) — every lifecycle event is routed through here. `InMemorySink` is the only impl today; a feature-gated `SqliteSink` will land without touching the scheduler.
4. **`EventEnvelope { schema_version, seq, event }`** with `#[non_exhaustive]` on `Event` — the wire format is versioned. Adding variants is append-only; changing semantics bumps `CURRENT_SCHEMA_VERSION` in `events/mod.rs`.

## Pipeline wire format

User YAML is defined by `orno_core::pipeline::Pipeline`. Two distinct tag conventions are intentional:

- `Event` uses `#[serde(tag = "type")]` — lifecycle events.
- `NodeKind` / `NodeRequest` use `#[serde(tag = "kind")]` — pipeline node discriminator.

Both are `#[non_exhaustive]`. After adding a variant that affects user pipelines, regenerate `schemas/pipeline.schema.json` via `cargo run -p orno-cli -- schema`.

Stream separation in `orno run`:

- **stdout**: `EventEnvelope` NDJSON (consumed by downstream tools).
- **stderr**: `tracing` JSON logs (consumed by log pipelines).

Do not cross the streams. `init_tracing` in `orno-cli/src/main.rs` enforces the split.

## Dependency discipline

Set on day 1, must be preserved:

- `default-features = false` on `reqwest`, `tokio`, `tracing-subscriber`, `minijinja`, `figment`. Feature lists are enumerated explicitly in root `Cargo.toml` under `[workspace.dependencies]`. Do not add `tokio` with `features = ["full"]`.
- YAML parser: **`serde_yaml_ng`** only. `serde_yaml` is archived; `serde_yml` carries RUSTSEC-2025-0068 and must never enter the tree.
- LLM stack: `genai` (ADR 0002), accessed only through `LlmTransport`.
- `unsafe_code = "forbid"` at the crate level in both crates.

Pedantic clippy is `warn` in both crates with a small documented allow list. When a new pedantic lint fires on intentional design, add a targeted allow with a one-line rationale in the same `[lints.clippy]` block rather than suppressing inline.

## Error conventions

One `thiserror` enum per subsystem (`CoreError`, `PipelineError`, `NodeError`, `LlmError`) in `orno-core/src/error.rs`. `#[from]` only when the conversion is unambiguous and the variant carries no extra context; otherwise use an explicit struct variant with `#[source]`. `anyhow::Result` is used exclusively in `orno-cli` at dispatch boundaries.

## ADRs

- `docs/adr/0001-workspace-split.md` — two-crate split rationale.
- `docs/adr/0002-llm-client-genai.md` — why `genai` wrapped behind `LlmTransport` (amends the research's hand-rolled recommendation).
- `docs/adr/0003-event-log-from-day-one.md` — the four trait seams and why they exist before any impl needs them.
- `docs/adr/0004-defer-plugin-protocol.md` — no plugin loader until post-v0.1; wire format frozen now.

Add ADR 0005+ (never edit older ones) when making a structural decision that a future reader would need to justify.

## Research context

`docs/initial_research.md` (market landscape) and `docs/implementation_toolset_research.md` (library selection) predate the skeleton. They are frozen reference documents — when their recommendations were overridden, the override is captured in an ADR (see 0002).
