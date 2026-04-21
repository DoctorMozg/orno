# ADR 0016 — Enforcement modes for strictness dimensions

- Status: proposed; extends ADR 0005 and ADR 0013
- Date: 2026-04-21

## Context

ADR 0005 fixes five strictness dimensions on `agent` nodes with
termination as the default response for three of them (bounded
iteration, tool surface, resources), tool-call failure for the
fourth (effects), and recording for the fifth (non-determinism).
ADR 0013 adds a sixth dimension (shell effects) split into declared
and observed halves.

As orno adds dimensions, a purely hard-fail posture everywhere risks
the pattern the brainstorm's lens-historian called out as the "BPEL
warning": pre-standardized governance that blocks adoption before
real users validate the shape. The symmetric opposite (all warn, no
enforcement) risks security theater. The hedge the historian
proposed — `warn → soft-fail → hard-fail` — becomes useful when it is
applied per dimension with a declared trajectory, not as a blanket
escape hatch.

This ADR adopts that framework **without weakening ADR 0005's
existing defaults for the three load-bearing dimensions**. The
defaults are preserved; the knob is added for the dimensions where a
softer default is defensible (observed shell-effect violations,
future dimensions).

## Decision

Every strictness dimension carries a declared `enforcement:` mode.
Three modes:

- **`warn`** — log a tracing event at `warn` level, emit the typed
  violation `Event`, and continue. The node runs to completion.
- **`soft-fail`** — emit the typed violation `Event`, mark the node
  `failed`, and let the pipeline's `continue_on_violation` flag
  decide whether downstream nodes run.
- **`hard-fail`** — emit the typed violation `Event` and abort the
  pipeline immediately with a non-zero exit code.

Defaults per dimension with a declared trajectory:

| Dimension                        | v0.1      | v0.2      | v1.0      |
| -------------------------------- | --------- | --------- | --------- |
| Bounded iteration (0005)         | hard-fail | hard-fail | hard-fail |
| Bounded tool surface (0005)      | hard-fail | hard-fail | hard-fail |
| Bounded resources (0005)         | hard-fail | hard-fail | hard-fail |
| Bounded effects (0005)           | tool-fail | tool-fail | tool-fail |
| Bounded non-determinism (0005)   | recorded  | recorded  | recorded  |
| Shell effects declared (0013)    | hard-fail | hard-fail | hard-fail |
| Shell effects observed (0013)    | warn      | soft-fail | hard-fail |

`tool-fail` and `recorded` are the existing postures from ADR 0005
and are not enforcement modes — they are in the table for
completeness. `tool-fail` means the violation surfaces to the model
as a tool-call failure (model may recover); `recorded` means the
event is emitted but no enforcement is applied.

User overrides:

1. Pipeline-level or agent-level YAML key:
   `strictness: { shell_effects_observed: soft-fail }`. Any dimension
   from the table may be named.
2. Downgrading from a default (e.g., `hard-fail → soft-fail`) emits
   a `StrictnessOverride { dimension, declared, default }` event on
   pipeline start. Ops can assert on the absence of
   `StrictnessOverride` events in CI.
3. Upgrading (e.g., running bounded-effects as hard-fail instead of
   tool-fail) is always allowed and does not emit
   `StrictnessOverride`.
4. No override can weaken the three load-bearing dimensions below
   `hard-fail` — attempts emit `PipelineError::StrictnessLocked` at
   validation time. These dimensions are the product claim;
   softening them would make the claim false.

## Consequences

- A realistic adoption ladder exists: shell-effects-observed ships
  as warn-only in v0.1 without weakening the hard-fail commitments
  that make ADR 0005's strictness claim real.
- The progressive-enforcement framework is declared once here and
  reused by any future dimension-adding ADR — new dimensions
  specify their v0.1/v0.2/v1.0 trajectory in the same table format.
- `StrictnessOverride` is a new `Event` variant — append-only per
  ADR 0003.
- `PipelineError::StrictnessLocked` is a new error variant — lists
  the dimensions that cannot be downgraded; future ADRs may add to
  the locked list but never remove from it.
- `docs/strictness.md` (to be created) owns the canonical trajectory
  table and the override knob names. One source of truth; the ADR
  copy above is illustrative.
- Defaults may harden in future ADRs but never silently. Every
  hardening change is an ADR that updates the trajectory table.

## Amendments

Extends ADR 0005 (adds enforcement modes; existing v0.1 defaults
unchanged). Extends ADR 0013 (specifies the shell-effects-observed
trajectory). Does not override ADR 0003's append-only event-enum
discipline.
