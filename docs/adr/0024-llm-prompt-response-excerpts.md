# ADR 0024 — Prompt and response excerpts on LLM events

- Status: accepted
- Date: 2026-04-22
- Phase: 4 follow-on — debuggability for the single-shot agent path

## Context

`Event::LlmRequestStarted` and `Event::LlmResponseReceived` today carry
only scalar identifiers: `{provider, model}` on the request side and
`{finish_reason, usage}` on the response side. Neither event carries any
of the content the agent actually sent or received. The doc comment on
`LlmRequestStarted` explicitly defended this as a hard rule:

> Carries provider + model identifiers but never the prompt — prompt
> bodies may contain rendered `secrets.*` values (ADR 0020) and must
> stay out of the event log.

That reasoning was correct *before the `Redactor` existed*. Since ADR 0020
landed, every rendered-value surface in the engine (`NodeFailure::
ExecutorError.error`, `NodePayloadFailure.stderr_tail`,
`NodeResponse.output` before it reaches `Context`) flows through the
per-run `Redactor` built from the resolved `secrets.*` map. The same
redaction boundary applies to agent-emitted payloads; the old "never the
prompt" position conflated "cannot be safely emitted" with "is not yet
wired through the redactor."

Meanwhile the user-facing observability cost is material:

- A user running `orno run -v examples/commit-digest.yaml` sees the
  provider and model name but not what was actually asked of the model
  or what came back. Today the only way to inspect either is to attach
  a debugger to `GenAiTransport::complete`.
- A CI pipeline that wants to assert "the agent was prompted with the
  expected system message" has no signal — `NodeResponse.output` is the
  *response*, not the *request*.
- A replay implementation (Phase 7) has to synthesize the prompt from
  `AgentConfig.system` plus the rendered `initial_prompt` rather than
  reading it straight off the tape.

The user asked directly: "if never the prompt/response — how can we
debug it properly?" Answer: we cannot, today. This ADR closes that gap.

## Decision

### 1. Wire format adds three excerpt fields

```rust
LlmRequestStarted {
    run_id: String,
    node_id: String,
    provider: String,
    model: String,
    prompt_excerpt: String,
    system_excerpt: Option<String>,
}

LlmResponseReceived {
    run_id: String,
    node_id: String,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    content_excerpt: String,
}
```

Three design points:

- **Three fields, not one merged blob.** The three payloads have
  different provenance (operator-authored system, rendered user prompt,
  model-authored response) and different redaction surfaces; a
  downstream tool that wants "only what the model said" should not
  have to parse it back out of a joined string.
- **`system_excerpt: Option<String>`.** An agent config without a
  `system:` block is distinct from one that declared an empty string.
  `None` on the wire preserves the difference; a `String::default()`
  collapse would lie.
- **`prompt_excerpt` / `content_excerpt` are `String`, not
  `Option<String>`.** Both fields are always populated (even with
  empty-string content), because the surrounding event only exists
  when the agent *did* call the transport. Forcing `Option` on every
  consumer without a real "absent" case is friction.

### 2. Redaction is the agent's responsibility, via a shared `Redactor`

The existing pattern in `execution::scheduler` is that whoever emits
an event owns redacting its user-visible strings. The scheduler does
this inline for `NodeFailure::{ExecutorError, NodePayloadFailure}`.
`LoopAgent` now follows the same pattern for its three excerpt fields:

```rust
let prompt_excerpt = self.excerpt_for_wire(&req.initial_prompt);
let system_excerpt = req.system.as_deref().map(|s| self.excerpt_for_wire(s));
// … later …
let content_excerpt = self.excerpt_for_wire(&response.content);
```

`excerpt_for_wire` composes redaction then head-truncation:

```rust
fn excerpt_for_wire(&self, s: &str) -> String {
    truncate_excerpt(self.redactor.redact(s).as_ref(), self.body_excerpt_max_bytes)
}
```

`LoopAgent` holds an `Arc<Redactor>` populated by the CLI before the
executor is constructed. The engine builds its own instance from the
same `inputs.secrets` map inside `Engine::run`; both carry the same
value list so redaction is consistent across agent- and
scheduler-emitted surfaces.

**Rejected alternative: `RedactingSink` decorator.** A sink that
wraps every emission in a redaction pass is architecturally cleaner
(no emitter has to remember to redact) but changes the sink contract
for every existing event and is substantially larger than the surface
this ADR needs. Left as a future refactor; the inline pattern matches
what the scheduler already does and the redactor is only ever
constructed once per run, so the duplication is cheap.

### 3. Truncation is head-retention at `body_excerpt_max_bytes`

Shared `truncate_excerpt` helper (promoted to `pub(crate)`), same cap
as `LlmFailure::ApiError.body_excerpt` (ADR 0023) and shell stderr
tails. A rendered prompt starts with the operator instruction; a model
response starts with the direct answer; truncating either from the
tail would drop the useful part. The leading `max_bytes` are kept and
a `"…"` marker is appended when truncation occurred.

`LoopAgent::new` already took `body_excerpt_max_bytes` as of ADR 0023
— the same value now also bounds the three new excerpt fields. The
CLI continues to thread `engine_config.max_output_bytes` through a
single call site, so a single flag governs all four caps.

