# orno documentation

orno is a CI-native runner for strict agentic loops. This directory holds the documentation set; the [top-level README](../README.md) holds the elevator pitch.

## Start here

1. [What is orno](what-is-orno.md) — the runtime contract, in plain English.
2. [Install](install.md) — building, running, and verifying a setup.
3. The [`hello`](../examples/hello/) example — smallest end-to-end run.

## Tutorials

End-to-end walkthroughs. Read these in order if you're new to orno.

- [Your first pipeline](tutorials/first-pipeline.md) — write, validate, plan, and run a one-node pipeline.
- [Record and replay a run](tutorials/record-replay.md) — capture a bundle and replay it offline.
- [Multi-agent PR review](tutorials/multi-agent-pr-review.md) — `subagent.<name>` delegation and the compose-down rule.

## How-to guides

Task-shaped recipes. Pick the one that matches what you're trying to do.

- [Add an MCP server](how-to/add-mcp-server.md) — stdio + streamable-HTTP recipes.
- [Pass secrets](how-to/pass-secrets.md) — secrets file format, redaction, provider auto-discovery.
- [Scope state across nodes](how-to/scope-state-across-nodes.md) — `SetState` + `nodes.<id>.state.<key>`.
- [Tighten the budget](how-to/tighten-budget.md) — sizing iteration, token, and tool-call ceilings.
- [Debug a failed run](how-to/debug-failure.md) — reading the streams, identifying the failure variant.

## Reference

Mechanical specs of every public surface. Read these to look something up; read the explanation layer to understand it.

- [CLI](reference/cli.md) — every subcommand and its flags.
- [Pipeline YAML](reference/pipeline-yaml.md) — full grammar of the input file.
- [Tools](reference/tools.md) — every builtin tool, its arguments, and its effect class.
- [Events](reference/events.md) — wire format of the NDJSON event stream.
- [Errors](reference/errors.md) — typed error enums and their causes.
- [Environment variables](reference/env-vars.md) — variables orno reads at the runtime boundary.
- [Exit codes](reference/exit-codes.md) — what each non-zero exit means.

The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated via `cargo run -p orno-cli -- schema`. When the schema and `pipeline-yaml.md` disagree, the schema wins.

## Explanation

Read these to understand *why* orno is shaped the way it is.

- [Strict agentic loops](explanation/strict-agentic-loops.md) — what makes the runtime contract *strict*, and why each dimension is shaped the way it is.
- [Comparisons](explanation/comparisons.md) — how orno relates to LangGraph, Inngest, Temporal, n8n, multi-agent frameworks, and provider SDKs.
- [Security](security.md) — threat model, what orno protects, what it doesn't, and recommended deployment shapes.

## Examples

Browsable, runnable example pipelines live in [`../examples/`](../examples/README.md), one folder per example. Each example folder ships its own README with the surface exercised and the inputs needed.

## Quick references

- [Glossary](glossary.md) — vocabulary in one page.
- [FAQ](faq.md) — common questions and recipes.
