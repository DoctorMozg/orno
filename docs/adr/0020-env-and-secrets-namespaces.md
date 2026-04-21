# ADR 0020 — Env and secrets as two template namespaces

- Status: accepted
- Date: 2026-04-21

## Context

Pipeline YAML currently references external values via a single `{{ env.* }}`
template namespace that resolves from the process environment. The examples
in `examples/` already mix two distinct concerns under that single name:

- **Pipeline inputs** — `TEST_PATH`, `CI_LOG_URL`, `PR_NUMBER`, `CURR_TAG`,
  and friends. They parameterize a run, end up interpolated into prompts
  and shell args, and appear verbatim in emitted events.
- **Credentials** — `OPENROUTER_API_KEY` (needed to initialize the LLM
  transport), `GITHUB_TOKEN` (passed through to the MCP GitHub server's
  `env:` block), and similar. They must never appear in events, in tracing
  above `debug!`, or in replay tapes.

One namespace for both is a latent leak. Any template author who writes
`{{ env.GITHUB_TOKEN }}` into a prompt — or any future feature that logs
the rendered template context — exposes the token. The "no secrets in
logs above `debug!`" rule in `CLAUDE.md` is a reminder, not a mechanism.

Additionally, the current implicit auto-inherit of the entire process
environment into `env.*` makes runs silently nondeterministic across
machines: a `$HOME` or `$TMPDIR` change flows into template context
without a declaration, which contradicts ADR 0005's fifth strictness
dimension (bounded non-determinism).

## Decision

Two disjoint template namespaces — `env.*` and `secrets.*` — with distinct
resolution rules, distinct precedence, and a redaction contract on the
event stream.

### `env.*` — pipeline inputs, opt-in only

Sources, highest precedence wins:

1. `-e KEY=VAL` on the CLI — single inline override.
2. `--env-file .env.inputs` — bulk load (dotenv format: `KEY=VAL` per
   line, `#` comments).
3. `pass_env: [NAMES]` top-level YAML block — pulls the named keys from
   the process env at run start.

The process environment does **not** auto-populate `env.*`. A name must
be explicitly listed via one of the three sources to be resolvable.
Missing references at render time are a hard error
(`TemplateError::UnknownVariable`), not a silent empty string.

Values in `env.*` are visible:

- Expand into prompts, shell args, node `initial_prompt`s, and any other
  templated string.
- Appear verbatim in emitted events.
- Logged at any tracing level without redaction.

### `secrets.*` — credentials, ambient + file override

Sources, highest precedence wins:

1. **Process env** for two populations:
   - **Provider-known names** — the LLM transport and MCP client know
     which env var each provider needs (`OPENROUTER_API_KEY` for
     `openrouter`, `ANTHROPIC_API_KEY` for `anthropic`, etc.). When a
     pipeline references a provider, its env var is auto-pulled as a
     secret; no YAML declaration is required.
   - **User-declared names** — the top-level `secrets: [NAMES]` YAML
     block adds names to the set. Typical use is MCP server `env:`
     blocks that need a token (`GITHUB_TOKEN`, `SLACK_BOT_TOKEN`, etc.).
2. **`--secrets-file .env.secrets`** — overrides process-env values for
   any secret name present in the file.

No CLI flag for individual secret values. A secret on `argv` leaks into
shell history (`HISTFILE`), process listings (`ps aux`), and some
exec-tracing facilities. The dotenv file is the supported surface for
ad-hoc overrides.

Classification follows the **name**, not the source. If someone writes
`OPENROUTER_API_KEY=sk-...` into `.env.inputs`, the resolver routes that
binding into `secrets.*` anyway, not `env.*`. This preserves the
redaction guarantee: a name's sensitivity cannot be accidentally
downgraded by choice of source file.

Values in `secrets.*` are protected:

- Expand into templated strings only when the template explicitly
  references `{{ secrets.FOO }}`.
