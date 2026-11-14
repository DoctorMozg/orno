# Exit codes

`orno` follows the Unix convention: exit `0` on success, non-zero on failure. The exact non-zero value depends on the subcommand and the failure category.

## `orno run`

| Exit code | Condition                                                                                                                                                                          |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`       | Pipeline loaded and executed; the final `run_finished` event reports `ok: true`.                                                                                                   |
| `1`       | One or more nodes reported `ok: false`; or the pipeline failed to load (`PipelineError`); or the engine returned `Err`. `run_finished` is always emitted with `ok: false` and the `failed_nodes` / `skipped_nodes` aggregates. |
| `130`     | Run interrupted by SIGINT (Ctrl-C). The event stream is closed immediately; no `run_finished` envelope is emitted.                                                                 |

## `orno replay`

| Exit code | Condition                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------ |
| `0`       | Bundle loaded and replay completed; the final `run_finished` event reports `ok: true`.                  |
| non-zero  | Bundle file missing or malformed, or replay encountered a tape miss. Tape misses are **never** a fallback to the live API — they always surface as failures.   |

Per-node replay-miss failures surface on `node_finished` events **and** on the process exit code (exit `1`) — same behavior as `orno run`.

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
