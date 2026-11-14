# Changelog

All notable changes to orno are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once the first tagged release ships.

## Unreleased

### Security

- Redaction runs before truncation for node stderr/stdout tails and agent
  tool output, so a secret cut by the size cap no longer leaks its tail.
- The redactor also matches uppercase hex, unpadded base64, base64 of a
  secret embedded at byte offset 1 or 2, percent-encoded and JSON-escaped
  forms, and each long line of a multi-line secret; `redact_json` redacts
  object keys too.
- Provider API keys read from the process env (`OPENAI_API_KEY`, ...) are
  redacted for the providers a pipeline uses, even when not declared under
  `secrets:`.
- `--record-bundle` scrubs every literal MCP `env` value, `headers` value
  and `auth` token/password, not only HTTP bearer tokens.
- `{{ secrets.* }}` / `{{ env.* }}` in `mcp_servers` (url, command, env,
  headers, auth) are now rendered; before, the template text was sent
  literally. A name listed in both `pass_env` and `secrets` fails
  validation, and the `self-review` example moves `GH_TOKEN` to `secrets:`.
- Dotenv parse errors and `-e` errors no longer echo the offending line,
  and `Debug` output of MCP auth, `env` and `headers` config hides values.
- `Read` / `Write` / `Edit` no longer follow a symlinked directory out
  of the root when the target does not exist yet (`link/new.txt` with
  `link -> /elsewhere`), and relative paths resolve against the root
  instead of the process cwd.

### Added

- Optional per-server `call_timeout_secs` on `mcp_servers` entries
  (default 30).

### Changed

- **Breaking (wire):** `schema_version` is now `4`.
  `node_output_truncated.captured_bytes` is renamed `total_bytes`
  (it always held the total the child wrote), and `Event::BudgetExceeded`
  stays removed; see `docs/reference/events.md`.
- **Breaking:** an agent that lists `Bash` together with a non-empty
  `allowed_domains` or `roots` now fails validation, because a shell
  escapes both limits. Add `unconfined_shell: true` to the agent's
  policy to accept that (the `commit-digest` and `flaky-test-triage`
  examples do).
- `policy.roots` entries and shell-node `env:` values are rendered
  through the template engine, so `{{ vars.* }}` / `{{ env.* }}` work
  there.
- `orno run` emits `run_finished { ok: false }` when the DAG walker or
  the `vars:` render fails before the first node, and engine errors no
  longer skip MCP shutdown or tape flushing.

### Fixed

- A tape or bundle flush failure no longer leaks MCP child processes or
  hides the engine error; the bundle records the pipeline text loaded at
  startup instead of re-reading the file.
- `Read`, `Write` and `Edit` reject FIFOs, devices and directories
  instead of hanging, and run their file I/O off the async workers.
- `Write` and `Edit` keep the target's permissions (a 0755 script stays
  0755).
- `Read` decodes invalid UTF-8 lossily for files of every size.
- Shell nodes and `Bash` run in their own process group and SIGKILL it on
  exit, timeout or cancellation, so a background grandchild neither hangs
  the node on the inherited pipes nor outlives a timeout.
- Capped shell and `Bash` output no longer collapses to empty when it
  ends in stray UTF-8 continuation bytes.

### Removed

- `Event::BudgetExceeded` (never emitted; budget breaches surface as
  `NodeFailure::BudgetExceeded` on `node_finished`).
- `AgentError::SubagentDepthExceeded` and `LlmError::NotImplemented`
  (never constructed).
- The uncompiled `orno_core::config` module.

### Fixed

- MCP stdio servers' stderr no longer leaks into orno's stderr; it is
  logged at debug level.
- MCP tool calls with non-object arguments now fail instead of running
  with none, image/audio-only results yield a placeholder instead of an
  empty string, and a repeated `initialize` returns the cached tools.

### Security

- A call to `subagent.<name>` is now checked against `allowed_tools`
  like every other tool; an unlisted subagent used to dispatch.
- `WebFetch` receives the calling agent's `allowed_domains` /
  `blocked_domains` on every call. Redirect hops were checked against
  empty lists, so an open redirect on an allowlisted host could reach
  any domain.
- `WebFetch` resolves names through a guard that drops blocked
  addresses at connect time (closing the DNS-rebinding window between
  the check and the dial) and ignores `HTTP(S)_PROXY`.
- The blocked-address list now covers `0.0.0.0/8`, `192.0.0.0/24`,
  `198.18.0.0/15`, multicast and `240.0.0.0/4` for IPv4, and `::`,
  multicast, documentation, NAT64, 6to4 and Teredo for IPv6, and
  bracketed IPv6 literals are recognized.
- `blocked_domains` and `allowed_domains` matching ignores case and a
  trailing dot, so `evil.com.` no longer slips past `evil.com`.