### 4. Always-on emission, no verbose gate

Excerpts are emitted unconditionally, not behind `-v`. Three reasons:

- **Small and bounded.** At the default cap of 2048 bytes per field
  (three fields per agent call), worst-case overhead is ~6 KB per LLM
  round-trip — trivial next to token usage. A fast-path reader can
  still ignore the fields.
- **Debuggability is a contract, not a mode.** A consumer writing a
  UI or log pipeline against `LlmRequestStarted` today cannot assume
  the excerpt is present "sometimes." Either the field is a schema
  guarantee or it is not on the wire at all.
- **Verbose is for tracing, not wire format.** `EngineConfig.verbose`
  already shapes stderr detail (`tracing::warn!` contents); promoting
  the wire format to also care about that flag entangles two
  surfaces that were kept deliberately separate (ADR 0003).

### 5. `AgentExecutor::new` arity grew to 4

`AgentExecutor::new(transport, sink, redactor, body_excerpt_max_bytes)`.
Embedders constructing the executor directly pass an
`Arc<Redactor>`. `with_defaults(transport, sink)` stays
source-compatible and builds an empty `Redactor` internally — a test
or embedder without secrets pays no redaction cost because
`Redactor::is_noop()` short-circuits.

## What this is explicitly **not**

- **Not a record/replay tape.** The excerpts are bounded and
  redacted; they are not lossless enough to reconstruct the exact
  request for replay. Tape recording remains a separate Phase 7
  concern and will capture the unbounded, pre-redaction request.
- **Not a `debug!` tracing addition.** The existing stderr tracing
  stream does not grow; excerpts live on stdout NDJSON only, inside
  their enclosing lifecycle event. A downstream tool that wants them
  on stderr can re-emit after reading the envelope.
- **Not a per-iteration loop event.** Iteration-level events
  (`AgentIterationStarted`, per-turn excerpts) land with the agent
  loop in Phase 5; this ADR stays inside the single-shot path.
- **Not a retroactive re-emission.** Runs from before this ADR do not
  grow excerpt fields on re-load — the fields do not exist on those
  envelopes and a future replay reader must accept missing-field
  deserialization via `#[serde(default)]` (documented deferral from
  ADR 0023 applies here identically).
- **Not a prompt-logging toggle.** The excerpts are always on at the
  engine's `max_output_bytes` cap. A user who wants a smaller cap
  passes `--stderr-tail-bytes 256` (the flag name is historical but
  governs this cap too); a user who wants a larger excerpt passes
  `--verbose` which bumps the cap alongside other verbose surfaces.

## Consequences

- **Wire format addition.** `Event::LlmRequestStarted` gains two
  fields; `Event::LlmResponseReceived` gains one. `Event` is
  `#[non_exhaustive]` and existing destructures in-tree already use
  `..` where they care about ordering rather than shape — recompiling
  is clean. A forward-compat consumer that reads old envelopes must
  either add `#[serde(default)]` at deserialize time or be tolerant
  to missing fields; today there is no such consumer (no replay
  reader yet) so this is a deferred constraint.
- **`AgentExecutor::new` arity grew.** Embedders must pass an
  `Arc<Redactor>`. The `with_defaults` convenience constructor stays
  source-compatible and keeps tests short.
- **No CLI surface change.** `--verbose` / `--stderr-tail-bytes`
  retain their meaning; the same `max_output_bytes` value now also
  caps three more excerpt fields.
- **Test surface adds 4 cases** in `crates/orno-core/src/agent/
  loop_agent.rs`:
  - `emits_request_and_response_events_in_order` (amended to assert
    excerpt presence and content)
  - `system_excerpt_present_when_agent_config_declared_a_system_prompt`
  - `prompt_excerpt_redacts_known_secret_values`
  - `prompt_excerpt_truncates_at_configured_cap`
- **CLI substring tests survive.** `crates/orno-cli/tests/cli.rs` only
  asserts on event `"type"` discriminants, not on payload shape.
- **Secrets contract preserved.** ADR 0020's "rendered `secrets.*`
  values never reach the event log" is upheld: every excerpt passes
  through the shared `Redactor` before leaving `LoopAgent`. A test
  (`prompt_excerpt_redacts_known_secret_values`) asserts this
  directly by constructing a redactor with a known secret value,
  rendering it into the prompt, and checking the excerpt carries
  `"***"` and not the raw literal.

## Phase 5 / future follow-ons

- **Per-iteration excerpts.** When `LoopAgent` grows real iteration
  (ADR 0005), the same excerpt discipline applies to each
  `AgentIterationStarted` / `ToolCallRecorded` envelope. The
  `excerpt_for_wire` helper is the single hook; extending it to new
  events is a two-line change each.
- **`RedactingSink` refactor.** Once three or more emitters need the
  same redact-before-record pattern, hoist redaction into a sink
  decorator so emitters stop carrying the responsibility. Today two
  emitters (scheduler, agent) use the inline pattern — not yet
  enough to justify the refactor.
- **Lossless record/replay.** A replay tape needs the full prompt and
  response, not an excerpt. When `ReplayTransport` lands, it will
  record against the unbounded `LlmRequest.prompt` /
  `LlmResponse.content` — orthogonal to the excerpt wire format.
