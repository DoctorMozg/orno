# `self-review` — pipeline that reviews its own pull requests

A single-agent pipeline that reads a trusted rubric, runs `git diff` against
`origin/master`, asks Claude Sonnet 4.5 (via OpenRouter) to produce a
markdown review, and posts the review as a PR comment via `gh`.

This example backs the `dogfood-self-review` GitHub Actions workflow under
`.github/workflows/dogfood-self-review.yml`. The workflow is the canonical
runtime; this directory is the source of truth for the rubric and the
pipeline shape.

## Surface exercised

- **Read-only agent** with `allow_mutations: false`, `allow_network: false`,
  `roots: ["{{ vars.pr_repo }}"]`, and `allowed_tools: [Read]`. Demonstrates
  the strictness dimensions on the lowest-privilege agent shape.
- **Trusted vs. untrusted input separation** — the rubric and pipeline come
  from a trusted-master checkout (`vars.trusted_root`); the PR head being
  reviewed sits at `vars.pr_repo`, which is also the agent's `roots`.
- **`stdin:`** on a shell node to persist agent output without shell
  expansion over LLM-generated content.
- **`pass_env:`** to pull workflow inputs into the template namespace.
- **`secrets:`** with auto-discovery — the LLM transport finds
  `OPENROUTER_API_KEY` automatically when `provider: openrouter`.
- **Process-env inheritance** — `gh pr comment` reads `GH_TOKEN` from the
  shell child's inherited env without orno templating it.

## Inputs

Set on the calling workflow step (`env:`):

| Name           | Purpose                                                      |
| -------------- | ------------------------------------------------------------ |
| `PR_NUMBER`    | Pull-request number, used for comment posting                |
| `PR_REPO`      | Absolute path to the PR-head checkout — agent's `roots[0]`   |
| `TRUSTED_ROOT` | Absolute path to the trusted-master checkout                 |
| `GH_REPO`      | `owner/name` for `gh pr comment --repo`                      |
| `GH_TOKEN`     | Token for `gh`, inherited by the `post_comment` shell child  |

Provided via `--secrets-file`:

| Name                 | Purpose                                       |
| -------------------- | --------------------------------------------- |
| `OPENROUTER_API_KEY` | Auto-discovered by the LLM transport          |

## Run live

```bash
export PR_NUMBER=482
export PR_REPO=$(pwd)
export TRUSTED_ROOT=$(pwd)
export GH_REPO=DoctorMozg/orno
export GH_TOKEN="$(gh auth token)"

echo "OPENROUTER_API_KEY=$OPENROUTER_API_KEY" > .env.secrets
chmod 600 .env.secrets

cargo run -p orno-cli -- run examples/self-review/pipeline.yaml \
  --secrets-file .env.secrets

rm .env.secrets
```

For local runs you can point `TRUSTED_ROOT` and `PR_REPO` at the same
checkout — the pipeline does not enforce that they differ. The CI workflow
keeps them separate because that's where the trust boundary actually lives.

To skip the comment posting locally (e.g. when `gh` is not authenticated),
remove the `post_comment` node from the YAML or run on a branch where you
genuinely want the comment.

## Trust model

The workflow runs on `pull_request` events. The pipeline file and rubric
come from the trusted-master clone — never from the PR head — and the orno
binary is installed from a pinned action release, never built from the PR
commit. The reviewer agent's tool surface is `Read` only, with mutations
and network denied.

A malicious PR therefore cannot:

- patch the rubric or pipeline (loaded from trusted master),
- patch the orno runtime (installed from a pinned tag),
- write to the filesystem or call the network (policy denies both),
- exfiltrate secrets via templates (the trusted YAML never templates
  `{{ secrets.* }}` into agent prompts; the only secret is the OpenRouter
  key consumed at the transport boundary, never rendered),
- break out of the read-jail (`roots` is the PR-head checkout).

Forks are excluded by the workflow's `if:` guard — secrets are not exposed
to fork PRs anyway, but the explicit guard makes the boundary visible.

## See also

- [`docs/how-to/pass-secrets.md`](../../docs/how-to/pass-secrets.md) — the
  secret lifecycle this example builds on.
- [`examples/pr-review/`](../pr-review/) — the larger multi-agent variant
  with security/performance/docs lens subagents.
- [`.github/workflows/dogfood-self-review.yml`](../../.github/workflows/dogfood-self-review.yml)
  — the workflow that runs this pipeline on every internal PR.
