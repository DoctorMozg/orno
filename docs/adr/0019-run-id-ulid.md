# ADR 0019 — Run identifiers are `run_<ULID>`

- Status: accepted
- Date: 2026-04-21

## Context

Until now, `orno run` minted run identifiers as `run-<nanos-since-epoch>`
with a TODO on the generator saying "switch to ULID once the scheduler
needs durable ordering guarantees." With ADR 0018 landing RFC 3339
timestamps on the envelope, the last remaining eyesore in the event
stream is the nanosecond-encoded run_id: human-unfriendly, not
copy-pastable without escape concerns on some shells (leading digits
are fine but 19-digit strings are easy to truncate or misread), and
colliding if two runs start in the same nanosecond tick on the same
machine.

The follow-on need is replay: the run_id becomes the primary key for
recorded LLM tapes and stored event logs (ADRs 0003, 0005, 0018). It
needs to be:

- **Unique** across runs without coordination.
- **Sortable** lexicographically so a `ls` of a replay directory lists
  runs chronologically.
- **Filename-safe** for use as a directory or file name across Linux,
  macOS, and Windows.
- **Grep-friendly** so `run_id` strings in logs are easy to search for
  as a single token.

## Decision

Run identifiers take the form `run_<ULID>`:

```
run_01KPRNJ5C12T866M9E71QBPGX2
```

- **Prefix `run_`** — self-describing in logs and replay filenames, and
  leaves room for a uniform convention (`node_`, `tool_`, `mcp_`, etc.)
  without ambiguity. Underscore over hyphen because underscores do not
  require quoting inside shell identifier contexts, are valid in Rust
  raw identifiers, and are filename-safe on every target platform.
- **ULID payload** — 26-character Crockford base32. First 48 bits are
  millisecond timestamp; lexicographic order matches chronological
  order. 80 bits of randomness after the timestamp make collisions
  within a millisecond negligible (2^80 namespace per ms).
- **Generation in `orno-core`** — `orno_core::execution::new_run_id()`
  is the canonical generator. The CLI calls it; tests, embedders, and
  the forthcoming replay driver do too. The `Engine::run(run_id, …)`
  caller-generates-id seam is unchanged — callers that need a specific
  id (fixed-id tests, replaying an existing tape) simply pass their
  own string.
- **Crate `ulid`** — the de-facto standard Rust ULID crate (v1.x,
  Apache-2.0). Minimal feature set; no serde integration needed
  because we serialize the `String` form directly.

What this is explicitly **not**:

- Not a newtype. `run_id: String` stays the signature everywhere.
  A `RunId` newtype is a defensible future refactor once multiple call
  sites parse or compare run_ids, but for v0.1 the `String` shape keeps
  the scheduler, sink, and event variants simple.
- Not URL-safe base64 or UUIDv7. ULID was chosen specifically for the
  Crockford base32 alphabet (no ambiguous characters, cleanly
  grep-able) and lexicographic-sort property; UUIDv7 has the sort
  property but its hex-with-dashes form is noisier in logs.

## Consequences

- Event streams become fully readable. A run_id is a single grep target
  and sorts chronologically in directory listings, log aggregators, and
  the forthcoming `orno replay <run-id>` CLI.
- Collision risk in the `run-<nanos>` form (two runs starting the same
  nanosecond on the same machine — possible under fast-restarting CI
  matrixes) is gone.
- Insta snapshot redactions shift: the filter regex goes from
  `run-\d+` to `run_[0-9A-HJKMNP-TV-Z]{26}`. CLAUDE.md's test-pattern
  section is updated accordingly; ADR 0018's incidental `run-<nanos>`
  example sentence stays as the historical record of that moment.
- One new crate in the graph. `ulid` is small (no transitive `chrono`
  or `uuid`), widely used, and has no unsafe in the public surface.
- Determinism guarantee unchanged: `Engine::run` still takes a
  caller-provided id. Replay drivers pass the recorded id verbatim;
  only fresh runs call `new_run_id()`.
