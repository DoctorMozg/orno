# `pr-review` — multi-agent review with subagent delegation

A parent reviewer agent delegates to three read-only lens subagents (security, performance, docs), each running its own bounded loop, and synthesizes their findings into a single verdict object.

## Surface exercised

- Subagent delegation via `subagent.<agent-name>` in `allowed_tools`.
- Multi-agent compose-down rule: a child cannot relax `allow_mutations` / `allow_network` past its parent.
- DAG: shell node `collect_diff` feeds the agent's `initial_prompt` through `{{ nodes.collect_diff.stdout }}`.
- Stdio MCP server (`@modelcontextprotocol/server-filesystem`) for read-only filesystem search.
- `pass_env:` for the `PR_NUMBER` input.

## Inputs

- `PR_NUMBER` — the pull-request number to review (`pass_env`).
- A clean working tree at the repo root, with `origin/main` reachable for `git diff`.

## Run live

```bash
export OPENROUTER_API_KEY=sk-or-v1-...
export PR_NUMBER=482
cargo run -p orno-cli -- run examples/pr-review/pipeline.yaml
```

## Notes

- The parent has `max_subagent_depth: 1`, so each lens runs at depth 1 and cannot recurse further (each lens has its own `max_subagent_depth: 0`).
- Filesystem MCP needs `npx` and a `/workspace` mount; adjust the `command:` to match your local layout.
