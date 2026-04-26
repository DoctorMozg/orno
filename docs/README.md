# orno documentation

orno is a CI-native runner for strict agentic loops. This directory holds the documentation set; the [top-level README](../README.md) holds the elevator pitch.

## Start here

1. [What is orno](what-is-orno.md) — the runtime contract, in plain English.
2. [Install](install.md) — building, running, and verifying a setup.
3. The [`hello`](../examples/hello/) example — smallest end-to-end run.

## Pipeline grammar

[`yaml-spec.md`](yaml-spec.md) is the prose specification of the pipeline YAML. The canonical JSON Schema lives at `schemas/pipeline.schema.json` and is regenerated via `cargo run -p orno-cli -- schema`.

## Examples

Browsable, runnable example pipelines live in [`../examples/`](../examples/README.md), one folder per example. Each example folder ships its own README with the surface exercised and the inputs needed.

## Quick references

- [Glossary](glossary.md) — vocabulary in one page.
- [FAQ](faq.md) — common questions and recipes.
