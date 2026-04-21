# Orno Architecture

This document synthesizes the 16 accepted and proposed ADRs into a
single readable whole. It is the orientation read for someone new to
the project; ADRs carry the load-bearing arguments and remain
authoritative for individual decisions.

- User-facing YAML shape: `docs/yaml-spec.md`.
- Phased build plan: `docs/roadmap.md`.
- Operator guidance for Claude Code sessions: `CLAUDE.md`.
- ADR detail: `docs/adr/NNNN-*.md`.

## What orno is

Orno is a CI-native runner for **strict agentic loops**. A pipeline is
a YAML DAG of nodes; some nodes are agents (run an LLM loop with
tools), some are shells, one reserved kind is subprocess plugins.
"Multi-agent" in orno means **recursive single-agent loops** where a
parent agent treats a child agent as a tool call, not peer-to-peer
messaging between cooperating agents (ADR 0006). This is Claude Code's
shape, not CrewAI's.

The product claim is *strictness*. Every agent loop runs under five
runtime-enforced bounds (ADR 0005). Every LLM call and tool call is a
typed event on a bounded, versioned log (ADRs 0003, 0012). Replay is
bit-for-bit given the recorded transport tape. Without those
guarantees, orno is a boring YAML DAG runner; with them, it is
auditable enough to hold CI credentials and run unsupervised.

## Design premises

Three premises drove the shape of the system; every ADR serves at
least one of them.

1. **Strictness is a user-facing guarantee, not prompt discipline.**
   The five bounds (ADR 0005) are enforced at the executor boundary.
   An agent that calls an undeclared tool terminates; a pipeline
   that breaches a token budget terminates; a read-only agent that
   tries to mutate receives a tool-call failure. Every violation is
   a typed event. Enforcement modes (ADR 0016) add a declared
   hardening trajectory for softer dimensions, but the three
   load-bearing dimensions (iteration, tool surface, resources) are
   locked at hard-fail and cannot be downgraded by user config.

2. **Auditability over extensibility.** The builtin tool set is
   fixed — `Bash`, `Read`, `Edit`, `Write`, `WebFetch` — and extended
   only through MCP (ADR 0008). Users do not author tool JSON
   schemas; whole-node subprocess plugins stay behind
   `--unstable-plugins` until a stabilization ADR lands (ADRs 0004,
   0014). Every tool has a static effect class; security review of
   a pipeline reads the YAML, not the code under each tool.

3. **Record and replay is the moat.** Every LLM request and every
   tool call produces a `#[non_exhaustive]` typed `Event` routed
   through an `EventSink` over a bounded broadcast channel (ADRs
   0003, 0012). `LlmTransport` (ADR 0002) is the single
   non-determinism seam and is recorded by definition. A recorded
   run replays without networks, without tokens, without an MCP
   server running — same seq stream, same decisions, same exit
   code.

These three premises are why orno's architecture looks conservative in
places where other agentic frameworks look flexible. The flexibility
is gated to keep the guarantees real.

## Runtime model

A pipeline file (`examples/pr-review.yaml`, etc.) declares:

- `version: 1` — schema version.
- `vars:` — template variables.
- `agents:` — named `AgentConfig`s (model, provider, system prompt,
  `allowed_tools`, `AgentPolicy`).
- `mcp_servers:` — MCP server declarations, lifecycle-managed by the
  run (ADR 0007).
- `nodes:` — the DAG. Each node has `id`, `kind`, optional `needs`.

Three node kinds are defined; two are user-facing in v0.1.0.

- **`agent`** — runs the strict loop from ADR 0005. Every LLM-facing
  work lives here; single-shot completion is the degenerate case
  (`max_iterations: 1`, `allowed_tools: []`). ADR 0009 collapsed the
  old separate `llm` kind into `agent`.
- **`shell`** — deterministic subprocess. Declares effects explicitly
  (network, fs, env passthrough, domain rules) — ADR 0013 — so SRE can
  deny pipelines by effect class without reading each command.
- **`external`** — reserved slot for subprocess plugins (ADR 0004).
  Accepting `kind: external` YAML requires `--unstable-plugins` until
  the stabilization ADR lands (ADR 0014).

Every node produces `NodeResult { status: Ok | Failed, output: String }`
(ADR 0010). Shell status comes from exit code; agent status comes from
policy violations or a `"status": "fail"` field in the final JSON
message. `orno run` exits `0` on all-pass, `2` if any uncovered node
failed, `1` on load/infra error. Per-node `continue_on_error: true`
opts out of failure propagation.

Stream discipline in `orno run`:

