# orno v0.1.0 Roadmap

**Target**: Strict Agentic MVP in 8–10 weeks solo.
**Status**: skeleton complete (Phases 1–3 committed). Substance starts at Phase 4.

This document is a plan, not a contract. Weeks are rough; the phase order is load-bearing, the exact timing is not.

## Positioning for the v0.1.0 launch post

> "Orno runs strict agentic loops in your CI. Five dimensions — iteration, tool surface, effects, resources, non-determinism — bounded by config, enforced by the runtime, audited through an event log. MCP-compatible tool extension. Subagent delegation. Replay from recorded tape. Plan before spend."

If any one of the five dimensions is unenforced at launch, the post above is false. That is the gate.

## Hero surface

The launch leads with two paired commands. Both are lossless, both are CI-runnable, and neither can be retrofit by Kestra, gh-aw, or LangChain because none of them ships a typed event-log seam (ADR 0003):

- **`orno plan <pipeline.yaml>`** — static preview of what a run will do. No LLM calls, no tool execution, no network. Emits the DAG, declared effects (per agent and per shell node), budget totals, tool surface, and MCP dependencies. Mental model: `terraform plan` for agent pipelines — a buyer already understands it.
- **`orno replay <tape.ndjson>`** — byte-identical re-execution from a recorded run. Reproduces outputs, exit code, and event log without spending LLM tokens or hitting networks.

The pairing is the pitch: `plan` proves a pipeline can be audited before spend; `replay` proves any past run can be re-executed without it. Brainstorm 2026-04-21 shortlisted this as the go-to-market wedge (lens-engineer, lens-cto), contingent on the seam-hardening epic (ADRs 0012–0014) landing first — a lossy event log produces replays that lie, and a plan that omits declared effects understates the audit claim.

## Phase list

### Phase 4 — LLM transport + single-shot agent (weeks 1–2)

- Wire `LlmTransport` to `genai` (ADR 0002). OpenAI, Anthropic, Ollama providers behind the trait.
- Implement `LoopAgent` with `max_iterations=1, allowed_tools=[]`. No tools yet.
- Wire `AgentExecutor` as the impl of `NodeExecutor` for `NodeKind::Agent` (ADR 0009).
- Remove `NodeKind::Llm`; update `examples/hello.yaml` to the new shape.
- Template rendering via MiniJinja for `initial_prompt`, `system`, `vars`, and `nodes.<id>.output` references.

**Exit criteria**: one real LLM call through the agent loop, with `LlmRequestStarted` + `LlmResponseReceived` events on the log, the response text propagated as the node output, and a `RecordingTransport`/`ReplayTransport` round-trip test passing.

### Phase 5 — Builtin tools + strict loop (weeks 3–4)

- Implement the five concrete `ToolHandler` impls (ADR 0008): `BashHandler`, `ReadHandler`, `EditHandler`, `WriteHandler`, `WebFetchHandler`.
- Agent loop extends to full five-dimension enforcement (ADR 0005): iteration cap, tool-surface check, effect gating (`allow_mutations`, `allow_network`, domain lists), budget enforcement (tokens, tool calls, wall clock), event emission at every decision point.
- Argument validation: typed args structs for builtins; JSON schema derived via `schemars`; permissive `serde_json` parsing with `on_parse_error: fail | retry_once`.
- Negative tests: one per strictness dimension minimum.

**Exit criteria**: an agent can read a file, call `WebFetch`, write a report, and terminate cleanly on budget or iteration breach. All five strictness events are emittable in tests.

### Phase 6 — MCP + subagents + replay (weeks 5–6)

- `McpClient` trait + `RmcpClient` impl (ADR 0007). Stdio and HTTP transports. Lifecycle-managed by run; events at every transition.
- `SubagentHandler` (ADR 0006). Depth-bounded recursion. Effect-policy compose-down enforced at pipeline load.
- Replay end-to-end: `RecordingTransport`/`ReplayTransport` around `LlmTransport`; tool-result recording around `ToolHandler`; subagent outputs cached in the parent's event log.
- Integration test against one real MCP server (filesystem or github).

**Exit criteria**: `examples/pr-review.yaml` runs with three subagent lenses; the same run replays deterministically from the recorded tape.

### Phase 7 — DAG scheduler + hero surface + polish (weeks 7–8)

