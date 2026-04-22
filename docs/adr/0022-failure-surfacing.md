# ADR 0022 — Surface every failure on stderr and on the wire

- Status: accepted
- Date: 2026-04-22

## Context

Through Wave 5, an `orno run` whose shell node exited non-zero produced
exactly one diagnostic signal: `{"type":"node_finished","ok":false}` on
stdout. The `exit_code`, captured `stderr`, and captured `stdout` of
the failed child were thrown away inside `Engine::dispatch_node` —
specifically by the `(ok, ok.then_some(resp.output))` return shape
that gated the output on success. `Engine::run` never logged at WARN.
The CLI never printed the error chain on a non-zero `RunFinished`.

ADR 0021 explicitly flagged this as a known gap:

> `Event::NodeFinished { run_id, node_id, ok }` does not carry
> `stdout` / `stderr` / `exit_code`. Node output is threaded into
> `Context` for downstream templating but not re-emitted on the event
> stream. A future ADR decides whether the envelope grows output
> fields or whether consumers must snapshot the sink in-process.

In practice the gap was worse than that paragraph suggests — on a
failed node, the output never reached `Context` either, because
`dispatch_node` only forwarded payload on success. A user investigating
a CI failure had no actionable signal anywhere: no exit code, no
stderr, no error chain, just `ok: false`.

Three failure surfaces were involved:

1. **Operator-facing stderr** — `tracing` JSON. Read by humans and log
   pipelines. Has no schema; can grow without coordination.
2. **Tool-facing stdout** — `EventEnvelope` NDJSON. Read by downstream
   automation. Schema is versioned; growth is constrained by
   `#[non_exhaustive]` plus `schema_version`.
3. **Pipeline-internal context** — `Context.nodes.<id>.*`. Read by
   downstream `{{ node.X.* }}` templates. No external schema; the
   shape is the YAML spec.

All three need the failure data, but not in the same shape: stderr can
carry truncated tails for human reading, the wire format needs a
typed discriminator for machine consumption, and `Context` needs the
unbounded payload because templates may need the full stderr to make a
recovery decision.

## Decision

### 1. Wire format grows `failure: Option<NodeFailure>` on `NodeFinished`

```rust
#[non_exhaustive]
pub enum NodeFailure {
    NoExecutorRegistered { node_kind: String },
    TemplateRenderFailed { error: String },
    ExecutorError { error: String },
    NodePayloadFailure {
        exit_code: Option<i64>,
        stderr_tail: Option<String>,
    },
}
```

`failure` is `Some` exactly when `ok: false`. We keep `ok: bool` next
to `failure` (rather than collapsing into a single tagged enum)
because every existing consumer that branched on `ok` continues to
work — destructuring with `.., ok, .. ` ignores the new field, and a
JSON consumer reading `event.ok` ignores `event.failure: null` on
success cases. The redundancy buys back-compat for free.

The variant covers four real failure paths in `dispatch_node`:

- **`NoExecutorRegistered`** — kind named in YAML has no executor in
  the registry. A configuration error, surfaced before any work runs.
- **`TemplateRenderFailed`** — MiniJinja rendering of the node's
  request failed. Renders the `anyhow`-style chain via `{:#}` so the
  root cause (unknown variable, syntax error) is on the wire.
- **`ExecutorError`** — `NodeExecutor::execute` returned `Err`. Covers
  process-spawn failures, transport errors, and any other pre-payload
  failure mode an executor surfaces. Same `{:#}` rendering as above.
- **`NodePayloadFailure`** — executor returned `Ok` but the payload
  signaled failure. Today this is shell with non-zero `exit_code`; the
  `node_response_ok` classifier is the single arbiter of which payload
  shapes count.

Strict-loop dimensions (`BudgetExceeded`, `IterationLimitExceeded`,
`ToolDenied`, `MutationDenied`, `NetworkDenied`) land here as
additional variants when those subsystems come online — `NodeFailure`
is the future home for ADR 0005's enforcement-result vocabulary.

#### Field-naming nit

`NoExecutorRegistered { node_kind: String }` deliberately uses
`node_kind`, not `kind`, because the enum's serde tag is `kind`. A
field named `kind` collides with the discriminator and rejects at
compile time. `node_kind` is the disambiguated form; the value is
whatever string `kind_str` returns for the node (`"shell"`,
`"agent"`).

#### Why `Option` for the payload fields

`exit_code: Option<i64>` and `stderr_tail: Option<String>` are
`Option` because the executor's payload may legitimately omit them
(the `shell_missing_exit_code_is_ok` test guards the contract). The
wire format reflects what was actually present rather than coercing
absence to a sentinel.

### 2. `tracing` WARN at every failure site

Every failure path in `dispatch_node` and the walker-construction path
in `Engine::run` emits a structured WARN before the failure
propagates. Shape is typed-native, not Debug:

```rust
tracing::warn!(
    node.id = %node.id,
    node.kind = kind,
    exit_code = exit_code.unwrap_or(-1),
    stderr_tail = %stderr_tail.as_deref().unwrap_or(""),
    stdout_tail = %stdout_tail,
    "node returned failure in payload",
);
```

`exit_code` is an `i64` field so the JSON formatter renders it as a
number, not a quoted Debug string. Tail fields are `Display` so they
render as plain strings without `Some(...)` wrapping.

`stdout_tail` is omitted from the wire format but included in the WARN
under `--verbose`. Stdout on failure is rarely the cause; verbose-mode
operators have already consented to noisier output.

### 3. `EngineConfig { verbose, max_output_bytes }`

A small struct on `Engine` consulted at every failure site so verbosity
is one tested knob rather than a forest of `if`-let guards. `verbose`
controls whether `stdout_tail` joins the WARN; `max_output_bytes`
caps the captured stderr/stdout windows. Defaults are
`verbose: false, max_output_bytes: 2048`.