- **stdout** — `EventEnvelope` NDJSON for downstream tool consumption.
- **stderr** — `tracing` JSON for log pipelines.

These streams never cross; `init_tracing` in `orno-cli/src/main.rs`
enforces the split.

## The five strictness dimensions

Every agent node runs under all five bounds (ADR 0005). Each carries a
declared enforcement mode from ADR 0016; the defaults below are
v0.1 defaults.

| # | Dimension                | v0.1 default | Violation event                                | Response       |
| - | ------------------------ | ------------ | ---------------------------------------------- | -------------- |
| 1 | Bounded iteration        | hard-fail    | `IterationLimitExceeded { iteration, limit }`  | terminate node |
| 2 | Bounded tool surface     | hard-fail    | `UnknownToolCalled { name }`                   | terminate node |
| 3 | Bounded effects          | tool-fail    | `MutatingCallBlocked` / `NetworkBlocked` / `DomainBlocked` | tool-call failure (model may recover) |
| 4 | Bounded resources        | hard-fail    | `BudgetExceeded { kind: Tokens \| ToolCalls \| WallClock }` | terminate node |
| 5 | Bounded non-determinism  | recorded     | —                                              | every call is on the event log; replay reproduces bit-for-bit |

Dimensions 1, 2, and 4 are the load-bearing guarantees. ADR 0016
forbids user config from downgrading them below hard-fail; attempts
emit `PipelineError::StrictnessLocked` at validation. Dimension 3
deliberately fails tool-call-only so the model observes its failure
and may recover. Dimension 5 is observational, not enforcement.

Parallel tool calls returned by a single model turn are executed
**serially** in declaration order. Agent-internal parallelism is out
of scope; parallelism is a DAG-level concern.

## Tools and effects

The builtin tool set is frozen at five entries (ADR 0008):

| Tool       | Effect class          | Requires                                    |
| ---------- | --------------------- | ------------------------------------------- |
| `Read`     | local_read            | —                                           |
| `Edit`     | local_write           | `allow_mutations: true`                     |
| `Write`    | local_write           | `allow_mutations: true`                     |
| `Bash`     | shell (mut + net)     | both `allow_mutations` and `allow_network`  |
| `WebFetch` | network_read          | `allow_network: true` + domain rules        |
| `mcp.<server>.<tool>` | declared by server | matches the advertised effect      |

`allowed_tools` grammar:

- A builtin name (no wildcards).
- `mcp.<server>.<tool>` — a specific MCP tool.
- `mcp.<server>.*` — every tool the server advertises.
- `subagent.<agent-name>` — expose a child agent as a tool call.

YAML uses dots as separators; provider function-calling schemas
usually disallow dots in tool names. Orno rewrites dots to underscores
on the wire (`mcp.github.search_issues` → `mcp_github_search_issues`;
`subagent.security_lens` → `subagent_security_lens`). Validation and
event messages use the dotted form.

The effect model is two booleans plus two domain lists: `allow_mutations`,
`allow_network`, `allowed_domains`, `blocked_domains`. Blocklist wins
on overlap. Shell nodes carry the same concepts in their `effects:`
block (ADR 0013) — `network: bool`, `fs: read-only|read-write|none`,
`env_passthrough: [NAME, ...]`, and the same domain rules.

## Subagents (recursive, not peer-to-peer)

A parent agent exposes a child as a tool by listing
`subagent.<agent-name>` in its `allowed_tools` (ADR 0006). The tool
takes `{ prompt: string }`; the parent-emitted prompt becomes the
child's `initial_prompt` for a fresh loop. Recursion is bounded by
`max_subagent_depth`.

Composition rules:

- Child budget per resource = `min(remaining_parent_budget, child_policy_budget)`.
- Child `allow_mutations` / `allow_network` cannot be less strict
  than the parent's. A read-only parent cannot delegate to a
  mutating child. Enforced at pipeline load.
- Errors surface up as tool-call failure strings; the parent decides
  whether to retry, branch, or give up.

There is no peer-to-peer messaging, no shared blackboard, no actor
system, no channels between agents. "Multi-agent" behavior emerges
from trait composition, not concurrency primitives. Parallelism lives
at the DAG scheduler; recursion lives in `SubagentHandler`.

## MCP

MCP (Model Context Protocol) is the only extension seam for tools.
`rmcp` is the concrete client, wrapped behind `trait McpClient` in
`orno-core` with a minimal surface (`initialize`, `list_tools`,
`call_tool`, `shutdown`). No `rmcp` types escape the trait; swapping
implementations touches one file (ADR 0007).

