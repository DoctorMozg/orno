# Environment variables

orno reads a small fixed set of environment variables. Variables are grouped by purpose; everything here is name-classified — a variable's role does not change based on the source file it came from.

orno does **not** auto-inherit the entire process environment into the `env.*` template namespace. Names must be opted in via `pass_env:`, `-e`, or `--env-file`. This section documents the exception list — variables orno itself reads at the runtime boundary.

## Provider credentials

orno supports exactly four providers (`openai`, `anthropic`, `ollama`, `openrouter`). Any other `provider:` value fails at run start with a configuration error.

| Provider     | Variable             | Notes                                                                                           |
| ------------ | -------------------- | ----------------------------------------------------------------------------------------------- |
| `openrouter` | `OPENROUTER_API_KEY` | One key reaches OpenAI, Anthropic, Google and open-weight models; `model:` is the OpenRouter route. |
| `openai`     | `OPENAI_API_KEY`     | Direct OpenAI.                                                                                  |
| `anthropic`  | `ANTHROPIC_API_KEY`  | Direct Anthropic.                                                                               |
| `ollama`     | none                 | Keyless. Endpoint comes from `OLLAMA_HOST` (below).                                             |

The key is read from the process env. If the name is also listed under `secrets:` and supplied through `--secrets-file`, that value wins. A missing key is a `ConfigError` (wire kind `config_error`) whose message names the variable to set.

Provider keys are **not** part of the `secrets.*` template namespace: `{{ secrets.OPENAI_API_KEY }}` renders only when the name is declared under `secrets:`. They are still redacted. For every provider a pipeline uses, the CLI adds the key from the process env to the redactor, whether or not it is declared under `secrets:`, so it is masked as `***` in events, tracing and tapes.

### `OLLAMA_HOST`

Base address of the Ollama server for `provider: ollama`. Defaults to `http://localhost:11434`.

| Value                              | Endpoint used                  |
| ---------------------------------- | ------------------------------ |
| unset                              | `http://localhost:11434/v1/`   |
| `ollama.internal:11434`            | `http://ollama.internal:11434/v1/` |
| `https://ollama.example.com`       | `https://ollama.example.com/v1/` |
| `http://host:11434/v1`             | `http://host:11434/v1/`        |

A non-loopback host logs a warning at client construction: prompts and tool-call payloads leave the machine, and over plain `http://` they travel in cleartext. A 404 from the server counts as "model not found" only when the body mentions the model, so a wrong `OLLAMA_HOST` shows up as an API error.

## Tracing

Standard `tracing-subscriber` filter. Affects only the stderr observability stream; the stdout event log is unaffected.

| Variable    | Default                                                | Description                                                                                                       |
| ----------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`  | `info` (or `debug` when `orno run --verbose` is set)   | Tracing filter directive. Standard `env_logger`/`tracing-subscriber` syntax, e.g. `RUST_LOG=orno_core=debug,info`. |

`RUST_LOG` always wins. `--verbose` only adjusts the default; it does not override an explicit `RUST_LOG`.

## Test transports

These variables exist for orno's own integration tests and for smoke runs without an API key. They only work in a binary built with the `test-transport` feature. Release binaries and a plain `cargo install` ignore them and use the real transport.

| Variable                      | Values                       | Description                                                                                                                                                                                                  |
| ----------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ORNO_TEST_LLM_TRANSPORT`     | `dummy`, `scripted`, unset   | When `dummy`, every LLM call returns a deterministic canned response (no network). When `scripted`, the transport serves the responses in the file named by `ORNO_TEST_SCRIPTED_TAPE`, in order. Any other value, or unset, selects the real transport. |
| `ORNO_TEST_SCRIPTED_TAPE`     | path                         | Required when `ORNO_TEST_LLM_TRANSPORT=scripted`. A single JSON array of `LlmResponse` objects, not JSONL.                                                                                                    |

To exercise an example pipeline without a key, build with the feature:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli --features test-transport -- run examples/hello/pipeline.yaml
```

## User-declared secrets

Names listed under the pipeline's top-level `secrets:` block are read from the process env (or `--secrets-file`) at run start and routed into the `secrets.*` template namespace. The variables themselves have no fixed names — they are whatever the pipeline declares.

```yaml
secrets:
  - GITHUB_TOKEN
  - SLACK_BOT_TOKEN
```

Reads `GITHUB_TOKEN` and `SLACK_BOT_TOKEN` from the process env (and any `--secrets-file`) and exposes them as `{{ secrets.GITHUB_TOKEN }}` and `{{ secrets.SLACK_BOT_TOKEN }}` in templates. Both are redacted on every event.

## User-declared env inputs

Names listed under `pass_env:` are pulled from the process env into the `env.*` template namespace. Same rule applies — the variable names are pipeline-specific.

```yaml
pass_env:
  - PR_NUMBER
  - CI_BUILD_ID
```

Reads `PR_NUMBER` and `CI_BUILD_ID` from the process env and exposes them as `{{ env.PR_NUMBER }}` and `{{ env.CI_BUILD_ID }}`. Visible in events and traces — never redacted.

A name listed in both `pass_env:` and `secrets:` fails validation: keep it in `secrets:` only so it stays redacted. Dotenv bindings that arrive through `--env-file` under a name declared in `secrets:` are routed into `secrets.*`.

## See also

- [Pipeline YAML › environment and secrets](pipeline-yaml.md#environment-and-secrets) — full grammar for `pass_env:` and `secrets:`.
- [CLI › `orno run`](cli.md#orno-run) — `-e`, `--env-file`, `--secrets-file` flags.
- [FAQ › how are secrets redacted?](../faq.md#how-are-secrets-redacted) — name-based classification rationale.
