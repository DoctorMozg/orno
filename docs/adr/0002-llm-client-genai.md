# ADR 0002 — LLM client via the `genai` crate, wrapped behind `LlmTransport`

- Status: accepted
- Date: 2026-04-21
- Amends: the research recommendation of a hand-rolled OpenAI-compatible client

## Context

`docs/implementation_toolset_research.md` §1 argued for hand-rolling the
chat-completions surface in ~400 LOC with permissive serde DTOs. The
rationale: "OpenAI-compatible" is a marketing truce rather than a
specification, so an SDK's type strictness is its abstraction leak. aider,
continue, and codex-cli all wrap HTTP directly for this reason.

The `genai` crate is the cleanest multi-provider abstraction in the Rust
ecosystem and normalizes `reasoning_content` across vendors. Its downsides
per the research: it contradicts an OpenAI-compat-only constraint and ties
the project to a 0.5→0.6-alpha release cadence.

## Decision

Use `genai` for the concrete LLM transport. Do **not** expose its types on
orno's public surface. Instead define an in-house trait:

```rust
pub trait LlmTransport: Send + Sync {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError>;
    fn stream(&self, req: LlmRequest) -> impl Stream<Item = Result<LlmChunk, LlmError>>;
}
```

Implementations:

- `DummyTransport` — used in the skeleton and in tests, returns canned
  responses.
- `GenAiTransport` — the production impl, added when the LLM node moves from
  dummy to real.
- A future `RecordingTransport<T>` / `ReplayTransport` pair lands on this
  trait without touching node executors.

Pipeline and node executor code depends on `LlmTransport`, never on `genai`
directly. The dependency stays swappable.

## Consequences

- Saves multiple weeks of hand-rolling chat-completions DTOs, SSE dialect
  parsing per provider, and tool-call normalization.
- Accepts coupling to `genai`'s release cadence and its opinion on the
  provider set. If `genai` breaks badly, the trait boundary lets us swap in a
  hand-rolled transport without rewriting callers.
- Orno's compatibility matrix becomes "whatever `genai` supports" until a
  second transport proves necessary. Providers outside its matrix will need
  either a `genai` PR or a second transport implementation.
- The research's ADR 0001 ("OpenAI-compat-only") is superseded here: orno is
  now multi-provider via `genai`, not OpenAI-compat-only.
