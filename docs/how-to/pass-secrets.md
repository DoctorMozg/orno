# How to pass secrets

orno's secret model is strict by design: secrets are loaded once, available only inside templates that explicitly reference them, redacted from every event and tape, and never injected into the host environment.

## Quick reference

```yaml
# pipeline.yaml
secrets:
  - OPENROUTER_API_KEY
  - GITHUB_TOKEN

agents:
  reviewer:
    provider: openrouter           # OPENROUTER_API_KEY auto-discovered
    allowed_tools: [WebFetch]
    # ...

nodes:
  - id: fetch
    kind: agent
    agent: reviewer
    initial_prompt: |
      Use this token for the API call: {{ secrets.GITHUB_TOKEN }}
```

```bash
# .env.secrets
OPENROUTER_API_KEY=sk-or-v1-...
GITHUB_TOKEN=ghp_...
```

```bash
orno run pipeline.yaml --secrets-file .env.secrets
```

## The four-step lifecycle

### 1. Declare in the pipeline

```yaml
secrets:
  - OPENROUTER_API_KEY
  - SCANNER_TOKEN
```

Names are normalized to UPPER_SNAKE_CASE. The list is the **allowlist** of secret names the pipeline acknowledges; an unrelated env var or unrelated entry in the secrets file is ignored.

A name in `secrets:` does not require a value to be present. Unused secrets are loaded into memory but never rendered, never logged, never recorded — they are harmless if absent. Use this to make optional credentials optional without code changes.

### 2. Provide the values

Two sources, in priority order:

**`--secrets-file path.env`** — a `KEY=VALUE` file:

```
OPENROUTER_API_KEY=sk-or-v1-abc123
SCANNER_TOKEN=tok_xyz789
```

The file is parsed once at run start. Lines starting with `#` are comments. Values may be quoted (`KEY="value with spaces"`) but are not required to be.

**Process environment** — fallback when no file is given. If `OPENROUTER_API_KEY` is exported in the shell, orno picks it up.

The file source is preferred for two reasons: it scopes the secret to one run instead of the shell session, and it allows the file to live outside the working tree (e.g. `/run/secrets/` mounted into a container) without leaking into the host shell.

### 3. Render in templates

Secrets render only inside MiniJinja templates that explicitly reference them:

```yaml
agents:
  api_caller:
    system: "You have a token: {{ secrets.SCANNER_TOKEN }}"
    # ... or in initial_prompt, or in MCP server bearer token, or in headers, etc.
```

They are **not** in `vars.*` or `env.*`. A template that wants a secret writes `{{ secrets.NAME }}` and nothing else.

