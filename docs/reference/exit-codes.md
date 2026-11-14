# Exit codes

`orno` follows the Unix convention: exit `0` on success, non-zero on failure. The exact non-zero value depends on the subcommand and the failure category.

## `orno run`

| Exit code | Condition                                                                                                                                                                          |
| --------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0`       | Pipeline loaded and executed; the final `run_finished` event reports `ok: true`.                                                                                                   |
| `1`       | One or more nodes reported `ok: false`; the pipeline failed to load or validate (`PipelineError`); env or secrets resolution failed; or the engine returned `Err`. |
| `130`     | Run interrupted by SIGINT (Ctrl-C). A `run_finished` envelope with `ok: false` and `failed_nodes: ["<interrupted>"]` is emitted, record tapes are flushed best-effort, and MCP servers are drained (bounded at 5 seconds) before the process exits. |

When a run has started, `run_finished` is emitted for every outcome above, including MCP handshake failures and `vars:` render failures. Failures that happen before the run starts (the file cannot be read, the YAML does not parse or validate, secrets do not resolve, a `--record-tape` or `--record-tool-tape` target already exists) emit no events: stdout stays empty and the error goes to stderr.

`--record-tape` and `--record-tool-tape` create the file with `create_new`: an existing path is an error, not overwritten. (The `--help` text says "creates or truncates", but that wording is out of date.)

## `orno replay`

| Exit code | Condition                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------ |
| `0`       | Bundle loaded and replay completed; the final `run_finished` event reports `ok: true`.                  |
| non-zero  | Bundle file missing or malformed, or replay encountered a tape miss. Tape misses are **never** a fallback to the live API — they always surface as failures.   |

Per-node replay-miss failures surface on `node_finished` events **and** on the process exit code (exit `1`), the same as in `orno run`.

## `orno validate`

| Exit code | Condition                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------ |
| `0`       | Pipeline loaded and passed semantic validation.                                                        |
| non-zero  | Any `PipelineError` — file not found, YAML parse error, validation failure, undefined agent reference. |

`orno validate` is a quick pre-flight check. Pair it with `orno plan` for a fuller pre-spend audit.

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

- [CLI](cli.md): every subcommand and its flags.
- [Events](events.md): the wire format that carries node-level pass/fail beyond the process exit code.
- [Errors](errors.md): typed error enums behind non-zero exits.