Servers are lifecycle-managed by the **run**, not by individual nodes:

- Top-level `mcp_servers:` block declares servers (stdio or http
  transport).
- At run start, each declared server is spawned, the MCP handshake
  runs, `tools/list` is called, and schemas are cached.
- At run end (success, failure, cancellation), servers shut down
  cleanly — `notifications/exit` for stdio, connection close for
  http, SIGTERM fallback after timeout.
- Server crash mid-run terminates the owning agent with a
  `McpServerCrashed` tool-call failure. Restart policies are
  explicitly deferred.

Agents opt in to specific servers and tools via their `allowed_tools`
list. No auto-discovery; auditability requires listing.

## Pipeline ergonomics

Prompts do not have to live inline in YAML. MiniJinja's `{% include %}`
directive reads a file and renders it as a template in the current
context (ADR 0011):

```yaml
agents:
  pr_reviewer:
    system: "{% include 'prompts/reviewer-system.md' %}"
```

The template loader is rooted at the pipeline YAML's directory. Paths
that escape the tree (absolute paths, `..` climbs, out-of-tree
symlinks) fail pipeline load. Included files are themselves templates,
so shared preambles compose via nested includes; MiniJinja detects
circular includes and errors cleanly. All template resolution happens
at pipeline load — `orno validate` catches missing files before any
network call.

`vars`, `env`, `secrets`, and `nodes.<id>.output` / `nodes.<id>.status`
are in template context. Auto-inferring `needs:` from template
references is explicitly not a v0.1 feature; declare `needs:` whether
the reference is inline or in an include.

## Event log and replay

Events are produced by nodes, sent through an `mpsc` channel to an
event-log actor, and fanned out to `broadcast` subscribers that
implement `EventSink` (ADRs 0003, 0012).

- `EventEnvelope { schema_version: u32, seq: u64, event: Event }` is
  the wire format. `Event` is `#[serde(tag = "type")]` and
  `#[non_exhaustive]` — additions are append-only; existing replays
  stay readable.
- The `mpsc` producer side uses `send().await` (block-on-full), so
  slow sinks create visible stalls instead of silent drops.
- The `broadcast` side is bounded with configurable capacity
  (`--event-channel-capacity`, default 1024). On
  `RecvError::Lagged(n)`, the subscriber wrapper emits
  `EventSinkBehind { sink_id, last_delivered_seq, dropped_count, first_dropped_seq }`
  so replay consumers observe gaps explicitly and may refuse to
  replay gapped streams.
- `--sink-max-lag-ms T` turns excessive lag into a warn-level tracing
  event and a non-zero exit code — CI-friendly detection of stuck
  sinks.

`InMemorySink` is the only `EventSink` in v0.1.0. A feature-gated
`SqliteSink` is the expected landing for durable persistence (ADR
0015 keeps this as a module inside `orno-core`, not a new crate).

Replay decorates the `LlmTransport` seam: `RecordingTransport<T>`
captures calls; `ReplayTransport` replays them from the recorded tape
without a network. An integration test runs any real provider once,
commits the replay NDJSON, and re-runs deterministically in CI.

## The seven architectural seams

Every executor path routes through one of seven traits. Nothing
side-steps them; additions are append-only.

| # | Trait           | Role                                        | ADR        | v0.1.0 impls                                            |
| - | --------------- | ------------------------------------------- | ---------- | ------------------------------------------------------- |
| 1 | `LlmTransport`  | every LLM call                              | 0002       | `DummyTransport`, `GenAiTransport`, future `Recording`/`Replay` |
| 2 | `NodeExecutor`  | every node kind                             | 0003, 0004 | `AgentExecutor`, `ShellExecutor`, (`ExternalExecutor` deferred) |
| 3 | `EventSink`     | every lifecycle event                       | 0003, 0012 | `InMemorySink`; feature-gated `SqliteSink` follow-up    |
| 4 | `EventEnvelope` | versioned wire format (not a trait proper)  | 0003       | one concrete struct; `#[non_exhaustive]` `Event` enum   |
| 5 | `Agent`         | agent-loop trait                            | 0006       | `LoopAgent` implements the five dimensions              |
| 6 | `ToolHandler`   | one impl per builtin + dispatch for MCP/subagent | 0008  | `BashHandler`, `ReadHandler`, `EditHandler`, `WriteHandler`, `WebFetchHandler`, `McpHandler`, `SubagentHandler` |
| 7 | `McpClient`     | MCP protocol client                         | 0007       | `RmcpClient` wrapping `rmcp`                            |

