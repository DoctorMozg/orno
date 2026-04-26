# FAQ

## What is orno actually for?

CI workflows that need to invoke an LLM agent under a contract a reviewer can audit before authorizing spend. The five strictness dimensions — iteration, tool surface, effects, resources, non-determinism — are runtime guarantees, not defaults. The hero surfaces `orno plan` and `orno replay` exist so a pipeline can be reviewed and re-run without spending tokens.

## Why YAML?

Pipelines are read more often than written, and they are the artifact a reviewer audits before a run. YAML is also what every other CI tool the pipeline lives next to (GitHub Actions, GitLab CI, Argo) uses, so authors do not have to learn a second declarative dialect.

## Why does an agent need a policy block?

Because the runtime contract is enforced from it. Every effect class — mutation, network, domain reach, context-self writes — is gated by an explicit policy field, and every breach produces a typed event a reviewer can grep for. Defaulting to "anything goes" is exactly what orno is designed not to be.

## How is "multi-agent" different from peer-to-peer agents?

orno's multi-agent model is recursive single-agent loops. A parent calls a child via the synthetic `subagent.<name>` tool, the child runs its own bounded loop, and returns its final assistant message to the parent like any other tool result. There are no chat channels between siblings. The flat list under `agents:` defines named agents; the DAG under `nodes:` decides which run as top-level nodes; the `subagent.<name>` tool decides which can be invoked as children.

## Why OpenRouter as the default provider?

A single API key (`OPENROUTER_API_KEY`) unlocks OpenAI, Anthropic, Google, and open-weight models behind one OpenAI-compatible endpoint. Agents pick the upstream by giving the OpenRouter route as `model:` (`openai/gpt-5`, `anthropic/claude-sonnet-4.5`, etc.) without per-vendor plumbing. Direct-vendor providers remain valid identifiers but require the matching vendor key.

## How are secrets redacted?

Classification is name-based. Provider-known names (e.g. `OPENROUTER_API_KEY` when an agent's `provider:` is `openrouter`) are auto-classified. User-declared names go in `secrets: [NAMES]` at the top level. Either way, values are replaced with `***` in every event body, tracing line, and replay tape. Putting an `OPENROUTER_API_KEY=sk-...` line in `.env.inputs` does not downgrade it — orno still routes it into `secrets.*`.

## Can a child agent be more permissive than its parent?

No. A child's `allow_mutations` / `allow_network` cannot exceed its parent's. Enforced at pipeline load (`orno validate`). The intent is that a read-only parent cannot launder a privileged operation through a mutating child.

## Why isn't `WebSearch` a builtin?

There is no provider-neutral web search. Whether the right backend is Tavily, Brave, Bing, or a private index depends entirely on the operator. The shipped surface is what every install can rely on out of the box; web search returns when there is a stable provider trait to host it on. In the meantime, MCP servers fill the gap.

## What happens if my replay bundle is missing an LLM call?

The replay transport returns `LlmError::ReplayMiss`, which surfaces as a `node_finished` event with `ok: false` and a structured `failure: LlmFailure::ReplayMiss`. The CLI process exits 0 — node failure is a pipeline-level signal, not a process-level one. A tape miss is never silently filled by a live call.

## Why are stdout and stderr separate?

stdout is the user-facing event log: NDJSON envelopes meant for downstream tools. stderr is internal observability: tracing JSON for log pipelines. Both timestamps are RFC 3339 UTC so the streams join trivially on wall clock. Mixing them would force every consumer to filter, and would let observability noise accidentally cross into the contract surface a reviewer audits.

## Can I parallelise nodes in the DAG?

Today, the engine schedules nodes serially. Disjoint branches still execute (a failure in one branch does not skip nodes in another), but they execute one node at a time. Parallel scheduling is on the work list and will not change the user-facing pipeline shape when it lands.

## Can an agent overwrite its own state mid-loop?

`SetState` writes single-level keys under `nodes.<self>.state.*`. A second call with the same key overwrites the prior value wholesale. The whole state tree is size-capped at the engine's `max_output_bytes`. Oversize writes become a typed error and roll back, so the state buffer never holds a half-written value. Secret-valued leaves are redacted before storage.

## How do I see what a pipeline will do without running it?

`orno plan <pipeline.yaml>` — one `plan_node` line per node, one `plan_summary` line for the whole pipeline. No LLM calls, no network, no tool execution. Exit code is `0` iff the pipeline loads, validates, and is spendable. A reviewer audits the worst-case ceiling before authorizing spend.

## How do I run a pipeline without an API key?

Two options. For deterministic smoke testing, set `ORNO_TEST_LLM_TRANSPORT=dummy` — the dummy transport returns canned responses. For replay-only execution against a previously recorded run, use `orno replay <bundle.ndjson>` — it never calls the live API and never touches the network.

## Can I declare an agent inline at the node level?

No. Every agent configuration lives under the top-level `agents:` block so the agent shape is reviewable in one place. A node references the agent by name (`agent: <name>`).
