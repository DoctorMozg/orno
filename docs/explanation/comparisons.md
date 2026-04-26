# orno compared to other tools

orno's territory is narrow: a CI-native runner for **strict, bounded, replayable agent loops**. That puts it alongside several tools that look superficially similar but solve different problems. This page is an honest comparison so you can decide whether orno fits your shape — and where to combine it with something else.

## TL;DR

- **Use orno** when an LLM agent needs to run inside CI or a scheduled job under enforced bounds, with auditable cost ceilings and replayable runs.
- **Use a workflow orchestrator** (Inngest, Temporal, Airflow) when the dominant concern is durability across hours/days, multi-actor coordination, or human-in-the-loop steps.
- **Use an interactive agent framework** (Mastra, CrewAI, AutoGen, LangGraph) when you want emergent multi-agent behaviors, debate, or a chat UI.
- **Use a provider SDK** (Claude Agent SDK, OpenAI Agents SDK) when you are embedding an agent inside a long-lived application and the host process is your audit boundary.

These categories compose. The most common pairing is **Inngest (or Temporal, or a CI step) for the workflow, orno for the AI leaf** — the orchestrator handles durability, retries, and human gates; orno handles the bounded LLM loop.

## vs. LangGraph

[LangGraph](https://langchain-ai.github.io/langgraph/) is a Python/TypeScript library for building agent applications as state graphs. Nodes are functions that read and write a shared state object; edges are conditional transitions; the graph runs in-process.

Differences:

- **Topology.** LangGraph is a programmable state graph: any node can transition to any other node, agents can loop, branch, and call back into themselves. orno is a static DAG of agent and shell nodes, plus recursive single-agent loops via `subagent.<name>`. There is no peer-to-peer messaging, no shared blackboard.
- **Bounds.** LangGraph relies on the host process to enforce limits. orno enforces five strictness dimensions at runtime — iteration, tool surface, effects, resources, non-determinism — independent of the host.
- **Replay.** LangGraph supports checkpointing for resume-on-crash, with the live LLM as the source of truth on resume. orno records every external interaction and replays from the bundle as a hard contract: a tape miss is a hard error, not a fallback.
- **Audit surface.** LangGraph state is a Python/TS object that the host introspects. orno emits a versioned NDJSON event stream that any tool — `jq`, a log pipeline, a CI dashboard — can consume without speaking Python.
- **Distribution.** LangGraph runs inside your application. orno is a single binary you invoke from CI.

When LangGraph fits better: you want emergent agent behaviors (debate, voting, market mechanisms), you're embedding the agent inside a Python/TS service, or you want to programmatically inspect and mutate state mid-run.

When orno fits better: the agent runs unattended in CI, the operator needs to audit what *could* happen before it does, and the run needs to be replayable from a recorded bundle.

## vs. Inngest

[Inngest](https://www.inngest.com/) is a durable workflow orchestrator. You write functions in TypeScript/Python/Go; Inngest schedules them, retries them on failure, and persists state across invocations. It's designed for long-running workflows that span hours or days.

Differences:

- **Time horizon.** Inngest workflows survive across machine restarts, weeks of waiting, human approval steps. An orno run is one process invocation — minutes to hours, not days.
- **Failure model.** Inngest retries with exponential backoff and durable state. orno terminates a node on a strict-mode breach with a typed error; recovery is the surrounding system's job.
- **Concurrency.** Inngest models concurrent workflows across actors. orno schedules a single DAG within a single run.
- **AI specifics.** Inngest is AI-agnostic — it can call LLMs, but it does not enforce bounded iteration, bounded tool surfaces, or replayable LLM tapes. orno's whole reason for existing is those bounds.

The natural pairing: Inngest schedules and durably retries the workflow; one of its steps invokes `orno run pipeline.yaml` for the AI-bounded leaf. Inngest gets a typed exit code and stream from orno; orno gets a single-shot invocation and records its own bundle for postmortem.

## vs. Temporal

[Temporal](https://temporal.io/) is a workflow durability engine — same shape as Inngest but lower-level, language-agnostic via SDKs, and with stronger semantics for long-running, fault-tolerant workflows. It is the reference implementation of "workflow as code, fully durable."

Differences are the same shape as Inngest's: Temporal is a workflow engine; orno is an agent-loop runner. Temporal handles the *when* and *how often*; orno handles the *what the agent is allowed to do during one invocation*.

The pairing pattern is the same: a Temporal activity invokes `orno run` and consumes its event stream as the activity's output.

## vs. n8n

[n8n](https://n8n.io/) is a visual, node-based workflow builder. You wire steps together in a UI; integrations cover hundreds of SaaS APIs.

Differences:

- **Authoring.** n8n is GUI-first; pipelines are JSON exported from the editor. orno is YAML-first; the file is the source of truth and is reviewed in a PR like any code.
- **Audience.** n8n targets ops/business users automating SaaS integrations. orno targets engineers running AI workloads in CI.
- **Bounds.** n8n has no concept of bounded iteration or tool surfaces — its agent nodes are convenience wrappers over LangChain.
- **Auditability.** A reviewer cannot diff an n8n workflow trivially across versions. An orno pipeline is a YAML file in git.

When n8n fits better: visual workflow building, dozens of SaaS integrations, non-technical users.

When orno fits better: the agent runs in CI, the team reviews changes via PRs, and bounded cost is a hard requirement.

## vs. Mastra / CrewAI / AutoGen

[Mastra](https://mastra.ai/), [CrewAI](https://www.crewai.com/), [AutoGen](https://microsoft.github.io/autogen/) are interactive multi-agent frameworks. They emphasize agent-to-agent collaboration: a "crew" of specialized agents debate, vote, hand off tasks, or simulate roles.

Differences:

- **Topology.** Multi-agent peer-to-peer collaboration is the headline feature. orno explicitly does **not** support peer-to-peer; multi-agent in orno is recursive single-agent loops via `subagent.<name>`. A child returns one final message to its parent and exits.
- **Use case.** These frameworks shine for emergent collaborative behaviors — research crews, role-play simulations, debate. orno targets bounded leaf workloads — "review this PR with five lenses," "summarize these tickets," "extract these fields."
- **Determinism.** These frameworks generally assume non-determinism is acceptable; orno makes determinism a first-class contract via record/replay.
- **CI fit.** These frameworks ship as Python libraries embedded in services. orno ships as a CLI binary that exits with a status code.

When these frameworks fit better: you want emergent multi-agent dynamics, you're building a chat UI, or your application is the audit boundary.

When orno fits better: you want a tree of bounded specialists, the run is unattended, and the cost ceiling is enforced before any spend.

## vs. Claude Agent SDK / OpenAI Agents SDK

The [Claude Agent SDK](https://docs.claude.com/en/api/agent-sdk) and [OpenAI Agents SDK](https://platform.openai.com/docs/guides/agents) are provider-specific libraries for building agents. They include tool calling, multi-turn conversation management, structured outputs, and (in Claude's case) subagent dispatch.

Differences:

- **Provider lock.** Each SDK is built around its provider's API. orno's `LlmTransport` trait abstracts the provider; the default is OpenRouter, which fronts most providers.
- **Bounds.** These SDKs offer iteration limits as opt-in parameters; tool gating is the application's responsibility. orno makes all five strictness dimensions mandatory and enforced at runtime.
- **Distribution.** These SDKs run in-process inside your application. orno runs as a separate CLI binary.
- **Replay.** These SDKs do not record/replay every external interaction byte-for-byte. orno does.
- **Audit.** A reviewer of your application code has to read the SDK call sites to know what an agent can do. A reviewer of an orno pipeline runs `orno plan` and gets a single audit-ready summary.

When these SDKs fit better: you are embedding an agent inside a long-lived service, the host process is your audit boundary, and you want first-class access to provider-specific features.

When orno fits better: the agent runs unattended in CI, the contract must be visible in YAML, and the run must be replayable from a recorded bundle.

The Claude Agent SDK's subagent pattern is a direct influence on orno's `subagent.<name>` design — same recursive shape, same "child agent as a tool call" semantic. orno enforces it as a runtime contract instead of a coding pattern.

## vs. raw `curl` to a chat completion endpoint

It's worth saying: for many CI tasks, the simplest answer is a `curl` to an LLM API followed by `jq`. No agent loop, no tools, no MCP. orno is overkill for "summarize this commit message in 30 words" — that's a one-shot completion.

orno's value compounds when the task involves *tool use* (the model needs to read files, run shell commands, call MCP tools, fetch URLs) and you want bounds on that tool use. If the agent never calls a tool, orno is just a fancy template engine wrapped around a chat completion — and you'd be better off with `curl` plus a YAML-aware shell pipeline.

A useful test: does the task need ≥2 turns of model interaction *with intermediate tool calls*? If yes, orno earns its weight. If no, a one-shot is simpler.

## When *not* to use orno

orno is opinionated against several things, and naming them is cleaner than dancing around them:

- **No interactive UI.** orno produces NDJSON and exits. Need a chat surface? Use Mastra, the Claude Agent SDK, or build directly on a provider API.
- **No durable workflows.** A run is one process. Need to wait six hours for a human approval? Use Inngest or Temporal, with an orno step inside.
- **No emergent multi-agent behavior.** orno's tree of bounded specialists is a structural choice. Need debate, market mechanisms, or sibling messaging? Use AutoGen or CrewAI.
- **No GUI authoring.** Pipelines are YAML in git. Need point-and-click? Use n8n or a no-code platform.
- **No "permissive mode".** Strict mode is the only mode. Need an unbounded "raw" agent? Use a provider SDK directly.

## Composition patterns

Most production AI workflows are not single-tool problems. The patterns that work well:

- **Workflow orchestrator + orno.** Inngest/Temporal/Airflow handles scheduling, retries, durable state, human approvals. One step shells out to `orno run` for the AI-bounded leaf. The orchestrator gets a typed exit code; orno gets a self-contained invocation.
- **CI + orno.** A GitHub Actions workflow runs `orno plan` on PR open (audit step), then `orno run` on merge (execution step) with `--record-bundle` writing to artifact storage. Postmortems use `orno replay`.
- **Provider SDK + orno.** Your application uses the Claude Agent SDK for an interactive surface; backend cron jobs use orno for the bounded batch jobs. Same model, different deployment shapes.

The shared theme: orno is the **leaf** — the smallest unit of "agent did something" that can be audited, bounded, and replayed. It is rarely the whole system.

## See also

- [Strict agentic loops](strict-agentic-loops.md) — what makes orno's contract *strict*.
- [What is orno](../what-is-orno.md) — the contract in plain English.
- [FAQ](../faq.md) — short-form answers to "why isn't there a *foo*."