Provider-specific keys (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`) are auto-discovered when the agent's `provider:` matches. You do not need to template them into the agent config — they're picked up at the transport boundary.

### 4. Redaction at emission

Before any event envelope hits stdout (or any tracing line hits stderr, or any tape is recorded), every rendered string is scanned for known secret values. Every match is replaced with `[REDACTED]`.

This applies to:

- **Event bodies** — content excerpts, tool arguments, tool results, error messages.
- **Tracing logs** — internal observability lines.
- **LLM tapes** — recorded request/response bodies.
- **Tool tapes** — recorded tool call/result pairs.
- **MCP tapes** — recorded MCP exchanges.

A secret accidentally echoed by the model in its content (e.g. it parrots back the token in its reply) is also redacted. The model's view is the *original* value; the audit log's view is `[REDACTED]`.

## Argv scrubbing

When a tool — typically `Bash` — uses a secret as a CLI argument, the secret is scrubbed from `tool_invoked.arguments` before the event is emitted:

```yaml
- id: deploy
  kind: shell
  command: deploy.sh
  args: ["--token", "{{ secrets.DEPLOY_TOKEN }}"]
```

The shell process receives the actual token; the event log shows `["--token", "[REDACTED]"]`.

This is a separate code path from string-content redaction because the secret value is known at template-render time. The runtime scrubs by reference, not just by string match.

## Auto-discovery for LLM providers

You do **not** need to template provider keys into the agent config. orno's transport layer reads them from the secret store at request time:

| Provider key                                 | Recognized when an agent has...                                 |
| -------------------------------------------- | ---------------------------------------------------------------- |
| `OPENROUTER_API_KEY`                         | `provider: openrouter`                                           |
| `ANTHROPIC_API_KEY`                          | `provider: anthropic`                                            |
| `OPENAI_API_KEY`                             | `provider: openai`                                               |
| `GEMINI_API_KEY`                             | `provider: gemini`                                               |
| `GROQ_API_KEY`                               | `provider: groq`                                                 |
| `XAI_API_KEY`                                | `provider: xai`                                                  |

If the secret is missing at request time, the LLM call fails with `LlmFailure::AuthFailed`.

## What does *not* leak

- Secrets are not in `env.*`. Even though the secrets file looks like `KEY=VALUE`, the values are not exposed via the `env.*` template namespace. A pipeline that wants a value as both a secret and a regular env var must declare it in both `secrets:` and `pass_env:` and provide the value through the appropriate channel.
- Secrets are not in the host environment. Tools spawned by orno (like `Bash`'s subprocess) inherit only the env variables you explicitly pass via the tool's argv or `env:` block, never the secret store.
- Secrets are not in replay tapes. A bundle replays with `[REDACTED]` in place of every recorded secret value. The model and tools see the redacted form during replay; the original credentials are gone.

## What still leaks

- **The structure of the call.** A redacted bundle still records that an HTTP request was made to a particular URL, with a header named `Authorization` of redacted value. The shape of the credential (bearer vs. basic vs. custom) is visible.
- **Network egress.** Setting `allow_network: false` denies `WebFetch` and MCP, but a `Bash` invocation with `allow_mutations: true` can still reach the network. orno does not network-namespace tool subprocesses. For OS-level egress control, run inside a container.
- **Side-channel inference.** Token-count fields and timing patterns are recorded verbatim. If your threat model includes side-channels, treat the bundle as sensitive and apply additional scrubbing before sharing.

## Redaction limitations

The redactor matches secret values as **literal byte sequences** in event strings. It scans every emitted event, log line, and tape entry for the exact UTF-8 of each declared secret and replaces matches with `***`. This is fast, allocation-free in the no-match case, and immune to most accidental leaks where a secret was rendered into a prompt or echoed by the model.

A literal matcher does **not** see a secret that has been transformed before emission. The following forms bypass redaction because they no longer contain the original byte sequence:

- **Base64-encoded.** A `Basic` HTTP auth header is `Authorization: Basic <base64(user:secret)>`. The base64 form is a different string from the secret; a tool that emits the base64 will leak it. Treat `secrets:` as the pre-encoding name and either declare both forms (`SECRET_RAW` and `SECRET_B64`) or perform the encoding inside a tool that does not echo its argv.
- **URL-encoded.** A secret containing `=`, `+`, `/`, or non-ASCII bytes that is concatenated into a query string (e.g. `?token=abc%2B123`) will reach the event log as the percent-encoded form, which the redactor will not recognize.
- **Hex-encoded or hashed.** A tool that hashes the secret (e.g. SHA-256 of an API key for HMAC signing) and emits the hash digest leaks the digest verbatim. The digest is not the secret, but it is a stable identifier and may be sensitive on its own.
- **JSON-escaped with embedded quotes or backslashes.** A secret like `pass"word` round-trips through JSON as `"pass\"word"`, which the literal matcher will not catch when the haystack is a JSON string. (A redactor-aware emission path that JSON-escapes the redaction inputs as well would close this gap; today it is not implemented.)
- **Split across event boundaries.** A secret whose bytes happen to straddle two separately-emitted events (e.g. a streamed LLM response chunked at an inopportune offset) is not reassembled; the redactor sees each event in isolation. orno emits LLM responses as a single envelope after the model finishes, so this rarely occurs in practice — but a bespoke streaming consumer that re-splits the bytes can re-introduce the gap.
- **Empty-string secrets.** A `secrets:` entry whose value is empty is dropped at redactor construction time. An empty matcher would replace every zero-width position in every string, corrupting all output.
- **Normalization mismatches.** The redactor compares bytes, not Unicode-normalized forms. A secret containing combining characters that the model normalizes differently (NFC vs. NFD) will not match.

The mitigation in every case is the same: declare every form the secret can take in the `secrets:` list, or scrub the transformed form at the call site (e.g. an MCP server that base64-encodes `Authorization` should be invoked via the `auth.kind: bearer` block whose token field is redactor-aware, not via a manually templated header). When in doubt, treat any tape committed to a repository or shared with a third party as sensitive even after redaction.

## Recipe 1 — single LLM provider

The minimum:

```yaml
# pipeline.yaml
secrets:
  - OPENROUTER_API_KEY

agents:
  greeter:
    model: openai/gpt-5
    provider: openrouter
    # ... no template reference needed
```

```bash
echo 'OPENROUTER_API_KEY=sk-or-v1-...' > .env.secrets
orno run pipeline.yaml --secrets-file .env.secrets
```

## Recipe 2 — multiple LLM providers in one pipeline

```yaml
secrets:
  - OPENROUTER_API_KEY
  - ANTHROPIC_API_KEY

agents:
  cheap_model:
    provider: openrouter        # uses OPENROUTER_API_KEY

  premium_model:
    provider: anthropic         # uses ANTHROPIC_API_KEY
```

Both keys auto-discover; no templating needed.

## Recipe 3 — passing a token to a tool

```yaml
secrets:
  - GITHUB_TOKEN

agents:
  github_caller:
    allowed_tools: [WebFetch]
    policy:
      allow_network: true
      allowed_domains: ["api.github.com"]

nodes:
  - id: call
    kind: agent
    agent: github_caller
    initial_prompt: |
      Fetch https://api.github.com/repos/owner/repo with header
      Authorization: Bearer {{ secrets.GITHUB_TOKEN }}
```

The token is rendered into the prompt verbatim, the model emits a `WebFetch` call with the token in the header, and the event log shows `[REDACTED]` everywhere the token appeared.

## Recipe 4 — bearer auth on an MCP server

```yaml
secrets:
  - MCP_TOKEN

mcp_servers:
  internal:
    transport: http
    url: "https://internal.mcp/api"
    auth:
      kind: bearer
      token: "{{ secrets.MCP_TOKEN }}"
```

The token renders into the `Authorization: Bearer <token>` header at request time; the bundle records `[REDACTED]`.

## Recipe 5 — running in CI

GitHub Actions:

```yaml
# .github/workflows/agent.yml
- name: Run pipeline
  env:
    OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
  run: |
    orno run pipeline.yaml
```

The shell-exported secret is auto-discovered by orno's process-env fallback. No `--secrets-file` needed.

For multiple secrets, prefer materializing a file:

```yaml
- name: Run pipeline
  run: |
    cat > .env.secrets <<EOF
    OPENROUTER_API_KEY=${{ secrets.OPENROUTER_API_KEY }}
    GITHUB_TOKEN=${{ secrets.GITHUB_TOKEN }}
    EOF
    orno run pipeline.yaml --secrets-file .env.secrets
    rm -f .env.secrets
```

## See also

- [Pipeline YAML › `secrets`](../reference/pipeline-yaml.md#secrets) — every field.
- [Security › Secret handling](../security.md#secret-handling) — full lifecycle including redaction internals.
- [Environment variables](../reference/env-vars.md) — every env var orno reads.
