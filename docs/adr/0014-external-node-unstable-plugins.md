# ADR 0014 — `NodeKind::External` behind `--unstable-plugins`

- Status: proposed; amends ADR 0004
- Date: 2026-04-21

## Context

ADR 0004 reserves `NodeKind::External { command, args, timeout }` as
the stable wire format for subprocess plugins, with no executor
registered in v0.1. The current shape freezes a schema that lacks
five things the real plugin protocol will need:

1. JSON handshake on subprocess start (protocol version + capability
   list).
2. Capability negotiation (does the plugin support streaming? cancel?
   effects declaration?).
3. Partial-output streaming semantics — today's shape implies
   request/response, tomorrow's will be long-running.
4. Stdout/stderr discipline matching `orno run`
   (stdout = `EventEnvelope` NDJSON, stderr = tracing JSON).
5. Cancel ladder distinct from timeout — SIGTERM → grace → SIGKILL.

The current schema is not a superset of any of those five extensions.
The type is public via `#[serde(tag = "kind")]` — any `kind: external`
YAML parses today, setting user expectations that a post-v0.1 protocol
rewrite will break.

The brainstorm on 2026-04-21 surfaced this as latent v0.1 debt
(lens-engineer's "Freezes a Wire Format That Isn't the Real
Protocol," lens-cto's "The Plugin Cliff"). Both supporters of the
winning seam-hardening proposal flagged it as a foundation-level gap.

## Decision

1. **`NodeKind::External` is not part of v0.1.0 user-facing
   surface.** Accepting a YAML pipeline with `kind: external` without
   the `--unstable-plugins` CLI flag is a hard
   `PipelineError::UnstableNodeKind` validation failure.
2. **`--unstable-plugins` ships from v0.1.0** and is documented in
   `--help` and the README as explicitly unstable: the struct shape
   may change without a semver bump, and no migration tooling is
   promised. Users opt in at their own risk.
3. **Stabilization checklist.** Before `NodeKind::External` becomes
   default-available (target: v0.2 or v0.3), a follow-up ADR must
   specify:
   - JSON handshake: protocol version field, capability list.
   - Capability flags: at least `streaming`, `cancel`,
     `effects-declaration`.
   - Stream discipline: stdout = `EventEnvelope` NDJSON, stderr =
     tracing JSON — identical to `orno run`.
   - Cancel ladder: SIGTERM → configurable grace → SIGKILL, with a
     `NodePluginCancelled` event on the parent side.
   - Timeout semantics distinct from cancel: timeout is a budget
     breach (emits `BudgetExceeded { kind: WallClock }`), cancel is
     a scheduler/user decision (emits `NodePluginCancelled`).
4. **Internal use is preserved.** `NodeKind::External` stays in-tree
   so the scheduler, `NodeExecutor` trait, and serde surface can be
   exercised against the reserved variant. Test helpers may
   construct `NodeKind::External` directly, bypassing pipeline parse.
5. **No speculative rename.** Renaming to `ExternalLegacy` or
   `ReservedFuture` was considered and rejected — the flag gate is
   sufficient and a rename is itself a wire-format break we pay for
   nothing. The stabilization ADR picks the final name.

## Consequences

- No `external`-kind pipeline ships in v0.1 examples, tests, or
  documented templates. `examples/` stays agent-and-shell only.
- Users who need subprocess behavior at v0.1 use `kind: shell` with
  a declared effects block (ADR 0013) — this covers the vast
  majority of near-term external-node use cases.
- The wire-format-freeze discipline from ADR 0004 is preserved for
  the actual stabilization ADR; we have not frozen a bad schema.
- `PipelineError::UnstableNodeKind` is a new error variant on the
  `#[non_exhaustive]` error enum — append-only.
- The `--unstable-plugins` flag becomes the standing gate for any
  future unstable surfaces; future ADRs that introduce unstable
  features can reuse the same flag rather than invent per-feature
  flags.

## Amendments

Amends ADR 0004. Adds a concrete v0.1 gating rule that ADR 0004
deliberately left open (ADR 0004 said "no loader ships in v0.1.0"
but left `NodeKind::External` accepting at the serde layer). Does
not touch ADR 0008 or 0009 — the builtin tool set and single-agent-
node-kind decisions stand.