- DAG scheduler with parallel node execution (ADR 0003 actor fan-out). Pipeline-level parallelism is the only parallelism orno offers (ADR 0005).
- `orno replay <tape.ndjson>` subcommand.
- `orno plan <pipeline.yaml>` subcommand — static DAG + declared-effects + budget preview. No LLM calls, no tool execution, no network. Emits `PlanNode` / `PlanSummary` NDJSON to stdout; exit code `0` iff the pipeline loads, validates, and is spendable. Pairs with `orno replay` as the launch-post hero surface.
- `orno validate` covers the full ADR 0005–0008 policy surface (tool-surface existence, domain-list shape, effect-class checks on MCP tools). `plan` is the spend-preview layer above `validate`; `validate` is the correctness floor.
- Documentation: README, CLAUDE.md in sync with code, `docs/yaml-spec.md` feature-complete, each `examples/*.yaml` annotated.
- Launch post draft committed. Leads with the `plan`/`replay` pairing; five-dimensions copy is the supporting paragraph, not the headline.

**Exit criteria**: three examples in `examples/` run green; replay deterministic; `plan` produces correct summaries for each; launch post draft in `docs/launch/`.

### Phase 8 — Launch (weeks 9–10, slack)

- Real-world dogfooding: run orno in CI on one open-source repo (likely a pr-review workflow on orno itself).
- Bug fixes from dogfooding.
- `v0.1.0` tag, release notes, launch post published.

## Deferred to v0.2.0+

- **`WebSearch` builtin** — needs `SearchProvider` trait + Tavily/Brave impls (ADR 0008 defers).
- **Generic `HttpHandler`** — use MCP for HTTP-backed tools until demand justifies the builtin.
- **User-authored tool schemas** (full Architecture A from `docs/chat.md`).
- **SQLite `EventSink`** (ADR 0003 designed the seam; impl lands when durability is actually requested).
- **Whole-node plugin protocol** (ADR 0004; remains deferred; ADR 0014 hides the stub behind `--unstable-plugins` for v0.1).
- **MCP server restart/retry policies** (ADR 0007 only terminates on crash in v0.1.0).
- **Streaming LLM responses** with mid-flight budget enforcement.
- **Budget-as-SLO with provider failover** — extend `AgentPolicy`'s budget model with `error_budget_burn` per provider and a `providers: [primary, fallback]` failover DSL. Motivation: real-world LLM SLA breaches (Anthropic, April 2026) are not modelable by today's counter-based budget; a stuck primary burns wall-clock and tokens without triggering any ceiling. Requires a follow-up ADR that specifies burn-budget accounting, failover-trigger semantics (error rate window? latency percentile? SLA-published posture?), retry/fallback composition with `max_iterations`, and new event variants (`ProviderErrorBudgetExceeded`, `ProviderFailoverTriggered`). Deferred to v0.2 because (a) the design is not concrete — it needs real-user failover pain to anchor the trigger thresholds, and (b) orno does not yet have the replay-tape coverage to reproduce provider-degradation scenarios deterministically.
- **Inline agent config** at the node level (v0.1.0 requires the `agents:` block for readability).
- **Advanced DAG scheduling**: fan-out/fan-in barriers, cross-run dependencies.
- **WASM plugins** (ADR 0004 hard no for v1.x).

## What can slip

- Phase 7 DAG parallelism. Linear node execution is acceptable for v0.1.0 if the rest ships; parallelism becomes a v0.1.1 patch.
- `orno plan`'s richer output can slip to v0.1.1 if Phase 7 is tight. The minimal shape — DAG listing, declared-effects summary, budget totals — is the floor; formatted diffs, cost estimates, and structured provider-rate-limit projections are nice-to-haves. The pairing-with-replay pitch needs the floor, not the ceiling.
- `examples/flaky-test-triage.yaml`'s specific MCP server dependency — if the `@modelcontextprotocol/server-github` shape changes, swap to a simpler MCP server and note the swap in the example.
- Anthropic / Ollama providers in Phase 4 if `genai` regresses. Target one provider (OpenAI) as the must-ship baseline.

## What cannot slip

- **The five strictness dimensions** (ADR 0005). Cutting any one of them moves the product from "strict agentic" to "agentic" — that space is CrewAI's and we don't compete there.
- **Replay from a recorded tape**. The positioning hinges on it.
- **The event log** (ADR 0003), with the bounded-backpressure guarantees of ADR 0012. A lossy event log makes `replay` a lie and `plan` partial — hero-surface claims collapse.
- **MCP support**. Without MCP, "bounded tool surface + extension" is incoherent; users have no way to add tools.

## Upgrade-pain budget

1–2 days per quarter for `genai` and `rmcp` version drift (ADRs 0002, 0007). This is the cost of not hand-rolling and it is the right trade at v0.1.0 scope.
