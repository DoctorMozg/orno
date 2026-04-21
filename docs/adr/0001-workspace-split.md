# ADR 0001 — Two-crate workspace from day one

- Status: accepted
- Date: 2026-04-21

## Context

Rust projects hit friction at roughly 8k LOC inside a single crate: compile
times, feature-flag cross-talk, and the "should this type live in bin or lib?"
question. The inverse failure mode is premature workspace fragmentation — uv's
60-crate tree only earns its keep because Astral has the headcount to manage
it. `jj-vcs` is the closest architectural analog to orno (~50–100k LOC, a
single contributor historically) and ships exactly two crates: `jj-lib` and
`jj-cli`.

## Decision

Ship orno as a Cargo workspace with two member crates from commit 1:

- `crates/orno-core` — library. Everything reusable: config loading, pipeline
  schema, execution engine, node trait, LLM transport, event log, budget
  enforcer.
- `crates/orno-cli` — binary. `clap` parsing, output formatting, subcommand
  dispatch. Re-exports nothing. Depends on `orno-core` only.

Further splitting (e.g. carving out `orno-sqlite` behind a feature flag)
happens only when a subtree has a demonstrated independent consumer or a
build-parallelism bottleneck.

## Consequences

- The binary's `tokio` and `clap` footprint stays out of any downstream
  library embedder's dependency graph.
- Clean test boundary: CLI integration tests live in `orno-cli/tests`,
  library tests colocated with source under `orno-core/src`.
- Workspace-level `[workspace.dependencies]` is the single source of truth
  for crate versions, avoiding skew.
- Two `Cargo.toml` files instead of one — a small tax we pay for the
  clean boundary.
