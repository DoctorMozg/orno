# `scoped-state` — `SetState` and downstream state reads

An agent triages a bug report, publishes structured fields via `SetState`, and a downstream shell node renders those fields into a triage summary file without re-parsing the assistant's free-form reply.

## Surface exercised

- `SetState` builtin tool — writes `nodes.<self>.state.<key>`.
- `policy.allow_context_writes: true` — the opt-in policy gate that lets the agent call `SetState` at all.
- Downstream template access via `{{ nodes.<id>.state.<key> }}` — sibling to the existing `nodes.<id>.output` slot.

## Inputs

- `OPENROUTER_API_KEY` — provider credential.

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
cargo run -p orno-cli -- run examples/scoped-state/pipeline.yaml
```

## Output

- `target/orno-scoped-state/triage-summary.md` — the rendered summary, with `severity`, `category`, `next_action` pulled directly from the agent's published state plus the agent's one-sentence summary.

## Notes

- The agent has a one-tool surface: only `SetState`. It can publish state but cannot edit or write any other file. The shell node downstream is what actually persists the triage to disk.
- Each `{{ nodes.triage.state.<key> }}` render is the exact JSON value the agent handed to `SetState`, not a regex over its final assistant message.
- This example exercises the happy path. Oversize-payload (`StateTooLarge`) and malformed-key rejection are covered by unit tests rather than here.
