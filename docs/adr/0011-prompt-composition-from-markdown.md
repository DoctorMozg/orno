# ADR 0011 — Prompt composition via MiniJinja file includes

- Status: accepted
- Date: 2026-04-21

## Context

Prompts in v0.1.0 are multi-line YAML strings under `agents.*.system`
and `nodes[*].initial_prompt`. At modest sizes this works; past roughly
30 lines it breaks down:

- The YAML file stops being reviewable — the DAG shape is drowned in
  prose.
- Editor tooling (markdown preview, syntax highlight, spellcheck,
  prose linters) does not understand YAML-embedded markdown.
- Shared preambles — tool-use discipline, output-format contracts,
  shared lens personas — have no composition primitive and get
  copy-pasted between agents, drifting over time.
- Prompt-engineering review and pipeline-shape review end up touching
  the same file for unrelated reasons, and commit diffs conflate both.

Three options:

1. Keep prompts inline; extract with YAML anchors for shared fragments.
   Anchors work but are fragile (ordering matters, no nested
   composition, tooling barely supports them) and YAML is still not
   markdown.
2. Introduce a new orno-specific file-reference syntax —
   `initial_prompt: "@prompts/review.md"` or
   `initial_prompt: { file: "prompts/review.md" }`. Simple; adds a
   convention users must learn; doesn't compose recursively without
   more syntax.
3. Reuse MiniJinja, which is already in the template stack per
   `docs/yaml-spec.md`. Its `{% include %}` directive reads another
   file and renders it as a template in the current context. Nested
   composition (markdown-including-markdown) falls out for free.

## Decision

- Prompt composition is done via MiniJinja's `{% include %}` directive.
  The loader is rooted at the directory of the pipeline YAML file.
  Example:

  ```yaml
  agents:
    pr_reviewer:
      system: "{% include 'prompts/reviewer-system.md' %}"
      ...

  nodes:
    - id: review
      kind: agent
      agent: pr_reviewer
      initial_prompt: "{% include 'prompts/review-initial.md' %}"
  ```

- Fields that accept `{% include %}` (same list that accepts template
  rendering today):
  - `agents.*.system`
  - `nodes[*].initial_prompt`
  - `nodes[*].command` / `args` (shell nodes) — technically supported
    but discouraged; a shell command assembled via `include` is a code
    smell. Use a script file and invoke it instead.
  - Other templated fields (`mcp_servers.*.command`, `url`, `env.*`,
    `auth.token`) accept `include` mechanically but should not use it
    in practice. These are short configuration strings, not prose.
- **Included files are themselves templates.** The full template
  context — `vars`, `env`, `secrets`, `nodes.<id>.output` — is
  available inside any included file. A single render pass resolves
  the whole tree. This is the key simplification over option 2: no
  separate "file-load then template" step.
- **Nested includes.** An included file may `{% include %}` further
  files. Recommended convention: shared preambles live under
  `prompts/common/` (e.g., `prompts/common/tool-use-discipline.md`,
  `prompts/common/output-contract.md`). MiniJinja detects circular
  includes at render time and errors cleanly.
- **Escape hatch for literal `{{` / `{%`.** Prompt text that contains
  literal double-braces or statement tags (for example, a prompt that
  teaches Jinja syntax to the model, or one that embeds another
  template language) wraps the section in `{% raw %}...{% endraw %}`.
  This is a real sharp edge and is documented with an example in
  `docs/yaml-spec.md`.
- **Path sandboxing.** The loader rejects any path that escapes the
  pipeline YAML's directory tree — no absolute paths, no `..` that
  climbs above the root, no symlinks followed outside the tree. This
  matches how `mcp_servers.filesystem` sandboxes its roots; the
  template loader is the YAML-side equivalent. Cross-project shared
  prompt libraries are not a v0.1.0 feature — vendor the `prompts/`
  directory (git submodule, checked-in copy, package extraction) into
  the pipeline repo.
- **Resolution timing.** All template rendering — inline strings and
  included files — happens once at pipeline load, before any
  `AgentStarted` event. A missing include file, a circular include,
  an undeclared template variable, or an out-of-sandbox path all fail
  pipeline load and return exit code `1` (per ADR 0010). There is no
  silent fallback.
- **`orno validate` exercises the full tree.** Validation renders every
  templated string with a representative context (empty `nodes.*.output`,
  `env` from the current process, `vars` from the pipeline) so missing
  include files and undeclared variables surface before any network
  call.

## Consequences

- Zero new CLI surface. No `--include-dir` flag, no new schema field,
  no new validator step beyond the existing template render.
- The YAML file becomes the DAG shape and policy declaration;
  `.md` files become the agent prose. Reviews split cleanly —
  scheduling and security review the YAML, prompt engineering review
  touches the `.md` files.
- Composable shared preambles are now trivial: a
  `prompts/common/output-contract.md` file included at the top of every
  lens prompt keeps the "return JSON array, no prose" contract in one
  place.
- Users who want templating inside prompts get it without importing a
  new language — `{{ vars.pr_number }}` and `{{ nodes.X.output }}` work
  identically inline and in an included file.
- Prompt files with literal `{{` or `{%` require `{% raw %}` fences.
  This surprises exactly once per team and is caught at load time, not
  at runtime.
- Cross-project shared prompt libraries are explicitly rejected for
  v0.1.0. Teams that need one vendor their library into each consuming
  repo. Post-v0.1 may revisit with a `--include-dir` CLI flag or a
  registered-root config; the one-way-door here is the sandbox rule,
  which can safely relax later but cannot safely tighten.
- MiniJinja's auto-escape remains **disabled** (per `docs/yaml-spec.md`).
  Prompts are plain text sent to LLMs, not HTML rendered to browsers,
  so the HTML-escape default would be wrong.
- Auto-inferred `needs:` from `{{ nodes.X.output }}` references inside
  included files is still out of scope (per `docs/yaml-spec.md`'s
  v0.1.0 non-shipping list). Users declare `needs:` explicitly whether
  the reference is inline or in an include.
