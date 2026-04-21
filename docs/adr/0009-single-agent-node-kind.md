# ADR 0009 — Single `agent` node kind, no separate `llm` kind

- Status: accepted
- Date: 2026-04-21

## Context

The skeleton shipped with three node kinds: `llm` (single-shot
completion), `shell`, and `external` (reserved for ADR 0004 plugins).
ADRs 0005–0008 introduce the strict-agentic-loop model, which naturally
wants its own kind — call it `agent`.

Running with both `llm` and `agent` doubles the validation path, the
executor wiring, the event coverage, and the template context a user
has to learn. The single-shot LLM call is the degenerate case of an
agent with no tools, `max_iterations: 1`, and a passthrough `stop`
condition. Keeping both is duplication disguised as flexibility.

## Decision

Collapse `NodeKind::Llm` into `NodeKind::Agent`. Single-shot LLM is
expressed as:

```yaml
- id: summarize
  kind: agent
  agent: inline          # or a reference to agents.<name>
  model: gpt-5
  provider: openai
  max_iterations: 1
  allowed_tools: []
  initial_prompt: "Summarize: {{ nodes.fetch.stdout }}"
```

Two node kinds remain for LLM-free work: `shell` and `external`.
`external` keeps its ADR 0004 deferral.

### What changes in the skeleton

- Remove the `Llm` variant from `NodeKind` and `NodeRequest`.
- Remove `LlmNode` type; introduce `AgentNode` with fields for
  inline agent config plus an `agent: String` reference to
  `agents.<name>`.
- Remove `LlmExecutor`; introduce `AgentExecutor` that owns the
  loop from ADR 0005.
- `LlmTransport` (ADR 0002) is unchanged — it is the record/replay
  seam inside `AgentExecutor`, not an executor of its own.
- Regenerate `schemas/pipeline.schema.json` via
  `cargo run -p orno-cli -- schema`.
- Update `examples/hello.yaml` to the new shape.

## Consequences

- One validation path for every LLM-facing node, one place the
  five strictness dimensions (ADR 0005) live.
- The minimal "just make an LLM call" pipeline is more verbose
  than the old `kind: llm`. This is intentional — the new
  verbosity is the knobs you must think about (iterations,
  tools, effects). Defaults exist but are explicit in the docs,
  not silent in code.
- Docs tagline shifts from "orno runs LLM nodes" to "orno runs
  agents"; the README and CLAUDE.md need updating accordingly.
- The Event enum no longer distinguishes LLM-node events from
  agent-node events; the same `LlmRequestStarted`,
  `LlmResponseReceived`, `ToolCallStarted/Completed/Failed`,
  `AgentCompleted` variants cover all cases. Fewer event
  variants, not more.
- Inline vs. referenced agent config is a surface-level
  convenience — under the hood both materialize to the same
  `LoopAgent` (ADR 0006) before execution.
