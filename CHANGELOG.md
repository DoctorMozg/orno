# Changelog

All notable changes to orno are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once the first tagged release ships.

## Unreleased

### Bug fixes

- Regenerated `schemas/pipeline.schema.json` to include `roots`,
  `max_message_history_bytes`, and `max_tool_output_bytes` on `AgentPolicy`.
  These three fields landed in 0.1.1 but the regenerated schema was not
  committed, so IDE yaml-language-server users saw stale autocomplete
  for one release.

### Other changes

- CI `install-smoke` now enforces a schema drift gate (`orno schema`
  must match the checked-in `schemas/pipeline.schema.json`) and validates
  every `examples/*/pipeline.yaml` rather than only `hello`.
- New CI `dogfood` job runs `examples/hello/pipeline.yaml` end-to-end
  through the binary using `ORNO_TEST_LLM_TRANSPORT=dummy`, asserting a
  `run_finished` event with `ok: true`. Closes the gap between
  "validates" and "actually runs" for the user-facing `orno run` path.
- Replay-bundle golden test (`crates/orno-cli/tests/replay_goldens.rs`)
  pins `orno replay tests/bundles/hello.ndjson` to an `insta` YAML
  snapshot of the redacted event stream. The committed bundle is the
  immutable input; a snapshot diff signals one of three drifts that
  the existing fresh-record-every-test integration tests cannot catch:
  bundle reader/writer format change, `EventEnvelope` shape change, or
  engine ordering drift. Regenerate via `cargo insta accept` only after
  confirming the diff is intentional.
- New `examples/self-review/` pipeline (rubric + YAML + README) plus a
  `dogfood-self-review` CI workflow that runs orno against every internal
  PR. The reviewer agent is single-shot Claude Sonnet 4.5 via OpenRouter,
  Read-only, with mutations and network denied at the policy level and
  `roots` jailed to the PR-head checkout. Two-checkout pattern (trusted
  master + PR head) prevents a malicious PR from rewriting either the
  pipeline YAML or the rubric; the orno binary itself is installed from
  the pinned `DoctorMozg/orno@v0.1.1` release tag, never built from the
  PR commit. Forks are excluded by an explicit `if:` guard. The verdict
  step parses `VERDICT: PASS` / `VERDICT: FAIL` from the produced
  `.orno-self-review.md` and gates the PR check accordingly.

## 0.1.1 - 2026-04-28

### Breaking changes

- **`roots` field required for file tools** (`Read`, `Write`, `Edit`). Any pipeline agent that
  uses these tools must now declare a `roots:` list in its `AgentPolicy`. Agents that omit
  `roots:` will receive a `ToolError::PolicyViolation` on every file-tool call. Migration:
  add `roots: ["/path/to/project"]` to the agent's policy block in your YAML.

- **`ORNO_TEST_LLM_TRANSPORT` env var gated behind `test-transport` feature**. The escape hatch
  that allowed tests to inject a scripted transport via the environment variable is now only
  compiled when `--features test-transport` is passed to `cargo`. CLI integration tests that
  rely on this mechanism must add `--all-features` to their `cargo test` invocation.

### Security fixes

- Hardened CI distribution surface against supply-chain RCE in GitHub Actions (`action.yml`,
  `release.yml`): pinned third-party actions to full commit SHAs, added `permissions: {}` floor,
  scoped `contents: write` to the single job that needs it.
- Closed SSRF allowlist gaps in `WebFetchHandler`: added IPv4 CGNAT (100.64.0.0/10), link-local
  (169.254.0.0/16), and documentation ranges to the block list; fixed redirect policy to re-check
  the literal-IP block list on every hop so a permitted host cannot redirect to a loopback address.
- Bash tool environment isolation: cleared `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`,
  `DYLD_LIBRARY_PATH`, and `PYTHONPATH` before exec to prevent library-injection attacks.
- Path jail (`jail_path`) enforced on all file tools; symlink traversal outside the jail now
  returns `ToolError::PolicyViolation`.
- Write and Edit tools now use `NamedTempFile` + atomic rename instead of direct `fs::write` to
  prevent partial-write corruption and toctou races.
- Tape file creation now uses `O_EXCL` (`create_new(true)`) and `mode(0o600)` on Unix so stale
  tapes cannot be silently overwritten and tape contents are not world-readable.
- Redactor upgraded to Aho-Corasick (`LeftmostLongest`) for O(N) multi-pattern secret scrubbing;
  replaces the previous sequential per-secret `replace` loop.
- Replay deserialization now uses per-key `VecDeque` for correct FIFO ordering when the same tool
  is called multiple times in a recorded session.
- WebFetch body now streamed in chunks up to 1 MiB cap; previously `response.bytes().await`
  allocated the full body before truncation, allowing a malicious server to OOM the process.

### Bug fixes

- Pipeline `vars` are now pre-rendered against the `{ env, secrets }` namespace at engine
  entry, so a pattern like `vars.tag: "{{ env.RELEASE_TAG }}"` resolves to the env value
  before any node is dispatched. Previously the literal template source was substituted
  verbatim by the single-pass downstream renderer, breaking shell args of the form
  `["{{ vars.prev }}..{{ vars.curr }}"]`. Cross-var references (`vars.b: "{{ vars.a }}"`)
  remain unsupported in v0.1.x — the render context exposes only `env` and `secrets`.

### Other changes

- `AgentPolicy.max_message_history_bytes` (default 4 MiB) bounds per-agent conversation history;
  oldest messages are evicted when the cap is exceeded, keeping at least the first two.
- `LoopAgentRequest` fields `initial_prompt`, `system`, `provider`, `model` changed to `Arc<str>`
  to eliminate per-iteration `String` clones on the hot path.
- Template engine LRU cache capacity fixed: now correctly removes evicted templates from the
  MiniJinja `Environment` (was using `put` instead of `push`, leaking compiled templates).
- Generic `TapeWriter<T>` / `TapeReader<T>` NDJSON tape abstraction added to `orno_core::util`;
  existing LLM and tool tape layers still use their own I/O (migration deferred).
- `orno-cli`'s `run.rs` split into `run/mod.rs`, `run/agent.rs`, `run/secrets.rs`,
  `run/transport.rs` for maintainability.

- Initial release.
