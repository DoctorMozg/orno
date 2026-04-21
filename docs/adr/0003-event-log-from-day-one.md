# ADR 0003 — Typed event log and four trait seams from day one

- Status: accepted
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