Seam count grew from four (ADR 0003's original set) to seven through
ADRs 0005–0008 as the strict-loop model landed. The discipline is
unchanged — every executor routes through a trait, the event log is
the record/replay seam, `#[non_exhaustive]` governs the wire enum.

## Workspace shape

Two crates from commit 1 (ADR 0001):

- **`crates/orno-core`** — library. Pipeline schema, agent loop,
  `NodeExecutor`, `LlmTransport`, `ToolHandler`, `McpClient`, event
  log, budget, telemetry. No `clap` or binary `tokio` footprint.
- **`crates/orno-cli`** — binary (`orno`). `clap` subcommand dispatch,
  output formatting, stream separation. Depends on `orno-core` only.

The crate-budget rule (ADR 0015) requires any new workspace crate to
cite one of three justifications in an ADR: a named second consumer,
a measured ≥15 s clean-build-time win on CI, or a security boundary
that cannot be enforced inside a single crate. "Hypothetical plugin
authors" and logical layering do not qualify. Short-term pressure to
split is resolved through module-level refactors inside `orno-core`.

## What orno does not ship in v0.1.0

Consolidated deferrals from across the ADR set. Many of these have a
target phase in `docs/roadmap.md`; some are explicitly post-v0.1.

- `WebSearch` tool — needs a `SearchProvider` trait plus Tavily/Brave
  implementations (ADR 0008).
- Generic `HttpHandler` — users bring an MCP server or shell out via
  Bash (ADR 0008).
- User-authored tool JSON schemas — Architecture A is rejected for
  v0.1; MCP is the extension seam (ADR 0008).
- `kind: external` subprocess plugins — behind `--unstable-plugins`
  only until the stabilization ADR lands (ADRs 0004, 0014).
- `SqliteSink` — planned as a feature-gated module inside
  `orno-core`; `InMemorySink` only ships (ADRs 0003, 0015).
- MCP server restart policies — server crash terminates the owning
  agent; restart deferred (ADR 0007).
- Streaming LLM responses — the `LlmTransport::stream` method exists
  in the trait but no executor consumes it yet (ADR 0002).
- Inline agent config at the node level — every agent config lives
  under `agents.*` in v0.1 (ADR 0009).
- `when: "<jinja>"` conditional node execution — adds predicate
  evaluator and DAG-validation surface; deferred to post-v0.1
  alongside scheduler work (ADR 0010).
- Cross-project shared prompt libraries — vendor `prompts/` into each
  consuming repo; a `--include-dir` flag is a post-v0.1 concern
  (ADR 0011).
- Observed shell-effects enforcement — v0.1 ships declared-only;
  observation (nsjail, landlock, unshare) is a follow-up ADR with a
  warn → soft-fail → hard-fail trajectory (ADRs 0013, 0016).
- WASM plugins — explicitly out of scope for v1.x (ADR 0004).
- Auto-inferred `needs:` from template references — declare
  explicitly (`docs/yaml-spec.md`).

## ADR index

| ADR  | Status   | Decision                                                   |
| ---- | -------- | ---------------------------------------------------------- |
| 0001 | accepted | Two-crate workspace: `orno-core` + `orno-cli`              |
| 0002 | accepted | LLM client via `genai`, wrapped behind `LlmTransport`      |
| 0003 | accepted | Typed event log and four original trait seams; extended by 0005–0008 |
| 0004 | accepted | Defer whole-node plugin protocol; wire format reserved     |
| 0005 | accepted | Strict agentic loops — five strictness dimensions          |
| 0006 | accepted | Subagent-as-tool-call; no peer-to-peer multi-agent         |
| 0007 | accepted | MCP via `rmcp`, wrapped behind `McpClient`                 |
| 0008 | accepted | Fixed builtin tool set; MCP is the extension seam          |
| 0009 | accepted | Collapse `llm` into `agent`; single LLM-facing node kind   |
| 0010 | accepted | Typed `NodeResult { status, output }` and CI exit codes    |
| 0011 | accepted | Prompt composition via MiniJinja `{% include %}`           |
| 0012 | proposed | Bounded event log with explicit backpressure (amends 0003) |
| 0013 | proposed | Shell nodes declare effects (extends 0005)                 |
| 0014 | proposed | `NodeKind::External` gated behind `--unstable-plugins`     |
| 0015 | proposed | Crate-budget rule — three justifications for any new crate |
| 0016 | proposed | Per-dimension enforcement modes with declared trajectory   |

Never revise an accepted ADR. Add an `## Amendments` section pointing
to a newer ADR, or supersede with a new ADR. Historical decisions must
remain readable.