- Are tracked by the template engine as a set of values at render time.
- Before any event leaves the engine, a redactor scans every string
  field of the event's `data` body and replaces each tracked secret
  value with `***`. `EventSink` impls (starting with `InMemorySink`)
  see already-redacted events and do not implement redaction
  themselves.
- Tracing: the same redactor wraps the `tracing-subscriber` `MakeWriter`
  so the "no secrets above `debug!`" rule is mechanical, not
  aspirational.

### MCP subprocess env passthrough

MCP servers take an `env:` map whose values are template strings:

```yaml
mcp_servers:
  github:
    command: ["npx", "@modelcontextprotocol/server-github"]
    env:
      GITHUB_TOKEN: "{{ secrets.GITHUB_TOKEN }}"
```

The value expands once at spawn time and is handed to the subprocess via
`std::process::Command::env`. It is not retained by the orno process
beyond the spawn call, not logged, and not echoed into any event. This
is the only path by which a secret value crosses a process boundary, and
it is explicit per binding under user control.

### Transport and MCP client initialization

`LlmTransport` and `McpClient` are constructed with a typed secrets
handle, not a raw `HashMap<String, String>`. The handle's only read API
is `get(name) -> Option<&str>`; it is not `Debug`-printable (custom impl
returns `Secrets { <redacted> }`) and does not implement `Serialize`.
This blocks accidental inclusion of the secrets map in any diagnostic
artifact.

## Consequences

- **Schema change (pre-v0.1, breaking-OK)**: `Pipeline` gains two
  optional top-level fields, `pass_env: Vec<String>` and
  `secrets: Vec<String>`, both defaulting to `[]`. Regenerate
  `schemas/pipeline.schema.json` after the `schema.rs` change.
- **Example migration**: every `{{ env.GITHUB_TOKEN }}` and similar
  credential reference in `examples/*.yaml` migrates to
  `{{ secrets.GITHUB_TOKEN }}`, with a matching `secrets: [GITHUB_TOKEN]`
  at the top level. Names like `TEST_PATH`, `CI_LOG_URL`, `PR_NUMBER`,
  `PREV_TAG`, `CURR_TAG`, `GITHUB_REPO` stay in `env.*` and gain either
  a `pass_env:` declaration or switch to `-e` / `--env-file` in the
  example's invocation snippet.
- **New redaction layer in the engine**: between event production and
  sink dispatch, a `Redactor` in `orno-core` replaces tracked secret
  values with `***` in every string field of the event body. The
  redactor is owned by the engine, constructed at run start, and shared
  with the tracing layer.
- **CLI surface**: `orno run` gains `-e KEY=VAL` (repeatable),
  `--env-file <path>` (repeatable; later files shadow earlier ones),
  and `--secrets-file <path>` (repeatable; same shadowing rule).
  Missing files are a hard error — no silent fallback to default paths.
- **Determinism**: pipelines become reproducible across machines
  without a matching `.env.inputs` / `.env.secrets` pair. Process env
  can no longer silently change template context.
- **Replay**: secret values are never written to a replay tape.
  Replaying a recorded run requires the same secrets to be present at
  replay time; the tape records *that* a secret was referenced, not its
  value.

## Explicitly rejected alternatives

- **Single `env.*` with a name-suffix allowlist** (`*_TOKEN`, `*_KEY`,
  `*_SECRET` auto-redacted). Fragile: false positives on
  `JWT_TOKEN_HEADER`-style keys, false negatives on custom names like
  `PAGERDUTY_ROUTE`. Classification must be explicit.
- **Auto-inherit process env into `env.*` by default**. Convenient for
  one-off scripts, hostile to determinism and to the strict posture
  established by ADR 0005.
- **CLI `-s KEY=VAL` flag for secrets**. Leaks into shell history and
  process listings; `--secrets-file` is a strictly better surface for
  the same ergonomics.
- **Transport-layer secret resolution without a top-level `secrets:`
  block**. Works for provider-known names, but leaves user-declared
  MCP secrets (`GITHUB_TOKEN`, etc.) without a classification path. The
  top-level block makes the full set explicit at pipeline-load time.
