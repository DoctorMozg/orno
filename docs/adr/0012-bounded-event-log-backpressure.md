# ADR 0012 — Bounded event log with explicit backpressure

- Status: proposed; amends ADR 0003
- Date: 2026-04-21

## Context

ADR 0003 establishes the event log as
`producer → mpsc → broadcast → subscribers`, with subscribers that
"never block producers." The shape leaves two failure modes undeclared:

1. `tokio::sync::broadcast` is a bounded ring. A slow subscriber yields
   `RecvError::Lagged(n)` and **drops events** — the receiver's view
   has gaps. This violates the `seq`-ordering contract that ADR 0003's
   replay story depends on: a replay built from a gapped subscriber is
   not a replay, it's a fiction.
2. The `mpsc` side has no explicit bound and no dead-letter path. A
   future `SqliteSink` with a stalled fsync looks, to an operator,
   identical to a hung pipeline — no `EventSinkBehind` event, no drop
   metric, no timeout signal.

The brainstorm on 2026-04-21
(`.mz/reports/brainstorm_2026_04_21_orno_strengths.md`, winning idea)
surfaced both holes as correctness bugs in the replay substrate:
ADR 0005's strictness claim cannot be honored on a lossy event log.

## Decision

1. **Bounded broadcast with explicit capacity.** Default `1024`,
   override via `--event-channel-capacity N`. Capacity is a CLI knob,
   not a silent compile-time constant.
2. **`EventSinkBehind { sink_id, last_delivered_seq, dropped_count,
   first_dropped_seq }`** becomes a first-class `Event` variant. On
   `RecvError::Lagged(n)` the per-subscriber wrapper emits this event
   through its sink before continuing, so downstream tools observe the
   gap rather than infer it from missing seqs.
3. **Bounded `mpsc` with block-on-full producer.** Producer-side
   backpressure (`send().await` blocks) replaces any `try_send`-plus-
   drop pattern. A slow sink stalls the pipeline — the right trade for
   an audit/replay tool where silent loss is worse than slow runs.
4. **`--sink-max-lag-ms T`** CLI flag: any sink lagging beyond `T`
   produces a warn-level tracing event and causes a non-zero exit code
   after run completion. Default: unset; recommended CI setting
   documented in `docs/roadmap.md`.
5. **Acceptance test.** A stress harness at 1k events/sec with a
   1-second-stalled sink is part of the v0.1 test suite. It must
   either emit `EventSinkBehind` with accurate counts or block
   producers — never silently drop.

## Consequences

- Replay consumers observe gaps explicitly and can refuse replay on
  gapped streams; the replay-as-moat story survives.
- Slow sinks create visible pipeline stalls instead of silent
  corruption. On-call has a discriminator between "pipeline hung" and
  "sink stuck."
- The event enum grows by one variant (`EventSinkBehind`) —
  append-only via `#[non_exhaustive]` (ADR 0003), no
  `CURRENT_SCHEMA_VERSION` bump required.
- Direct `producer.send()` inside the scheduler migrates to the
  bounded-async form; the change is contained to the event-actor in
  `orno-core/src/events/`.
- `try_send`-and-drop is banned in the event path. Reviewers enforce
  block-on-full manually until a clippy lint covers it.

## Amendments

Amends ADR 0003. Clarifies the "subscribers never block producers"
contract as: subscribers don't block producers, but slow sinks do, and
the drops that are possible are visible rather than silent.
