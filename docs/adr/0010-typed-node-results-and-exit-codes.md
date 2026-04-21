# ADR 0010 — Typed node results and process exit codes

- Status: accepted
- Date: 2026-04-21

## Context

The v0.1.0 skeleton models node output as a bare `String` and has no
notion of pass/fail at the node level. For a CI-native tool that is a
gap in two directions:

- Downstream nodes cannot branch on whether an upstream agent succeeded
  or failed. Today a read-only lens agent that "fails review" looks
  identical to one that "passes review" — both produce a prose message
  and set no status flag.
- `orno run` cannot propagate pipeline outcome to a non-zero exit code.
  CI wrappers (GitHub Actions, CircleCI, buildkite) rely on exit-code
  conventions; a process that always returns 0 is useless as a gate.

Three options exist:

1. Keep the bare string and let each pipeline author agree on a parse
   convention. Forces every pipeline to re-invent the contract; no
   exit-code semantics.
2. Typed `NodeResult { status, output }` plus standard CI exit codes.
   Small schema growth, unlocks downstream status reads and exit-code
   wiring.
3. A full control-flow DSL — conditional nodes, error handlers,
   branches. Speculative for v0.1.0 and overlaps with DAG scheduling
   work deferred to Phase 7.

## Decision

- Every node produces `NodeResult { status: NodeStatus, output: String }`
  where `NodeStatus = Ok | Failed` (marked `#[non_exhaustive]` so
  `Skipped` / `TimedOut` can land later without a breaking change).
- Status derivation:
  - **Shell nodes**: `Ok` iff the subprocess exits with code `0`,
    `Failed` otherwise. `output` is captured stdout.
  - **Agent nodes**: `Failed` on any policy/budget/iteration violation,
    unrecoverable tool error, MCP crash that terminates the loop, or
    when the final assistant message parses as JSON with a top-level
    `"status": "fail"` field. `Ok` otherwise. On `Failed`, `output`
    carries the terminator's error string; on `Ok`, `output` is the
    final assistant message.
- Downstream templates read `{{ nodes.<id>.status }}` — serialized as
  the lowercase string `"ok"` or `"failed"` — and `{{ nodes.<id>.output }}`
  (unchanged).
- Scheduler semantics: if an upstream node's status is `Failed`, its
  descendants are **skipped** by default and the pipeline reports
  failure. Per-node opt-out `continue_on_error: true` overrides this —
  the node's `Failed` status is still recorded in events and templates,
  but descendants run and the pipeline's overall exit code is computed
  as if the node succeeded.
- `orno run` exit codes:
  - `0` — every node `Ok` (or `Failed` but covered by `continue_on_error`).
  - `1` — pipeline load / infra error: bad YAML, missing required env,
    MCP handshake failure at startup, unknown tool in `allowed_tools`.
    Pipeline did not run.
  - `2` — pipeline ran but at least one `Failed` node was not covered
    by `continue_on_error`.
- Events: every `NodeCompleted` / `AgentCompleted` / `ShellCompleted`
  carries the resulting `NodeStatus`, so replay reproduces exit codes
  without re-running the pipeline.

### Conditional execution: deferred

A `when: "<jinja expression>"` node field — letting a node gate on an
upstream's status — is the obvious follow-on. Rejected for v0.1.0: it
adds a predicate evaluator, expands the DAG-validation surface (dead
branches, unreachable subgraphs), and overlaps with the scheduler work
in Phase 7. Pipelines that need branching today put the check inside
an agent's `initial_prompt` — the agent can read `nodes.X.status` and
decide what to do. Revisit post-v0.1 if multiple pipelines demand it.

### Agent failure convention, not tool

An agent emits failure by returning a final message of the form
`{"status": "fail", ...}`. This is enforced by the agent's system
prompt, not by a runtime `FailNode` tool. Rationale:

- Adding a new builtin expands the fixed tool set from ADR 0008 and
  forces a policy decision (does `FailNode` need `allow_mutations`?).
- The JSON-final-message pattern is what well-configured agents already
  produce; teaching them to set a `status` key costs a system-prompt
  line.
- Convention means an agent that forgets the key defaults to `Ok` on
  successful loop completion. That is the correct default — silence
  is not failure.

## Consequences

- YAML schema grows by one field (`continue_on_error` on `Node`) and one
  reserved template slot (`nodes.<id>.status`). Regenerate
  `schemas/pipeline.schema.json` when the change lands.
- CLI documents a three-level exit-code contract (`0` / `1` / `2`) in
  `orno run --help` and `README.md`. This is the ABI users script
  against; later changes are breaking.
- `NodeResult` extends ADR 0003's event envelope without revising any
  existing seam. New status-carrying variants of `NodeCompleted` /
  `AgentCompleted` / `ShellCompleted` are append-only via
  `#[non_exhaustive]`. ADR 0003's amendment section should gain a
  pointer to this ADR when it is next touched.
- Pipelines that rely on "downstream still runs after upstream failure"
  semantics (e.g., "collect review findings even if the build fails")
  must add `continue_on_error: true` explicitly. This is a behavior
  change versus the implicit always-proceed skeleton; the explicitness
  is the point.
- The `"status": "fail"` convention means agents communicating with
  `orno` should emit JSON as their final message. Free-form prose
  still works and still maps to `Ok`; mixed prose-and-status is
  ambiguous and parsed as `Ok` (no `status` key found).
- Replay consumers gain the ability to reconstruct exit codes from the
  recorded event log alone, without re-running the pipeline or the
  agents inside it.
