# orno

> Strict agentic loops for CI.

- Documentation: [docs/](./docs)
- Pipeline spec: [docs/yaml-spec.md](./docs/yaml-spec.md)
- Architecture: [docs/arch.md](./docs/arch.md)
- Examples: [examples/](./examples)
- Releases: [github.com/drmozg/orno/releases](https://github.com/drmozg/orno/releases)
- Discussions: [github.com/drmozg/orno/discussions](https://github.com/drmozg/orno/discussions)

Orno is a CI-native runner for strict agentic loops. You describe what an LLM agent may do, what it may spend, and how it reports back — in a single YAML file. Orno holds the agent to every promise at runtime, captures every LLM call and tool call on a replayable event log, and exits with a deterministic code your CI can act on.

Teams adopt orno to run agents in CI without the three classic failure modes: **runaway spend**, **unauthorized blast radius**, and **unreproducible failures**.

---

## Key features

- **Strict by default.** Every agent runs under five runtime-enforced bounds — iteration, tool surface, effects, resources, non-determinism. Cross any one and the agent stops. Your invoice and your blast radius are protected by the same mechanism, not by prompt discipline.

- **Plan before you spend.** `orno plan` previews the full DAG, every declared effect, the tool surface, and a worst-case budget ceiling — without calling an LLM. If a YAML change doubles your cost or widens what an agent can touch, a reviewer sees it before it ships.

- **Replay without spending.** Every run can be captured to a tape. Any tape replays bit-for-bit anywhere — no tokens burned, no network touched, no MCP servers needed. Debugging last Thursday's CI failure becomes a one-line command. Handing a pipeline to security review becomes a file transfer.

- **CI-native from day one.** NDJSON events on stdout, structured logs on stderr, deterministic exit codes, matching wall-clock timestamps on both streams. Orno drops into GitHub Actions, GitLab CI, Buildkite, CircleCI, or any runner that can execute a binary and read an exit code.

- **Model-agnostic.** OpenRouter is the default — one API key unlocks OpenAI, Anthropic, Google, and open-weight models. Direct vendor keys work too. Swap models without touching pipeline shape.

- **Extensible via MCP.** A frozen set of five builtin tools, plus any Model Context Protocol server as the extension seam. GitHub, Slack, Postgres, filesystem, your own internal services — all pluggable without patching orno.

- **Secrets that stay secret.** Two disjoint template namespaces — visible inputs and redacted credentials. Every secret is replaced with `***` in every event, every log line, every tape. Classification follows the name, not the source file.

---

## See it in action

### Automated PR review

A lead reviewer agent delegates to three specialist subagents — security, performance, docs — and synthesizes their findings into a single verdict. None of them can touch your repo.

```yaml
agents:
  pr_reviewer:
    model: anthropic/claude-sonnet-4.5
    provider: openrouter
    system: |
      You are a lead PR reviewer. Delegate to the specialist lenses and
      synthesize their findings as JSON. No prose outside the verdict.
    allowed_tools:
      - "subagent.security_lens"
      - "subagent.performance_lens"
      - "subagent.docs_lens"
    policy:
      max_iterations: 10
      max_total_tokens: 40000
      max_tool_calls: 12
      max_subagent_depth: 1
      allow_mutations: false       # the reviewer cannot modify the repo
      allow_network: false         # the reviewer cannot call out
      on_parse_error: retry_once
```

```bash
PR_NUMBER=482 orno run examples/pr-review.yaml > review.ndjson
```

The parent is structurally incapable of writing a file or reaching the network. Each lens is read-only. Findings come back as structured JSON ready for a PR bot, a dashboard, or a block-on-high-severity gate.

Full pipeline: [`examples/pr-review.yaml`](./examples/pr-review.yaml).

### Flaky-test triage

Point orno at a failing test. It reads the file, runs the test locally multiple times to confirm flakiness, searches your issue tracker for prior reports, optionally adds targeted log lines, and writes a triage report — all inside one tight budget and a strict domain allowlist.

```yaml
agents:
  triager:
    allowed_tools:
      - Read
      - Edit
      - Write
      - Bash
      - WebFetch
      - "mcp.filesystem.*"
      - "mcp.github.search_issues"
    policy:
      max_iterations: 25
      max_total_tokens: 90000
      max_tool_calls: 80
      allow_mutations: true
      allow_network: true
      allowed_domains:
        - api.github.com
        - raw.githubusercontent.com
        - docs.python.org
        - doc.rust-lang.org
```

The agent can write code and hit the network, but only inside the domains you whitelisted. Anything else fails as a tool call, the model sees the failure, and decides what to do next.

Full pipeline: [`examples/flaky-test-triage.yaml`](./examples/flaky-test-triage.yaml).

### Release notes from a commit range

A three-step pipeline: a `shell` step collects the commit range, an enricher agent hits `api.github.com` only, and a synthesizer agent writes `CHANGELOG.md`. Each step has its own policy — the enricher cannot write a file, the synthesizer cannot call the network.

Full pipeline: [`examples/release-notes.yaml`](./examples/release-notes.yaml).

---

## What "strict" means in practice

Every agent runs under five bounds, checked at the executor boundary, at runtime.

| Bound                | What it protects you from                                                        |
| -------------------- | -------------------------------------------------------------------------------- |
| **Iterations**       | Loops that never terminate. No "just one more turn" forever.                     |
| **Tool surface**     | The model inventing a tool and the runtime pretending it exists.                 |
| **Effects**          | Read-only agents mutating files. Networked agents reaching past their allowlist. |
| **Resources**        | Token blowouts, tool-call blowouts, wall-clock blowouts.                         |
| **Non-determinism**  | Runs that can't be reproduced because nothing was recorded.                      |

These are runtime checks, not prompts. The loop terminates or the tool call fails — there is no third outcome.

---

## Getting started

### Install

**From crates.io**

```bash
cargo install orno-cli
```

**From source**

```bash
git clone https://github.com/drmozg/orno
cd orno
cargo install --path crates/orno-cli
```

**Prebuilt binaries**

Linux, macOS (Intel + Apple Silicon), and Windows binaries ship with each tag — [github.com/drmozg/orno/releases](https://github.com/drmozg/orno/releases).

### Your first pipeline

```yaml
# hello.yaml
version: 1

agents:
  greeter:
    model: openai/gpt-5
    provider: openrouter
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
    initial_prompt: "Say hello."
```

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
orno run hello.yaml
```

The agent runs once, cannot call any tool, cannot exceed 1,000 tokens, and emits its decision trail as NDJSON on stdout.

### Plan a run

```bash
orno plan examples/pr-review.yaml
```

```text
Pipeline:        examples/pr-review.yaml
Nodes:           2 (1 shell, 1 agent)
Agents:          4 (pr_reviewer, security_lens, performance_lens, docs_lens)
MCP servers:     filesystem (stdio)
Tool surface:    Read, subagent.{security_lens,performance_lens,docs_lens}, mcp.filesystem.*
Declared effects:
  - pr_reviewer            read-only, no network
  - security_lens          read-only, no network
  - performance_lens       read-only, no network
  - docs_lens              read-only, no network
Budget ceiling (worst case):
  tokens:      120000
  tool calls:  117
  subagent depth: 1
Exit code:     0 (loaded, validated, spendable)
```

Gate YAML changes on `orno plan` in PR CI — no review ships without an explicit diff of effects and budget.

### Record and replay

```bash
# Record a real run once.
orno run examples/pr-review.yaml --record ./ci-repro.ndjson

# Replay anywhere — no tokens, no network, no MCP server.
orno replay ./ci-repro.ndjson
```

---

## CLI at a glance

```text
orno — strict agentic loops for CI

USAGE:
    orno <COMMAND>

COMMANDS:
    run          Execute a pipeline YAML
    plan         Preview a run — DAG, budgets, tool surface, effects — without calling an LLM
    replay       Re-execute from a recorded tape, byte-identically
    validate     Load and validate a pipeline without running it
    schema       Print the pipeline JSON Schema
    completions  Generate shell completions
```

Exit codes: `0` every node passed · `1` pipeline load or infrastructure error · `2` at least one node failed.

---

## Documentation

- **[docs/yaml-spec.md](./docs/yaml-spec.md)** — every field of the pipeline shape, with examples.
- **[docs/arch.md](./docs/arch.md)** — how orno works under the hood.
- **[docs/roadmap.md](./docs/roadmap.md)** — what ships next.
- **[schemas/pipeline.schema.json](./schemas/pipeline.schema.json)** — JSON schema for editor autocomplete:

  ```yaml
  # yaml-language-server: $schema=./schemas/pipeline.schema.json
  ```

---

## Contributing

Issues and pull requests are welcome at [github.com/drmozg/orno](https://github.com/drmozg/orno).

Before opening a PR:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo test --workspace --all-targets
```

---

## License

Orno is dual-licensed.

- **[AGPL-3.0](./LICENSE)** — the default license for open-source use,
  self-hosted CI, and any deployment where the AGPL's source-disclosure
  obligations fit your model.
- **[Commercial license](./LICENSE-COMMERCIAL.md)** — for embedding or
  redistributing orno inside a proprietary product, offering orno as a
  hosted service to third parties, or shipping modifications without the
  AGPL share-alike requirements. Includes a direct support channel.

Commercial licenses are issued by the copyright holder. Contact
**mailbox@sgon.ai** with subject `orno commercial license — <your organization>`
to request a quote.
