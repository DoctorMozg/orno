# Exit codes

`orno` follows the Unix convention: exit `0` on success, non-zero on failure. The exact non-zero value depends on the subcommand and the failure category.

## `orno run`

| Exit code | Condition                                                                                                                                                                                  |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `0`       | Pipeline loaded and executed; the final `run_finished` event reports `ok: true`.                                                                                                            |
| non-zero  | Pipeline failed to load (`PipelineError`), or the engine returned `Err` while driving the DAG. `run_finished` may or may not be emitted depending on where the failure surfaced.            |

A node failure within the pipeline does **not** by itself force a non-zero exit. Node failure is a *pipeline-level* signal carried on `node_finished { ok: false }` and aggregated in `run_finished.failed_nodes`. The CLI process exits `0` even when nodes failed, as long as the engine itself completed cleanly. This is intentional: downstream tools consuming the NDJSON stream are the deciders, not the shell.

The implication for CI: a green `orno run` exit is **necessary but not sufficient** for a green run. Always inspect the trailing `run_finished` envelope's `ok` field. The repository's example pipelines and integration tests do this with `jq`:

```bash
orno run pipeline.yaml | tee events.ndjson
tail -1 events.ndjson | jq -e '.event.type == "run_finished" and .event.ok' >/dev/null
```

## `orno replay`

| Exit code | Condition                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------ |
| `0`       | Bundle loaded and replay completed; the final `run_finished` event reports `ok: true`.                  |
| non-zero  | Bundle file missing or malformed, or replay encountered a tape miss. Tape misses are **never** a fallback to the live API — they always surface as failures.   |

Same caveat as `orno run`: per-node replay-miss failures surface on `node_finished` events, not on the process exit code unless the bundle itself was unusable.

## `orno validate`

| Exit code | Condition                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------ |
| `0`       | Pipeline loaded and passed semantic validation.                                                        |
| non-zero  | Any `PipelineError` — file not found, YAML parse error, validation failure, undefined agent reference. |

`orno validate` is a quick pre-flight check; pair it with `orno plan` for a fuller pre-spend audit.

## `orno plan`

| Exit code | Condition                                                                                                                                                |
| --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`       | Pipeline loaded, validated, and is spendable. One `plan_node` line per node and a single `plan_summary` line have been emitted on stdout.                 |
| non-zero  | Pipeline failed to load or validate. No `plan_*` lines are emitted on a non-zero exit.                                                                    |

`orno plan` performs no LLM calls, no network, and no tool execution, so a `0` exit is a clean signal that the pipeline is *structurally* approvable. Whether the *intent* is approvable is what the human reviewer decides.

## `orno schema`

| Exit code | Condition                                                  |
| --------- | ---------------------------------------------------------- |
| `0`       | JSON Schema written to stdout.                             |
| non-zero  | Internal serialization failure (should not occur).         |

## `orno completions`

| Exit code | Condition                                                                            |
| --------- | ------------------------------------------------------------------------------------ |
| `0`       | Completion script written to stdout.                                                 |
| non-zero  | Unknown shell argument; clap will print a usage message before the non-zero exit.    |

## See also

- [CLI](cli.md) — every subcommand and its flags.
- [Events](events.md) — the wire format that carries node-level pass/fail beyond the process exit code.
- [Errors](errors.md) — typed error enums behind non-zero exits.
