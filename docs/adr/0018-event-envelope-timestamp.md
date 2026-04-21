# ADR 0018 — Event envelope carries an RFC 3339 emission timestamp

- Status: accepted
- Date: 2026-04-21

## Context

ADR 0003 pinned the wire envelope at
`EventEnvelope { schema_version: u32, seq: u64, event: Event }`. `seq`
gives strict monotonic emission order within a run, which is what the
scheduler and record/replay infrastructure need, but it is not
human-readable and is not useful for cross-stream correlation — pairing
an event line with a stderr tracing log, or merging the event streams
of two concurrent runs, both require wall-clock time.

Operators running `orno run` in CI today see event lines like:

```json
{"schema_version":1,"seq":1,"event":{"type":"run_started","run_id":"…"}}
```

There is no way to tell when an event fired without cross-referencing
the surrounding log context. That makes triage of CI failures and
replay-diff debugging needlessly painful.

The one-line fix that every downstream consumer expects is an RFC 3339
timestamp on the envelope itself.

## Decision

Add a required `timestamp: OffsetDateTime` field to `EventEnvelope`,
serialized as RFC 3339 UTC via `time::serde::rfc3339`:

```rust
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub event: Event,
}
```

Wire shape on stdout:

```json
{"schema_version":1,"seq":1,"timestamp":"2026-04-21T18:31:54.387860Z","event":{"type":"run_started","run_id":"…"}}
```

Supporting rules:

- **UTC only.** Emission always captures `OffsetDateTime::now_utc()`; no
  local-time flag, no per-run timezone. CI logs get consolidated across
  timezones, and RFC 3339 with `Z` suffix is the lingua franca.
- **RFC 3339, not ISO 8601 broadly.** RFC 3339 is a strict subset of ISO
  8601 and is what every JSON log pipeline (OpenTelemetry, Loki, Datadog)
  expects out of the box.
- **`time` crate, not `chrono`.** `time` is smaller, has no transitive
  legacy `chrono-tz` / localtime baggage, and `tracing-subscriber`'s
  built-in timer already speaks `time::format_description::well_known::Rfc3339`,
  so the two streams use the same formatter.
- **Envelope, not per-event.** The timestamp is orthogonal to event
  payload and lives on the envelope next to `seq` and `schema_version`.
  Adding it to individual `Event` variants would both duplicate the
  field across every variant and muddle the separation between wire
  metadata and event payload.
- **Constructor `EventEnvelope::new(seq, event)`** owns schema-version
  and timestamp filling, so no scheduler site can forget either. The
  scheduler still owns `seq` generation because emission order is its
  authoritative property.
- **No `schema_version` bump.** The envelope is pre-v0.1 and has no
  downstream consumers or persisted replay files to break. Once the
  first `SqliteSink` ships, future shape changes will bump the version.
  The field can be made backwards-compatible at that point with
  `#[serde(default)]` if we retrofit a tolerant reader.

Stderr tracing stream is updated in lockstep: `init_tracing` uses
`UtcTime::new(Rfc3339)` so `tracing` JSON output on stderr carries the
same timestamp format as `EventEnvelope` NDJSON on stdout. A run's two
streams can be joined on wall clock without a format translator.

## Consequences

- Event streams are legible in raw form — an operator reading `orno run`
  output can tell what happened and when without a decoder.
- Replay fidelity is unchanged: `seq` is still the strict ordering key.
  `timestamp` is metadata, not load-bearing for determinism.
- Record/replay must redact `timestamp` in snapshot assertions. insta
  filters and the forthcoming replay diff tool treat `timestamp` the
  same way they treat `run-<nanos>` run ids.
- The `time` crate enters the dependency graph. Features are pinned
  minimal: `std`, `formatting`, `parsing`, `serde`, `serde-well-known`.
  `tracing-subscriber`'s `time` feature is enabled for the matching
  timer.
- ADR 0003's envelope shape is extended, not replaced. This ADR is
  listed in 0003's `## Amendments` section; 0003's four-seam rationale
  stands unchanged.
