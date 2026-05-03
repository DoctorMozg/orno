# orno

<p align="center">
  <img src="docs/images/title.jpg" alt="orno" />
</p>

<p align="center">
  <a href="https://github.com/DoctorMozg/orno/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/DoctorMozg/orno/ci.yml?branch=master&label=CI&logo=github" alt="CI" /></a>
  <a href="https://github.com/DoctorMozg/orno/releases"><img src="https://img.shields.io/github/v/release/DoctorMozg/orno?include_prereleases&sort=semver&logo=github&label=release" alt="Release" /></a>
  <a href="https://github.com/DoctorMozg/orno/blob/master/LICENSE"><img src="https://img.shields.io/github/license/DoctorMozg/orno?color=blue" alt="License: AGPL-3.0" /></a>
  <img src="https://img.shields.io/badge/rust-1.95%2B-orange?logo=rust" alt="Rust 1.95+" />
</p>

One bad prompt and an agent loop in CI will burn a weekend of tokens, hit endpoints it had no business reaching, and leave you nothing to audit afterwards. orno wraps the loop in a runtime contract you declare in YAML — iterations, tool surface, effects, resources — and stops the agent the moment it tries to step outside.

## Why orno

- **The contract lives in YAML.** Set the ceiling once; orno enforces it at runtime. Nothing to wire up in code, no policy library to keep in sync, and no place to quietly disable the limits.
- **One binary, one YAML file.** No server. No database. No scheduler to babysit. Drop the binary on a runner and call it.
- **Two streams, no parser.** NDJSON events on stdout, tracing JSON on stderr. Pipe stdout into `jq`, Splunk, or Datadog as-is; the streams never cross.
- **GitHub Action you pin like any other.** `DoctorMozg/orno@v0.2.0` in the workflow, secrets file alongside, done.

## The five strictness dimensions

Every `agent` node enforces all five at runtime. A breach terminates the node with the corresponding event on the log.

| Dimension                | What it bounds                                  | Config key(s)                                                                                        |
| ------------------------ | ----------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Bounded iteration        | Agent-loop turns                                | `policy.max_iterations`                                                                              |
| Bounded tool surface     | Which tools the model may call                  | `allowed_tools` (builtin names, `mcp.<server>.<tool>`, `subagent.<name>`)                            |
| Bounded effects          | Mutating ops, network calls, domain reach       | `policy.allow_mutations`, `policy.allow_network`, `policy.allowed_domains`, `policy.blocked_domains` |
| Bounded resources        | Total tokens, total tool calls, subagent depth  | `policy.max_total_tokens`, `policy.max_tool_calls`, `policy.max_subagent_depth`                      |
| Bounded non-determinism  | Every LLM call recorded; replay is exact        | `orno run --record-bundle` / `orno replay`                                                           |

Wall-clock deadlines are a node-level attribute (`timeout:`) and apply uniformly to agent and shell nodes.

## Quickstart

### Install

Grab the latest release binary for your platform:

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/DoctorMozg/orno/master/install.sh | bash
```

The script pulls `orno` from GitHub Releases and drops it into `${CARGO_HOME:-$HOME/.cargo}/bin`. Override the destination with `ORNO_INSTALL_DIR=/path/to/bin`, or pin a version with `ORNO_VERSION=v0.2.0`.

Supported targets: `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` (via Git Bash).

### Use as a GitHub Action

Run an orno pipeline from a workflow without bringing your own binary:

```yaml
- uses: DoctorMozg/orno@v0.2.0
  with:
    pipeline: examples/hello/pipeline.yaml
    command: run                # run | plan | validate (default: run)
    secrets-file: .env.secrets  # optional
    args: --record-bundle run.ndjson  # optional, forwarded to orno
```

Pin to a tagged release (`@v0.2.0`) when you want the run reproducible; pin to the major tag (`@v0`) if you'd rather take non-breaking patches as they ship. The action installs orno into the runner's tool cache and runs the pipeline. Stdout is NDJSON and stderr is tracing JSON, the same shape you'd see invoking it locally.

### Build from source

Build against the workspace's pinned toolchain (MSRV 1.95):

```bash
git clone https://github.com/DoctorMozg/orno.git
cd orno
cargo build --release -p orno-cli
./target/release/orno --help
```

### Run an example

`examples/hello/pipeline.yaml` calls a real LLM through OpenRouter. If you don't have an API key handy, point it at the dummy transport. It returns a deterministic canned response and skips the network entirely:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

For a real run:

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
cargo run -p orno-cli -- run examples/hello/pipeline.yaml
```

