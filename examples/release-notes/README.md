# `release-notes` — three-node DAG with heterogeneous kinds

Generates a `CHANGELOG.md` between two git tags using a three-stage DAG: a shell node collects the commit range, a networked enricher fetches PR metadata, and a mutating synthesizer writes the file.

## Surface exercised

- Heterogeneous `kind:` per node (`shell` → `agent` → `agent`).
- Per-agent policy separation: `issue_enricher` is networked + read-only; `notes_synthesizer` is mutating + offline.
- A tight `allowed_domains: [api.github.com]` on the enricher.
- Cross-node templating: each agent reads its predecessor via `{{ nodes.<id>.output }}` (or `.stdout` for shell).

## Inputs

- `PREV_TAG` — start of the changelog range (`pass_env`).
- `CURR_TAG` — end of the range (`pass_env`).
- `GITHUB_REPO` — `owner/repo` slug for `api.github.com/repos/<repo>/pulls/<NNN>` lookups (`pass_env`).

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
export PREV_TAG=v0.4.0
export CURR_TAG=v0.5.0
export GITHUB_REPO=acme/widgets
cargo run -p orno-cli -- run examples/release-notes/pipeline.yaml
```

## Output

The synthesizer writes `CHANGELOG.md` at the repo root (the path is the `vars.output_path` default). Override with `vars:` overrides at the top of the pipeline if you want a different destination.

## Notes

- The enricher has a generous `max_tool_calls: 150` budget because each PR in the range costs one `WebFetch`. Bring the cap down for small ranges.
- The synthesizer's `on_parse_error: fail` means a malformed JSON intermediate from the enricher terminates the run loudly rather than producing a half-rendered CHANGELOG.
