# `commit-digest` — every builtin tool, end to end

A weekly commit digest pipeline that exercises every builtin tool handler at least once. Useful as an integration smoke test of the tool surface.

## Surface exercised

- Builtins: `Bash`, `Read`, `WebFetch`, `Edit`, `Write` — each invoked at least once in a fixed-order recipe.
- Subagent delegation: parent calls `subagent.contributor_vibes` (depth 1, then no recursion).
- Two-stage DAG: status writer → next-steps planner, with a final shell node persisting the planner's output.
- A `seed_tracker` shell node that prepares the tracker file with a sentinel line so the agent's `Edit` has a unique `old_string` to match.

## Inputs

- `OPENROUTER_API_KEY` — provider credential, auto-discovered from process env or `.env.secrets`.
- A git repo at the working directory with at least one commit (so `git log` and `git shortlog` return content).
- Network access to `api.github.com`, `github.com`, and `raw.githubusercontent.com` for `WebFetch`.

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
cargo run -p orno-cli -- run examples/commit-digest/pipeline.yaml
```

## Output

- `target/orno-digest/digest.md` — the digest paragraph + top contributors + vibe line.
- `target/orno-digest/tracker.md` — single-line tracker stamped with today's UTC date.
- `target/orno-digest/next-steps.md` — three numbered next steps from the planner subagent.

## Notes

- The parent's `max_subagent_depth: 1` is deliberate. The child agent (`contributor_vibes`) sets `max_subagent_depth: 0` for itself, so a child-of-child call would trip the depth gate and feed back a denial — which the parent handles by inlining the vibe line itself.
- If the recent commit log lacks a GitHub PR URL, the agent falls back to `https://api.github.com/zen` so `WebFetch` is still exercised.