`examples/hello/pipeline.yaml` in full:

```yaml
version: 1

vars:
  target: README.md

agents:
  greeter:
    model: openai/gpt-5
    provider: openrouter
    system: "You are friendly."
    allowed_tools: []
    policy:
      max_iterations: 1
      max_total_tokens: 1000
      max_tool_calls: 0
      max_subagent_depth: 0
      allow_mutations: false
      allow_network: false
      on_parse_error: fail

nodes:
  - id: greet
    kind: agent
    agent: greeter
    initial_prompt: "Say hello to {{ vars.target }} in one sentence."
```

## Pipeline YAML shape

A pipeline declares `vars`, named `agents`, optional `mcp_servers`, and a list of `nodes` that form a DAG. There are two node kinds. `kind: agent` runs the strict loop against a named agent; downstream nodes read its final assistant message as `nodes.<id>.output`. `kind: shell` is a deterministic subprocess: its output splits into `nodes.<id>.stdout`, `.stderr`, and `.exit_code`, and nothing about it goes through agent policy.

Templates are MiniJinja. They see three namespaces: `vars.*` for declared variables, `env.*` for explicitly opted-in pipeline inputs, and `secrets.*` for credentials that get redacted in logs. The full grammar lives in [`docs/reference/pipeline-yaml.md`](docs/reference/pipeline-yaml.md), and each folder under `examples/` walks through one piece of functionality.

## Commands

- **`orno run <pipeline.yaml>`** — execute a pipeline. NDJSON events to stdout, tracing JSON to stderr.
  Key flags: `-e KEY=VAL`, `--env-file`, `--secrets-file`, `-v` / `--verbose`, `--stderr-tail-bytes`, `--record-bundle`, `--record-tape`, `--replay-tape`, `--record-tool-tape`, `--replay-tool-tape`.
- **`orno validate <pipeline.yaml>`** — load and validate the full policy surface (tool names, agent and MCP references, budget fields).
- **`orno plan <pipeline.yaml>`** — static preview. No LLM or network. Emits `plan_node` and `plan_summary` NDJSON.
- **`orno schema`** — print the pipeline JSON Schema to stdout. Used to regenerate `schemas/pipeline.schema.json`.
- **`orno completions <shell>`** — emit shell completions (bash, zsh, fish, elvish, powershell).

`orno run` keeps the streams apart: NDJSON event envelopes go to stdout, tracing JSON to stderr. Both carry RFC 3339 UTC timestamps, so joining the two on wall clock is a one-liner. Exit code is `0` on success, non-zero if the pipeline failed to load or any node failed.

## Documentation

Documentation lives under [`docs/`](docs/README.md):

- [What is orno](docs/what-is-orno.md) — the runtime contract, in plain English.
- [Install](docs/install.md) — building, running, and verifying a setup.
- Tutorials: [Your first pipeline](docs/tutorials/first-pipeline.md), [Record and replay](docs/tutorials/record-replay.md), [Multi-agent PR review](docs/tutorials/multi-agent-pr-review.md).
- How-to: [MCP servers](docs/how-to/add-mcp-server.md), [Secrets](docs/how-to/pass-secrets.md), [Scoped state](docs/how-to/scope-state-across-nodes.md), [Budget](docs/how-to/tighten-budget.md), [Debugging](docs/how-to/debug-failure.md).
- Reference: [CLI](docs/reference/cli.md), [Pipeline YAML](docs/reference/pipeline-yaml.md), [Tools](docs/reference/tools.md), [Events](docs/reference/events.md), [Errors](docs/reference/errors.md), [Environment variables](docs/reference/env-vars.md), [Exit codes](docs/reference/exit-codes.md).
- Explanation: [Strict agentic loops](docs/explanation/strict-agentic-loops.md), [Comparisons](docs/explanation/comparisons.md), [Security](docs/security.md).
- [Glossary](docs/glossary.md) and [FAQ](docs/faq.md).

Browsable, runnable example pipelines live in [`examples/`](examples/README.md), one folder per example.

## Changelog

See [`CHANGELOG.md`](CHANGELOG.md) for notable changes between releases.

## License

AGPL-3.0-only. See `Cargo.toml` for the canonical SPDX identifier.
