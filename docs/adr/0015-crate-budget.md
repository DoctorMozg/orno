# ADR 0015 — Crate budget

- Status: proposed; extends ADR 0001
- Date: 2026-04-21

## Context

ADR 0001 commits orno to a two-crate workspace (`orno-core`,
`orno-cli`) and flags uv's 60-crate tree as the opposite trap. It
leaves the rule implicit: "further splitting happens only when a
subtree has a demonstrated independent consumer or a build-
parallelism bottleneck." Without a concrete test, "demonstrated"
drifts toward "imagined," and the two-crate discipline erodes one
sensible-sounding split at a time.

Pressure to split exists today for `orno-sqlite` (persistent sink),
`orno-plugins` (subprocess host), `orno-mcp` (MCP client wrapper),
and `orno-yaml` (pipeline schema). Each of these is individually
defensible and collectively toxic — the jj-vcs two-crate analog that
ADR 0001 leans on stops working once the split count crosses four or
five.

The brainstorm on 2026-04-21 surfaced this as the highest-probability
architectural drift risk (lens-cto's "Third-Crate Trap,"
lens-historian's comparator analysis of orchestrator graveyards).
The runner-up idea ("write the crate-budget ADR") takes this on
directly.

## Decision

A new workspace crate ships only when an ADR documents at least one
of the following rules, with evidence:

1. **Named second consumer.** A committed or published downstream
   tool — not `orno-cli` — that depends on the carved-out subtree.
   "Hypothetical plugin authors" and "future embedders" do not count;
   the consumer must exist in a repository the reviewer can open.
2. **Measured build-time win.** ≥ 15 seconds of clean-build wall-
   clock reduction on the project CI image (`ubuntu-latest`, `cargo
   build --workspace`), measured before and after the split. The ADR
   includes both numbers.
3. **Security boundary.** A privilege separation that cannot be
   enforced inside a single crate — e.g., a process-isolated plugin
   host with a reduced-privilege child. Module-level visibility
   (`pub(crate)`, `pub(super)`) does not qualify; this is about
   binary separation, not logical layering.

Any PR that modifies `[workspace.members]` in the root `Cargo.toml`
requires an accompanying ADR citing the rule invoked. A split under
rule 2 must include the measurement in the ADR body, not in a linked
issue.

## Consequences

- Short-term pressure to split (the event log growing large, the
  pipeline schema gaining many variants) is resolved through module-
  level refactors inside `orno-core` until one of the three tests is
  met.
- `orno-sqlite` as a feature-gated module inside `orno-core`
  (`sqlite` feature flag) remains the expected landing for durable
  persistence per ADR 0003 — no new crate, no ADR needed.
- `orno-mcp` stays a module, not a crate — ADR 0007's `McpClient`
  trait lives in `orno-core::mcp`.
- The plugin-host crate, when it lands post-v0.1, invokes rule 3
  (process-isolation boundary) in its own ADR alongside the
  stabilization ADR for `NodeKind::External` (ADR 0014).
- The discipline aligns with ADR 0001's jj-vcs precedent: two crates,
  grown internally, split only on hard evidence.

## Amendments

Extends ADR 0001. Makes the "when to split" test explicit. No
existing crate is affected; the rule applies to all future workspace
additions.
