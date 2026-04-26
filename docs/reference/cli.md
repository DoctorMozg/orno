# CLI reference

The `orno` binary has six subcommands. Every subcommand exits `0` on success and non-zero on failure; see [exit codes](exit-codes.md) for the discriminating values.

```
orno <COMMAND> [OPTIONS]
```

`orno --help` and `orno <command> --help` print the same surface this page documents.

## `orno run`

Execute a pipeline.

```
orno run <PIPELINE> [OPTIONS]
```

| Argument     | Required | Description                                                                                                          |
| ------------ | -------- | -------------------------------------------------------------------------------------------------------------------- |
| `<PIPELINE>` | yes      | Path to the pipeline YAML file. Loaded, validated, and executed.                                                     |

| Flag                                | Value     | Description                                                                                                                                                                                                                                                                                                                       |
| ----------------------------------- | --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `-e`, `--env <KEY=VAL>`             | `KEY=VAL` | Inline binding for the `env.*` template namespace. Repeatable; last flag wins on a duplicate key. Refused for names declared in the pipeline's `secrets:` block — `argv` leaks into shell history. Use `--secrets-file` for credentials.                                                                                          |
| `--env-file <PATH>`                 | path      | Dotenv file merged into the `env.*` template namespace. Repeatable; later files shadow earlier ones. A binding whose name appears in the pipeline's `secrets:` block is routed into `secrets.*` instead (name-based classification cannot be downgraded by source).                                                              |
| `--secrets-file <PATH>`             | path      | Dotenv file merged into the `secrets.*` template namespace. Repeatable; later files shadow earlier ones.                                                                                                                                                                                                                          |
| `-v`, `--verbose`                   | flag      | Verbose diagnostics. Bumps tracing to `debug` (unless `RUST_LOG` is already set) and lifts the default `--stderr-tail-bytes` cap from 2 KB to 64 KB.                                                                                                                                                                              |
| `--stderr-tail-bytes <BYTES>`       | integer   | Cap on captured stderr (and similarly bounded payloads — `LlmFailure.body_excerpt`, MCP excerpts, `SetState` payloads) in failure diagnostics. Default `2048` in normal mode, `65536` with `--verbose`. An explicit value here always wins.                                                                                       |
| `--record-tape <PATH>`              | path      | Write every LLM request/response pair to a tape file for later replay. Creates or truncates. Mutually exclusive with `--replay-tape` and `--record-bundle`.                                                                                                                                                                       |
| `--replay-tape <PATH>`              | path      | Replay LLM calls from a tape instead of hitting the live API. A tape miss is a hard error — no live-API fallback. Mutually exclusive with `--record-tape` and `--record-bundle`.                                                                                                                                                  |
| `--record-tool-tape <PATH>`         | path      | Write every tool/MCP call result to a tape file. Creates or truncates. Mutually exclusive with `--replay-tool-tape` and `--record-bundle`.                                                                                                                                                                                        |
| `--replay-tool-tape <PATH>`         | path      | Replay tool/MCP calls from a tape instead of executing them live. A tape miss is a hard error. Mutually exclusive with `--record-tool-tape` and `--record-bundle`.                                                                                                                                                                |
| `--record-bundle <PATH>`            | path      | Record the full run (LLM tape + tool tape + the pipeline YAML itself) into a single bundle for replay with `orno replay`. Mutually exclusive with the four individual tape flags.                                                                                                                                                 |

### Streams

`orno run` emits two distinct streams:

- **stdout** — `EventEnvelope` NDJSON, the user-facing event log. Downstream tools consume this.
- **stderr** — `tracing` JSON, internal observability. Log pipelines consume this.

Both streams use RFC 3339 UTC timestamps so they are joinable on wall clock without parsing `seq`. Do not pipe stderr into the same consumer as stdout — they have different schemas.

### Examples

```bash
# basic run with a pipeline argument
orno run examples/hello/pipeline.yaml

# pass an env binding and a secrets file
orno run pipeline.yaml -e PR_NUMBER=482 --secrets-file .env.secrets

# record a bundle for later replay
orno run pipeline.yaml --record-bundle run.ndjson

# verbose diagnostics
orno run pipeline.yaml -v
```

## `orno replay`

Re-execute a recorded run from a bundle. No live LLM calls, no network, no MCP server spawning — every external interaction is served from the bundle's tapes.

```
orno replay <BUNDLE>
```

| Argument   | Required | Description                                                  |
| ---------- | -------- | ------------------------------------------------------------ |
| `<BUNDLE>` | yes      | Bundle file written by a prior `orno run --record-bundle`.   |

A tape miss during replay produces `LlmFailure::ReplayMiss` (for LLM calls) or `ToolError::Invocation` (for tool/MCP calls), surfaces as `node_finished { ok: false }`, and exits non-zero. There is no fallback to the live API.

```bash
orno replay run.ndjson
```

## `orno validate`

Load and validate a pipeline without executing it. Checks tool names against the registered set, agent and MCP server references, budget fields, and the DAG structure.

```
orno validate <PIPELINE>
```

| Argument     | Required | Description                            |
| ------------ | -------- | -------------------------------------- |
| `<PIPELINE>` | yes      | Path to the pipeline YAML file.        |

Exits `0` if the pipeline is loadable and structurally valid, non-zero on any `PipelineError`. No events are emitted; failures print to stderr.

## `orno plan`

Static preview of a pipeline. No LLM calls, no network, no tool execution. Emits one `plan_node` line per node followed by a single `plan_summary` line on stdout as NDJSON.

```
orno plan <PIPELINE>
```

| Argument     | Required | Description                            |
| ------------ | -------- | -------------------------------------- |
| `<PIPELINE>` | yes      | Path to the pipeline YAML file.        |

The reviewer audits the worst-case ceiling — declared tools, declared effects, max tokens, max tool calls, MCP dependencies — before any spend is authorized. Exit code is `0` iff the pipeline loads, validates, and is spendable.

```bash
orno plan examples/hello/pipeline.yaml
```

## `orno schema`

Print the pipeline JSON Schema (the canonical wire-format spec) to stdout.

```
orno schema
```

No arguments. The output is the JSON Schema generated from the in-tree types and is the source of truth for the YAML grammar — when this disagrees with [`pipeline-yaml.md`](pipeline-yaml.md), the schema wins.

The repository's `schemas/pipeline.schema.json` is regenerated from this command:

```bash
cargo run -p orno-cli -- schema > schemas/pipeline.schema.json
```

## `orno completions`

Emit shell completions to stdout.

```
orno completions <SHELL>
```

| Argument  | Required | Description                                                                |
| --------- | -------- | -------------------------------------------------------------------------- |
| `<SHELL>` | yes      | One of `bash`, `zsh`, `fish`, `elvish`, `powershell`.                      |

```bash
orno completions bash > /etc/bash_completion.d/orno
orno completions zsh  > "${fpath[1]}/_orno"
orno completions fish > ~/.config/fish/completions/orno.fish
```

## See also

- [Exit codes](exit-codes.md) — what each non-zero exit means.
- [Environment variables](env-vars.md) — variables `orno` reads.
- [Events](events.md) — wire format of the NDJSON stream emitted by `orno run`.
- [Pipeline YAML](pipeline-yaml.md) — the input grammar for `<PIPELINE>` arguments.
