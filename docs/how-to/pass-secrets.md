# How to pass secrets

orno's secret model is strict by design. Secrets are loaded once, available only inside templates that explicitly reference them, redacted from every event and tape, and never injected into the host environment.

## Quick reference

```yaml
# pipeline.yaml
secrets:
  - OPENROUTER_API_KEY
  - GITHUB_TOKEN

agents:
  reviewer:
    provider: openrouter           # key read from OPENROUTER_API_KEY
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

The list is the **allowlist** of secret names the pipeline acknowledges. An unrelated process env var is ignored, and a `--secrets-file` key that is not listed is an error.

A name in `secrets:` doesn't need a value to be present. Unused secrets are loaded into memory but never rendered, logged or recorded, so they're harmless if absent. That makes optional credentials optional without code changes.

### 2. Provide the values

Two sources, in priority order:

**`--secrets-file path.env`** is a `KEY=VALUE` file:

```
OPENROUTER_API_KEY=sk-or-v1-abc123
SCANNER_TOKEN=tok_xyz789
```

The file is parsed once at run start. Lines starting with `#` are comments. Values may be quoted (`KEY="value with spaces"`) but don't have to be.

**Process environment** is the fallback when no file is given. If `OPENROUTER_API_KEY` is exported in the shell, orno picks it up.

Prefer the file, for two reasons. It scopes the secret to one run instead of the shell session, and it lets the file live outside the working tree (e.g. `/run/secrets/` mounted into a container) without leaking into the host shell.

### 3. Render in templates

Secrets render only inside MiniJinja templates that explicitly reference them:

```yaml
agents:
  api_caller:
    system: "You have a token: {{ secrets.SCANNER_TOKEN }}"
    # ... or in initial_prompt, or in MCP server bearer token, or in headers, etc.
```

They are **not** in `vars.*` or `env.*`. A template that wants a secret writes `{{ secrets.NAME }}` and nothing else.

Provider keys (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) are read at the transport boundary when the agent's `provider:` matches, so you don't template them into the agent config. They're redacted whether or not you declare them under `secrets:`.

### 4. Redaction at emission

Before any event envelope hits stdout (or any tracing line hits stderr, or any tape is recorded), every rendered string is scanned for known secret values. Every match is replaced with `***`.

This applies to:

- **Event bodies**: content excerpts, tool arguments, tool results, error messages.
- **Tracing logs**: internal observability lines.
- **LLM tapes**: recorded request/response bodies.
- **Tool tapes**: recorded tool call/result pairs.
- **MCP tapes**: recorded MCP exchanges.

If the model echoes a secret in its content (say it parrots the token back in a reply), that is redacted too. The model sees the *original* value. The audit log sees `***`.

## Argv scrubbing

When a tool, typically `Bash`, uses a secret as a CLI argument, the secret is scrubbed from the `input_excerpt` of `tool_call_recorded` before the event is emitted:

```yaml
- id: deploy
  kind: shell
  command: deploy.sh
  args: ["--token", "{{ secrets.DEPLOY_TOKEN }}"]
```

The shell process receives the actual token. The event log shows `***` in its place.

This is a separate code path from string-content redaction because the secret value is known at template-render time. The redactor matches by value, including encoded forms (base64, hex, percent-encoded, JSON-escaped).

## Auto-discovery for LLM providers

You do **not** need to template provider keys into the agent config. orno's transport layer reads them at request time (from the process env, or from `--secrets-file` when the name is declared under `secrets:`):

| Provider key                                 | Recognized when an agent has...                                 |
| -------------------------------------------- | ---------------------------------------------------------------- |
| `OPENROUTER_API_KEY`                         | `provider: openrouter`                                           |
| `ANTHROPIC_API_KEY`                          | `provider: anthropic`                                            |
| `OPENAI_API_KEY`                             | `provider: openai`                                               |
| `OLLAMA_HOST` (address, not a key)            | `provider: ollama`                                               |

If the key is missing, the call fails with a `config_error` that names the env var to set. A key the provider rejects is `auth_failed`. `ollama` needs no key.

## What does *not* leak

