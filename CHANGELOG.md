# Changelog

All notable changes to orno are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once the first tagged release ships.

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
