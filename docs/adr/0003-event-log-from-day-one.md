# ADR 0003 — Typed event log and four trait seams from day one

- Status: accepted; seam set extended by ADRs 0005–0008
- Date: 2026-04-21

## Context

Orno's evolution path is "thin executor today, durable record/replay engine
later." Retrofitting an event log into a system that streams directly to
stdout is painful — every node executor needs to be touched. Retrofitting
durable state into a log-as-strings pipeline is worse. The research (§3) is
unambiguous: "encode pipeline execution as a typed event log from day one."

## Decision

From the skeleton onward, four architectural seams exist and are respected
by every node executor and scheduler:

1. **`trait LlmTransport`** — LLM calls go through this, never direct HTTP.
   Enables record/replay as a decorator pattern (`RecordingTransport<T>`,
   `ReplayTransport`).
2. **`trait EventSink`** — event log subscribers implement this. Default is
   `InMemorySink`. A future `SqliteSink` behind a `sqlite` feature flag
   plugs in without scheduler changes.
3. **`NodeKind::External { command, args }`** — enum variant declared in the
   node registry from day one, no implementation. Subprocess plugins plug
   into the existing `NodeExecutor` trait when the protocol is designed
   post-v0.1.
4. **`EventEnvelope { schema_version: u32, seq: u64, event: Event }`** with
   `#[non_exhaustive]` on `Event` — the on-the-wire format is versioned
   from v0 so the enum can grow without breaking replays.

The event log is an actor: nodes send events into an `mpsc` channel, the
actor fans out to `broadcast` subscribers. Subscribers never block
producers. The scheduler itself is a subscriber for lifecycle
decisions.

## Consequences

- v0.1.0 has `InMemorySink` only — the scheduler and node code look the
  same today as they will after SQLite durability lands.
- Event additions are append-only on the enum with a `schema_version` bump
  when semantics change; existing replay files stay readable.
- Direct println/eprintln inside node executors is a code smell — log via
  the event bus instead. CLI-facing output formatters subscribe to the
  bus.
- The `RecordingTransport<T>` pattern means an integration test can run any
  real provider once, commit the replay NDJSON, and re-run deterministically
  in CI with zero network.

## Amendments

ADRs 0005–0008 extend the seam set and the `Event` enum; ADR 0018
extends the envelope shape. None revise any of the four original seams.

- **ADR 0005** (strict agentic loops) adds event variants for each
  strictness violation (`IterationLimitExceeded`,
  `BudgetExceeded { kind }`, `UnknownToolCalled`,
  `MutatingCallBlocked`, `NetworkBlocked`, `DomainBlocked`) and the
  loop-progress events (`LlmRequestStarted`, `LlmResponseReceived`,
  `ToolCallStarted`, `ToolCallCompleted`, `ToolCallFailed`,
  `AgentCompleted`).
- **ADR 0006** (subagent as tool-call) introduces `trait Agent` as
  a fifth seam and adds `SubagentStarted`, `SubagentCompleted`,
  `SubagentFailed`, `SubagentDepthExceeded`.
- **ADR 0007** (MCP via rmcp) introduces `trait McpClient` as a
  sixth seam and adds the `McpServer*` and `McpToolCall*` variants.
- **ADR 0008** (builtin tool set) introduces `trait ToolHandler` as
  a seventh seam. Concrete impls: `BashHandler`, `ReadHandler`,
  `EditHandler`, `WriteHandler`, `WebFetchHandler`, `McpHandler`,
  `SubagentHandler`.
- **ADR 0018** (event envelope timestamp) adds a required RFC 3339
  `timestamp: OffsetDateTime` field to `EventEnvelope`, alongside
  `schema_version` and `seq`. `seq` remains the determinism-load-bearing
  ordering key; `timestamp` is a human-readable correlator that makes
  the stdout event stream and stderr tracing stream joinable on wall
  clock.

Seam count: four → seven. The discipline is unchanged — every
executor routes through a trait, the event log is the record/replay
seam, and `#[non_exhaustive]` still governs the on-the-wire enum.