- Secrets are not in `env.*`. The secrets file looks like `KEY=VALUE`, but its values are not exposed through the `env.*` template namespace. A name listed in both `secrets:` and `pass_env:` fails validation, so keep it in `secrets:` only.
- Secrets are not in the host environment. Tools spawned by orno (like `Bash`'s subprocess) inherit only the env variables you explicitly pass via the tool's argv or `env:` block, never the secret store.
- Secrets are not in replay tapes. A bundle replays with `***` in place of every recorded secret value. The model and tools see the redacted form during replay, and the original credentials are gone.

## What still leaks

- **The structure of the call.** A redacted bundle still records that an HTTP request was made to a particular URL, with a header named `Authorization` of redacted value. The shape of the credential (bearer vs. basic vs. custom) is visible.
- **Network egress.** Setting `allow_network: false` denies `WebFetch` and MCP, but a `Bash` invocation with `allow_mutations: true` can still reach the network. orno does not network-namespace tool subprocesses. For OS-level egress control, run inside a container.
- **Side-channel inference.** Token-count fields and timing patterns are recorded verbatim. If your threat model includes side-channels, treat the bundle as sensitive and apply additional scrubbing before sharing.

## Redaction limitations

The redactor matches secret values as byte sequences in every emitted event, log line, and tape entry, and replaces each match with `***`. Alongside the raw value it registers the encodings a tool or model is likely to produce:

- base64 in the standard, unpadded, and URL-safe alphabets, including the forms that appear when the secret sits inside a larger encoded blob such as `Authorization: Basic <base64(user:secret)>`
- lowercase and uppercase hex
- percent-encoding, with either hex case
- the JSON-escaped form, so `pass"word` is caught as `pass\"word`
- each line of a multi-line secret, such as a PEM key, on its own.

Derived patterns shorter than 8 bytes are dropped, because a short fragment would redact ordinary text. JSON object keys are redacted as well as values.

Some forms still get through:

- **Hashed or otherwise transformed.** A tool that emits a SHA-256 digest or an HMAC of the secret leaks that output verbatim. It isn't the secret, but it is a stable identifier and may be sensitive on its own.
- **Mixed-case hex and other encodings.** Only all-lowercase and all-uppercase hex are registered. Other schemes, such as base32 or a custom escape, aren't recognized.
- **Split across event boundaries.** A secret whose bytes straddle two separately emitted events isn't reassembled, because the redactor sees each event in isolation. orno emits each LLM response as one envelope after the model finishes, so this rarely happens. A streaming consumer that re-splits the bytes can bring the gap back.
- **Empty-string secrets.** A `secrets:` entry with an empty value is dropped when the redactor is built. An empty matcher would replace every zero-width position and corrupt all output.
- **Normalization mismatches.** The redactor compares bytes, not Unicode-normalized forms. A secret with combining characters that the model normalizes differently (NFC vs. NFD) won't match.

For any of these, declare the transformed form as its own entry in `secrets:`, or scrub it at the call site. Treat a tape committed to a repository or shared with a third party as sensitive even after redaction.

## Recipe 1: single LLM provider

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

## Recipe 2: multiple LLM providers in one pipeline

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

Both keys auto-discover, so no templating is needed.

## Recipe 3: passing a token to a tool

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

The token is rendered into the prompt verbatim and the model emits a `WebFetch` call with the token in the header. The event log shows `***` everywhere the token appeared.

## Recipe 4: bearer auth on an MCP server

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

The token renders into the `Authorization: Bearer <token>` header at request time. The bundle records the template, never the value.

## Recipe 5: running in CI

GitHub Actions:

```yaml
# .github/workflows/agent.yml
- name: Run pipeline
  env:
    OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
  run: |
    orno run pipeline.yaml
```

orno's process-env fallback picks up the exported secret, so there's no need for `--secrets-file`.

For multiple secrets, write them to a file instead:

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

- [Pipeline YAML › `secrets`](../reference/pipeline-yaml.md#secrets-credentials-ambient--file-override): every field.
- [Security › Secret handling](../security.md#secret-handling): full lifecycle including redaction internals.
- [Environment variables](../reference/env-vars.md): every env var orno reads.