- The network gate scans tool arguments recursively for http(s) URLs in
  any case, not just top-level strings starting with lowercase `http`.
- Bump `rmcp` to 3.10 (fixes RUSTSEC-2026-0462, unbounded response
  buffering in the streamable HTTP client).

## 0.2.0 - 2026-05-03

### Security

- **SSRF defense in `WebFetchHandler` redirect handling.** The previous
  redirect path used `reqwest`'s built-in follower, which checks the
  initial host against `allowed_domains` / `blocked_domains` /
  private-IP rules but lets the followed hops escape that gate. A
  malicious public host could `302` to `http://169.254.169.254/`
  (cloud metadata) or to an internal hostname and the request would
  proceed. The handler now sets `Policy::none()` and runs the redirect
  loop manually: each hop re-resolves the host through the bounded
  blocking DNS path, re-checks every IP against the private-block list,
  and re-validates the host against the agent's domain policy. DNS is
  capped at 5 s via `tokio::time::timeout` over `spawn_blocking` and
  fails closed.
- **Bash `cwd` now requires a configured jail.** Previously, agents
  with an empty `roots` list could pass an arbitrary `cwd:` to the
  Bash tool and execute commands rooted anywhere on the filesystem.
  The handler now denies `cwd` outright when `roots` is empty so a
  pipeline cannot accidentally hand a bash node unrestricted reach.
  Pipelines that want a working directory must declare a `roots`
  list; cwd is then validated against those roots.
- **Bundle integrity: post-trailer content rejected.** `read_bundle`
  used to verify the BLAKE3 trailer and stop, but additional bytes
  appended *after* the trailer were silently consumed without error.
  An attacker who could append to a record bundle (e.g., shared
  storage with append-but-not-overwrite ACLs) could thus inject extra
  recorded turns that replay would treat as legitimate. The new
  `BundleError::ExtraContentAfterTrailer { line_count }` variant
  surfaces any post-trailer bytes and aborts the read.

### Bug fixes

- Bash node no longer denied when `allowed_domains` is configured but
  `allow_network` is `true`. The `MutationsAndNetwork` policy arm
  previously forced `allowed_domains` to imply network-required, even
  for tools (like Bash) whose effects don't traverse the network at
  all.
- LoopAgent: `declared_tools` is now stored as `Arc<Vec<OrnoChatTool>>`
  and shared by reference across iterations rather than deep-cloned
  every turn. Cuts a per-iteration allocation that grew with the tool
  count.
- LoopAgent: `Arc::make_mut` invariant on the message history is now
  covered by a regression test (`RetainingTransport`) that retains an
  `Arc::clone` of the request and asserts the loop's copy-on-write
  path produces a fresh allocation, guarding against a future refactor
  that accidentally aliases storage with an external retainer.
- `wire_name_from_yaml` (formerly `LoopAgent::to_wire_name`) is now a
  public free function in `agent::loop_agent`, removing the duplicate
  `yaml_to_wire_name` helper from the replay command.
- `LlmRequest::with_tools(...)` builder added to centralize the
  `Arc<Vec<OrnoChatTool>>` wrap-and-replace pattern across the seven
  call sites that previously inlined it.
- Tracing parity: `ReadHandler::invoke` and `DenyShellExecutor::execute`
  now carry `#[instrument]` attributes matching the rest of the tool /
  executor seams.
- `install.sh`: corrected sha256 sidecar URL derivation. Previous
  versions appended `.sha256` to the archive URL, producing
  `<bin>-<version>-<target>.tar.gz.sha256`, but
  `taiki-e/upload-rust-binary-action` publishes the sidecar with the
  archive extension *replaced* (`<bin>-<version>-<target>.sha256`),
  so v0.1.1 installs failed with a 404 on the checksum download.
  Action consumers pinned to v0.1.1 had to set `ORNO_SKIP_SHA256=1`
  as a workaround; that workaround is no longer required at v0.2.0.
- Regenerated `schemas/pipeline.schema.json` to include `roots`,
  `max_message_history_bytes`, and `max_tool_output_bytes` on
  `AgentPolicy`. These three fields landed in 0.1.1 but the regenerated
  schema was not committed, so IDE yaml-language-server users saw
  stale autocomplete for one release.

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
  the pinned `DoctorMozg/orno@v0.2.0` release tag, never built from the
  PR commit. Forks are excluded by an explicit `if:` guard. The verdict
  step parses `VERDICT: PASS` / `VERDICT: FAIL` from the produced
  `.orno-self-review.md` and gates the PR check accordingly.
- `dogfood-self-review` workflow gained a bootstrap-detection step:
  if `examples/self-review/pipeline.yaml` is not yet on master, the
  job emits a `::notice::` and short-circuits to a green check
  instead of failing. Activates automatically once the file lands on
  master.

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
