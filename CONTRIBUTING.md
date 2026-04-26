# Contributing to orno

orno is a CI-native runner for strict agentic loops. Contributions are welcome, but the project is opinionated about code shape, dependency hygiene, and the runtime contract. This document captures the conventions.

## Before you start

- **Read the docs.** [`docs/what-is-orno.md`](docs/what-is-orno.md) and [`docs/explanation/strict-agentic-loops.md`](docs/explanation/strict-agentic-loops.md) describe the runtime contract — they constrain most design decisions.
- **Open an issue first for non-trivial changes.** New features, new tools, new node kinds, and breaking changes all warrant a discussion before code lands. Bug fixes and doc improvements can come straight in as a PR.
- **Check the [FAQ](docs/faq.md).** Common "why isn't there a *foo*" questions are already answered there.

## Setting up

Build and run the full quality gate:

```bash
git clone https://github.com/drmozg/orno.git
cd orno
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo deny check
cargo machete
typos
cargo doc --workspace --all-features --no-deps
```

If any of those fail on a clean clone, file an issue — that's a setup bug, not a you bug.

The supplemental tools install via:

```bash
cargo install cargo-deny cargo-machete typos-cli
```

The MSRV is 1.95, pinned by `rust-toolchain.toml`. `rustup` will pick it up automatically when you `cd` into the repo.

## The verify gate

After every batch of edits — before opening a PR, before claiming a task is done — run all seven gates:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo deny check
cargo machete
typos
cargo doc --workspace --all-features --no-deps
```

CI runs the same set in parallel jobs. Skipping a gate locally just defers the failure to CI; the gate catches drift the others miss.

If a gate fails on something unrelated to your change (a new advisory on a transitive dep, a typo dictionary update), fix it or add a targeted ignore with a rationale. Never bypass with `--no-verify`, never add a blanket `allow(...)` to silence a clippy lint, and never delete a failing test.

## Code shape

The full conventions live in [`CLAUDE.md`](CLAUDE.md) at the repo root. The highlights:

### Workspace layout

Two crates only — `orno-core` (library) and `orno-cli` (binary). The binary's `clap` and `tokio` dependencies do not live in `orno-core`. Do not split further without a concrete consumer.

### Architectural seams

Seven traits define the seams: `LlmTransport`, `NodeExecutor`, `EventSink`, `Agent`, `ToolHandler`, `McpClient`, plus the `EventEnvelope` wire format. Every executor path routes through one of these. Additions are append-only.

When adding a new variant to `Event`, `NodeKind`, `NodeRequest`, or any error enum, the enum must stay `#[non_exhaustive]`. When adding a new tool, it must implement `ToolHandler` and declare its `ToolEffect`.

### Dependency discipline

- `default-features = false` on `reqwest`, `tokio`, `tracing-subscriber`, `minijinja`. Feature lists are enumerated explicitly in the root `Cargo.toml` under `[workspace.dependencies]`.
- YAML parser: `serde_yaml_ng` only. `serde_yaml` is archived; `serde_yml` carries an unfixed advisory.
- LLM stack: `genai`, accessed only through `LlmTransport`.
- MCP stack: `rmcp`, accessed only through `McpClient`.
- `unsafe_code = "forbid"` at the crate level in both crates.
- `rust-version` in `Cargo.toml` must match `rust-toolchain.toml`'s `channel`. Bumping one requires bumping the other in the same commit.

### Errors

One `thiserror` enum per subsystem. `#[from]` only when the conversion is unambiguous and carries no extra context. Otherwise use named struct variants with `#[source]`. Every public error enum is `#[non_exhaustive]`.

### Tracing

Stream discipline is fixed: stdout = NDJSON events, stderr = tracing JSON. `init_tracing` in `crates/orno-cli/src/main.rs` is the only setup site.

Use structured fields, not format strings: `info!(node.id = %id, attempt = i, "retrying")`, never `info!("retrying node {id}")`. Field names are `snake_case` with dot namespaces matching OpenTelemetry semantic conventions.

No secrets in logs above `debug!`. The redactor is the safety net, but the cleanest fix is to not log the field at all.

### Comments

WHY, not WHAT. `pub` items in `orno-core` carry doc comments. No section-header comments (`// === parsing ===`). No autobiographical comments (`// added to fix bug #47`, `// refactored from X`). Bug context belongs in commit messages, not source.

## Testing

CLI integration tests live in `crates/orno-cli/tests/` and use `assert_cmd` + `predicates`. `tests/cli.rs` is the template — copy its pattern.

Event-stream tests use `insta` YAML snapshots with the standard redaction filters for `run_id` (ULID) and timestamps (RFC 3339).

