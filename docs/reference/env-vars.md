# Environment variables

orno reads a small fixed set of environment variables. Variables are grouped by purpose; everything here is name-classified — a variable's role does not change based on the source file it came from.

orno does **not** auto-inherit the entire process environment into the `env.*` template namespace. Names must be opted in via `pass_env:`, `-e`, or `--env-file`. This section documents the exception list — variables orno itself reads at the runtime boundary.

## Provider credentials

Auto-pulled into the `secrets.*` namespace when an agent's `provider:` matches the variable's owning provider. Always redacted to `***` on every event body, every tracing line, and every replay tape.

| Variable             | Pulled when                            | Used by                                                                                              |
| -------------------- | -------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `OPENROUTER_API_KEY` | An agent declares `provider: openrouter` | OpenRouter `LlmTransport` adapter — the default provider in v0.1.                                    |
| `OPENAI_API_KEY`     | An agent declares `provider: openai`     | Direct OpenAI transport.                                                                             |
| `ANTHROPIC_API_KEY`  | An agent declares `provider: anthropic`  | Direct Anthropic transport.                                                                          |
| `GEMINI_API_KEY`     | An agent declares `provider: gemini`     | Direct Google Gemini transport.                                                                      |
| `GROQ_API_KEY`       | An agent declares `provider: groq`       | Direct Groq transport.                                                                               |
| `XAI_API_KEY`        | An agent declares `provider: xai`        | Direct xAI transport.                                                                                |
| `DEEPSEEK_API_KEY`   | An agent declares `provider: deepseek`   | Direct DeepSeek transport.                                                                           |
| `COHERE_API_KEY`     | An agent declares `provider: cohere`     | Direct Cohere transport.                                                                             |

A pipeline using a single non-OpenRouter provider needs only the matching key; OpenRouter routes every upstream behind one key. A name listed in the pipeline's top-level `secrets:` block is also auto-pulled from the process env at run start.

## Tracing

Standard `tracing-subscriber` filter. Affects only the stderr observability stream; the stdout event log is unaffected.

| Variable    | Default                                                | Description                                                                                                       |
| ----------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `RUST_LOG`  | `info` (or `debug` when `orno run --verbose` is set)   | Tracing filter directive. Standard `env_logger`/`tracing-subscriber` syntax, e.g. `RUST_LOG=orno_core=debug,info`. |

`RUST_LOG` always wins. `--verbose` only adjusts the default; it does not override an explicit `RUST_LOG`.

## Test transports

These variables are intended for orno's own integration tests and for smoke testing without an API key. They are not part of the user-facing surface, but they are stable and documented because the dummy transport is the recommended path for `examples/hello/pipeline.yaml` without spending tokens.

| Variable                      | Values                       | Description                                                                                                                                                                                                  |
| ----------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ORNO_TEST_LLM_TRANSPORT`     | `dummy`, `scripted`, unset   | When `dummy`, every LLM call returns a deterministic canned response (no network). When `scripted`, the transport replays from a tape pointed at by `ORNO_TEST_SCRIPTED_TAPE`. Unset selects the real transport. |
| `ORNO_TEST_SCRIPTED_TAPE`     | path                         | Required when `ORNO_TEST_LLM_TRANSPORT=scripted`. Path to a JSONL tape of pre-recorded LLM responses.                                                                                                         |

`ORNO_TEST_LLM_TRANSPORT=dummy` is the recommended way to exercise an example pipeline without a key:

```bash
ORNO_TEST_LLM_TRANSPORT=dummy cargo run -p orno-cli -- run examples/hello/pipeline.yaml
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

A `pass_env:` entry whose name is also declared in `secrets:` is routed into `secrets.*`, never `env.*` — name-based classification cannot be downgraded.

## See also

- [Pipeline YAML › environment and secrets](pipeline-yaml.md#environment-and-secrets) — full grammar for `pass_env:` and `secrets:`.
- [CLI › `orno run`](cli.md#orno-run) — `-e`, `--env-file`, `--secrets-file` flags.
- [FAQ › how are secrets redacted?](../faq.md#how-are-secrets-redacted) — name-based classification rationale.
