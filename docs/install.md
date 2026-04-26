# Install

orno is a Rust workspace with two crates: `orno-core` (library) and `orno-cli` (the `orno` binary). The CLI is the user-facing entry point.

## Prerequisites

- A Rust toolchain at MSRV 1.95 or later. The repo pins the channel via `rust-toolchain.toml`, so a `rustup`-managed install picks the right toolchain automatically once you `cd` into the workspace.
- A working `git` for examples that consume git history (`commit-digest`, `release-notes`, `pr-review`).
- Optional: `npx` (Node) for the stdio MCP examples that spawn `@modelcontextprotocol/server-filesystem` or `@modelcontextprotocol/server-github`.
- Optional: an `OPENROUTER_API_KEY` if you want to run examples against a real LLM.

## Build from source

```bash
git clone https://github.com/<owner>/orno.git
cd orno
cargo build --release -p orno-cli
./target/release/orno --help
```

For development you can run the binary through cargo without building a release artifact:

```bash
cargo run -p orno-cli -- --help
cargo run -p orno-cli -- validate examples/hello/pipeline.yaml
```

## Verify the install

A clean working tree should pass orno's full quality gate:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo deny check
cargo machete
typos
cargo doc --workspace --all-features --no-deps
```

The supplemental tools (`cargo deny`, `cargo machete`, `typos`) are not required to use orno — they are required to *contribute* to it. See the repo-root `CONTRIBUTING.md` once it lands.

## Smoke-test against the dummy transport

`examples/hello/pipeline.yaml` calls a real LLM via OpenRouter, but you can exercise it against a deterministic dummy transport without needing a key:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

You should see NDJSON event envelopes on stdout terminating in a `run_finished` envelope with `"ok": true`.

## Smoke-test against a real LLM

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
cargo run -p orno-cli -- run examples/hello/pipeline.yaml --secrets-file .env.secrets
```

The OpenRouter key is auto-discovered when an agent's `provider:` is `openrouter`. orno will redact the key from every event body and tracing line.

## Set up shell completions

```bash
orno completions bash > /etc/bash_completion.d/orno
orno completions zsh  > "${fpath[1]}/_orno"
orno completions fish > ~/.config/fish/completions/orno.fish
```

The `completions` subcommand also supports `elvish` and `powershell`.

## Where to go next

- [What is orno](what-is-orno.md) — the runtime contract, in plain English.
- [Pipeline YAML grammar](yaml-spec.md) — every field of every block.
- The per-example READMEs under [`../examples/`](../examples/README.md) — runnable shapes to copy from.