Parametric tests use `rstest`. Each strictness dimension has at least one test that asserts termination with the exact expected variant.

Hand-rolled fakes, not `mockall`. The seam count is small enough that a fake struct in `mod tests` is clearer than derived mocks.

## Pull requests

### Title

Short, concrete, present tense:

- `pr-reviewer: classify subagent denial as non-terminal`
- `tools: add SetState size cap regression test`
- `docs: clarify replay tape-miss semantics`

Avoid `WIP`, `Fix bug`, `Update`, etc.

### Body

Three sections, brief:

```markdown
## What

One paragraph describing the change.

## Why

The motivation. Link to an issue if there is one.

## Verification

- [x] cargo fmt --all --check
- [x] cargo clippy --workspace --all-targets --all-features -- -D warnings
- [x] cargo test --workspace --all-targets
- [x] cargo deny check
- [x] cargo machete
- [x] typos
- [x] cargo doc --workspace --all-features --no-deps
```

The verification checklist is the load-bearing part. A PR that doesn't list the gates is a PR that didn't run them.

### Scope

One concern per PR. Bug fix + refactor + dep bump in one PR is three PRs. Splitting them lets each be reviewed and reverted independently.

### Commits

Squash-merge is the default. Local commits don't need to follow a convention; the merge commit will carry the PR title.

## Adding a new tool

A new builtin tool requires:

1. A new file under `crates/orno-core/src/tool/<name>.rs`.
2. An `impl ToolHandler` declaring its `ToolEffect`.
3. Tests covering: happy path, bad-arg path, effect-class denial, and (if the tool can fail externally) the failure path.
4. An entry in [`docs/reference/tools.md`](docs/reference/tools.md) with the argument schema and effect class.
5. An entry in the `validate_allowed_tool` enumeration if the tool is callable by name.

Tools that wrap external crates (like `Bash` wrapping `tokio::process`) translate at the trait boundary — the external crate's types do not appear on the public surface.

## Adding a new node kind

A new `NodeKind` variant requires alignment between many places. **Open an issue first.** The runtime contract assumes only `agent` and `shell` are supported, and adding a new kind requires updating the schema, the executor registry, the planner, the dispatcher, the validator, and the docs.

The post-v0.1 path for "subprocess plugins" is a `transport:` axis on the existing kinds, not a new sibling kind. Most "I want a new node kind" requests can be served by an MCP server or a shell node instead.

## Adding a new MCP server feature

The MCP client wraps `rmcp`. A new feature should:

- Be expressible through the `McpClient` trait, or
- Be a new method on the trait if the existing surface is genuinely insufficient.

Don't pass `rmcp::*` types through the public API. The trait isolates orno from `rmcp` version churn — that's the load-bearing rule.

## Documentation changes

`docs/` follows the [Diátaxis](https://diataxis.fr/) framework:

- `docs/tutorials/` — learning-oriented, end-to-end walkthroughs.
- `docs/how-to/` — task-oriented recipes.
- `docs/reference/` — mechanical specs.
- `docs/explanation/` — conceptual rationale.

The lines between these are sharp. A "how-to" that explains why orno is shaped a certain way belongs in `explanation/`. A "tutorial" that's actually a flat reference list belongs in `reference/`.

When adding a new public surface (a flag, a field, a tool), the reference doc must be updated in the same PR. A reviewer should never have to read code to learn the contract.

The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated via `cargo run -p orno-cli -- schema > schemas/pipeline.schema.json`. When the schema and `docs/reference/pipeline-yaml.md` disagree, the schema wins — but the docs should not be wrong.

## License

orno is licensed AGPL-3.0-only. Contributions are accepted under the same license. By submitting a PR you agree your contribution is licensed under AGPL-3.0-only and that you have the right to submit it.

For commercial licensing, see [`LICENSE-COMMERCIAL.md`](LICENSE-COMMERCIAL.md).

## Security

Do not file security-sensitive bugs as public issues. Report them privately:

- Email the maintainer (see `Cargo.toml` `authors`).
- Or open a private security advisory on GitHub.

See [`docs/security.md`](docs/security.md) for the threat model and the kinds of issues that warrant private disclosure.

## Asking questions

- For usage questions, open a discussion on GitHub.
- For bug reports, open an issue with a minimal reproducer (a pipeline YAML + the command line + the actual vs. expected output).
- For feature requests, open an issue describing the use case before the implementation.

The maintainers are pragmatic about scope but opinionated about the contract. A feature that would erode bounded iteration, bounded effects, or replay determinism is unlikely to be accepted regardless of how useful it is — the contract is what makes orno orno.