The `truncate_tail` helper keeps the **trailing** window of a string
on a UTF-8 boundary, prefixed with `"…"` to mark truncation. Tail wins
over head because the actionable error in a tool's stderr is almost
always the last line (stack trace, fatal:, ENOENT). UTF-8 boundary
discipline is enforced — the `truncate_tail_respects_utf8_boundaries`
test guards against splitting a multi-byte codepoint.

The wire-format `stderr_tail` and the WARN `stderr_tail` use the same
cap, so a user looking at stderr and a downstream tool reading the
event stream see the same data.

### 4. `dispatch_node` no longer discards `resp.output` on failure

Previous code:

```rust
(ok, ok.then_some(resp.output))
```

`then_some` evaluated to `None` whenever `ok == false`, dropping the
shell payload (its `stderr`, `stdout`, `exit_code` JSON keys) before
it could reach `Context`. Now the engine always records `resp.output`
into `Context.nodes.<id>` when the executor returned `Ok`, regardless
of payload-derived `ok`. A downstream node — including a future
recovery branch that does not transitively depend on the failed node
— can read `{{ node.fail.exit_code }}` and `{{ node.fail.stderr }}`.

The wire format's `NodeFailure::NodePayloadFailure` carries a
**bounded** stderr tail; `Context` carries the **unbounded** payload.
Two surfaces, two different audiences, two different bounds.

### 5. CLI flags

`orno run` grows two flags:

- `-v` / `--verbose`: tracing default filter bumps from `info` to
  `debug`, and `stdout_tail` joins failure WARNs. Explicit `RUST_LOG`
  always wins over the verbose-derived default.
- `--stderr-tail-bytes <BYTES>`: explicit override for the WARN/wire
  truncation cap. Default is 2048 bytes, or 65 536 bytes under
  `--verbose` if the flag is unset.

The CLI also prints the error chain on a non-zero result via
`eprintln!("error: {err:#}")` so a setup failure (cycle, missing
file) reaches the operator without parsing tracing JSON. The exit
code is unchanged: pipeline `ok: false` is still a stream-level
signal, never a process-level error.

## What this is explicitly **not**

- **Not a record/replay extension.** `NodeFailure` rides the existing
  `EventEnvelope` versioning; replay tapes from before this ADR
  remain readable because `failure` deserializes to `None` when
  absent (it is `Option`, not a required field).
- **Not a `RunFinished` summary.** Surfacing `failed_nodes` /
  `skipped_nodes` aggregates on `RunFinished` is a Phase 3 concern
  (see Deferrals below). This ADR only covers per-node failure.
- **Not LLM transport failure surfacing.** A new
  `Event::LlmRequestFailed` for transport errors during agent nodes is
  also Phase 3.
- **Not a `continue_on_error` opt-out.** Behavior on failure (skip
  cascade) is unchanged from ADR 0021.
- **Not a schema regen trigger.** `schemas/pipeline.schema.json`
  covers only the YAML pipeline shape (driven by `Pipeline`'s
  `JsonSchema` derive); events have no published schema today.

## Consequences

- **Wire format addition.** `Event::NodeFinished` carries
  `failure: Option<NodeFailure>`. `NodeFailure` is `#[non_exhaustive]`
  so future variants are additive. Every existing destructure of
  `NodeFinished` either uses `..` (tolerates the new field) or
  recompiles cleanly with the explicit field added.
- **Behavior change.** A failed shell node's payload now reaches
  `Context.nodes.<id>`. Downstream templates that did not previously
  read failed-ancestor output cannot accidentally regress; the change
  is purely additive for templates and entirely behind a per-node
  failure that already cancels the failed node's transitive
  descendants (so dependents are skipped, not exposed to the new
  data, unless they explicitly opt out via independent `needs:`).
- **`Engine::new` arity grew to 4** with `EngineConfig` as the new
  argument. Embedders using `EngineConfig::default()` get the
  pre-existing behavior.
- **Two new CLI flags** (`--verbose`, `--stderr-tail-bytes`) and one
  new tracing-on-error path (`init_tracing(verbose)` + the
  `eprintln!` chain printer in `main`).
- **Test surface adds 7 cases**: `truncate_tail_keeps_trailing_window`,
  `truncate_tail_passthrough_when_within_cap`,
  `truncate_tail_respects_utf8_boundaries`,
  `walker_construction_failure_emits_warn_before_propagating`,
  `shell_nonzero_exit_emits_warn_with_exit_code_and_stderr_tail`,
  `shell_nonzero_exit_emits_node_payload_failure_on_wire`,
  `shell_failure_payload_still_lands_in_context_for_downstream`. The
  pre-existing `missing_executor_surfaces_failure` was extended to
  assert the wire `NodeFailure::NoExecutorRegistered` shape.
- **`tracing-subscriber` enters `orno-core` as a dev-dependency** for
  the capture-based WARN tests. Production code only depends on the
  `tracing` macro crate.
- **No insta snapshots churned** because the test suite uses
  substring matching against NDJSON; the addition of `failure: null`
  on success cases does not break any assertion. Wire-format
  snapshots will be added as part of the record/replay ADR (Phase 7).

## Phase 3 deferrals (next ADR)

- `Event::LlmRequestFailed { run_id, node_id, provider, model, error }`
  for agent nodes whose transport call fails. Today the failure
  surfaces only as `NodeFailure::ExecutorError`; an explicit event
  lets log pipelines page on transport-class failures separately
  from generic node failures.
- `Event::RunFinished` grows `failed_nodes: Vec<String>` and
  `skipped_nodes: Vec<String>` aggregates so a single tail-line read
  of the event stream tells the operator the full failure footprint
  without folding the stream.
