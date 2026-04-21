# A Rust architecture for a CI-native multi-agent orchestrator

A 6–8 week solo MVP can credibly ship as a thin executor today and evolve into a durable, record/replay, budget-first engine later — **if, and only if, you hand-roll the OpenAI-compatible client, keep a single Cargo workspace with a `core`/`cli` split, and encode pipeline execution as a typed event log from day one**. Everything else in this report flows from those three commitments. The hazards are specific and well-documented: `serde_yaml` is archived, its popular fork `serde_yml` is under RUSTSEC advisory for unsoundness, "OpenAI-compatible" is a marketing truce rather than a specification, and the Rust ecosystem does not yet have a good LLM-SSE mocking story. The recommendations below are opinionated defaults you'd need a concrete reason to override.

---

## 1. Library recommendations

### CLI, config, and boring foundations

**Clap v4 derive plus `clap_complete`** is the only defensible pick for a DevOps-facing tool with 5–10 subcommands. The ~5-second cold-build cost is a rounding error against the help-quality and shell-completion gap to `bpaf`, `argh`, or `pico-args`. Wrap CLI parsing in your `core` crate rather than `main.rs` so downstream uses (library embedding, completion generation, schema export) don't require the binary. Use the derive API for declared subcommands and drop into the builder only for any future plugin-contributed subcommands. Ship `clap_complete` output for bash/zsh/fish/powershell; treat `clap_complete`'s dynamic-completion feature as **unstable-pinned** and regenerate on upgrade.

**YAML parsing is the single sharpest landmine in this stack.** `serde_yaml` was archived by dtolnay in March 2024 (released as `0.9.34+deprecated`). The obvious-looking replacement **`serde_yml` carries RUSTSEC-2025-0068 ("unsound and unmaintained")** — the initial fork contained AI-generated slop that dtolnay publicly criticized, and trust is not recoverable. **Pick `serde_yaml_ng`** (Antoine Catton's independent continuation) as the conservative, drop-in-compatible replacement; `serde_norway` is a viable alternate with the "Norway problem" (unquoted `no` parsing as `false`) explicitly in its name. Plan to migrate to **`serde-saphyr`** when you outgrow 1.1 semantics or want merge-keys — it's the pure-Rust YAML 1.2 path the ecosystem is converging on. Strongly-type every field (no untagged enums over bool-like strings) and quote in your own sample configs to dodge the Norway problem.

**Schema generation and validation are two jobs, two crates.** `schemars` 1.0 (released 2025) derives JSON Schema from Rust types — ship the schema via a `yourtool schema` subcommand and reference it from a `# yaml-language-server: $schema=…` comment so IDEs validate user pipelines. `jsonschema` (Stranger6667) validates YAML-as-JSON at runtime with structured error output. Add `garde` only if you need cross-field validation beyond what JSON Schema expresses; skip `validator` (older, heavier, proc-macro-forward).

**Templating is decided: `minijinja`.** Armin Ronacher wrote Jinja2 and minijinja both; Jinja2 semantic fidelity is unmatched in the Rust ecosystem. For prompt rendering, **explicitly set `env.set_auto_escape_callback(|_| AutoEscape::None)`** rather than trusting default extension-heuristics — this is the single line that separates "prompts render correctly" from "your HTML-encoded JSON breaks the tool-call path next month." Enable `python_methods` if your prompt authors come from Jinja-on-Python; enable `loader` for filesystem templates; use `minijinja-embed` to bake default prompts into the binary.

**Tracing via `tracing` + `tracing-subscriber`.** Wire JSON output from day one (for DevOps log pipelines); gate OpenTelemetry behind a `--otel-endpoint` flag — **do not force users to run a collector**. The OTel Rust project is actively churning: traces are stable, logs use the appender-tracing bridge, minor versions still break APIs, and `tracing-opentelemetry` is desynced from the `opentelemetry` crate's version numbering. Pin both or expect confusing compile errors. The #1 operational footgun is that **`tokio::spawn`'d tasks do not inherit the current tracing span** — codify `task.instrument(Span::current())` as house style or your multi-agent spans will disappear. Flush the batch exporter on exit via a RAII guard; short-lived CLI processes drop spans otherwise.

**Errors: `thiserror` 2.x in `core`, `anyhow` in `cli`.** thiserror 2.0 is worth the upgrade (no-std support, cleaner attributes). Use `#[from]` sparingly — auto-conversion inflates error enums. Prefer explicit variants with `#[source]`. Add `miette` only if you plan to render user-facing diagnostics with source spans for pipeline YAML ("this field is invalid, line 42") — it integrates cleanly with thiserror via a compatible derive.

**Config: `figment` with explicit layer composition.** Precedence is `Cli > Env > File > Defaults`. The known-good recipe composes `Serialized::defaults(Config::default())`, `Yaml::file(path)`, `Env::prefixed("YOURTOOL_")`, then `Serialized::defaults(Cli::parse())`. This only works if **every CLI field is `Option<T>`** with `#[serde(skip_serializing_if = "Option::is_none")]`; otherwise clap stuffs `None` and nukes your file values. Skip `default_value_t` on clap fields and move defaults entirely into the figment `Serialized::defaults` layer. `config-rs` is the alternate if you want dotted-env conventions (`APP__NESTED__KEY=value`) out of the box.

**Testing: `cargo-nextest` + `insta` + `rstest` + `wiremock-rs` with a custom LLM replay transport.** Nextest for process-per-test isolation and parallelism across binaries (doctests still go through `cargo test --doc`). `insta` remains best-in-class for snapshot testing of CLI output; pair with `assert_cmd`. `proptest` for property tests (`quickcheck` is semi-abandoned). **The Rust ecosystem has no first-class LLM-SSE mock** — `wiremock-rs`, `httpmock`, and `mockito` all require you to hand-wire `ResponseTemplate::set_body_raw` with `Content-Type: text/event-stream` and an async body stream. The correct architectural response is to **define a `trait LlmTransport` in `core` and supply a filesystem-backed replay implementation** for tests, using wiremock only to verify request-shape correctness (auth headers, JSON bodies). This is also exactly the seam that durability/record-replay slots into later.

### LLM and HTTP stack

**HTTP: `reqwest` 0.13 with `default-features = false`** plus `json`, `stream`, `http2`, `rustls-tls-webpki-roots`. Explicitly disable `cookies`, `brotli`, `zstd`, `deflate`, `native-tls*`, `charset`, `system-proxy`, `multipart`, `hickory-dns`. **Pin the rustls crypto provider to `ring`**, not aws-lc-rs: aws-lc-rs is rustls 0.23's new default but requires cmake + NASM on Windows and complicates cross-compile. Ring is pure-Rust+assembly and sufficient for HTTPS to LLM APIs in 2026. Webpki-roots (bundled CA bundle) beats native-roots for scratch/distroless container users; use native-roots behind a feature flag for corporate MITM environments.

The **long-lived SSE footguns** are concrete. `.timeout()` applies to the entire response body read, so it will kill long streams — use `.connect_timeout()` plus a per-chunk watchdog via `tokio::time::timeout(stream.next())`. Set `.tcp_keepalive(Duration::from_secs(30))` explicitly (reqwest doesn't by default, and NAT/LB idle drops will silently hang). Set `.pool_idle_timeout(Duration::from_secs(50))` to stay under the typical 60-second proxy idle timeout. Force HTTP/1.1 for local endpoints (vLLM, Ollama, llama.cpp, LM Studio) via `.http1_only()` on a per-provider client — H2 connection coalescing causes stream-starvation surprises that aren't worth debugging.

**Rust LLM client libraries: hand-roll the chat-completions surface.** This is the single most important technical decision in this report. The three alternatives are all defective for this use case:

- **`async-openai`** has first-class `OpenAIConfig::with_api_base(...)` and an `OPENAI_BASE_URL` env var, and is the most polished OpenAI surface in Rust. But its types are strict OpenAI types; it **errors on unknown enum variants** that vLLM/Anthropic-compat/DeepSeek emit (`reasoning_content`, `native_finish_reason`, vendor-specific `finish_reason` strings). Its `bring-your-own-type` escape hatch exists but means re-wrapping the same calls with `create_byot(Value)` — at which point you've hand-rolled badly.
- **`genai`** is well-designed and its `ServiceTargetResolver` is the cleanest multi-provider abstraction in the Rust ecosystem. It normalizes `reasoning_content` across DeepSeek/Groq/Ollama. But it is explicitly a multi-provider abstraction — adopting it contradicts your "OpenAI-compat-only" constraint and ties you to its 0.5→0.6-alpha release cadence.
- **`rig`** is an agent framework with the wrong level of opinion (Agent/Preamble/Tool struct types). `llm-chain` is functionally unmaintained for 2026. `openai-api-rs` offers no advantages over async-openai.

**Hand-roll in ~400 LOC.** A permissive `serde` DTO with `#[serde(default)]` on every field and `#[serde(flatten)] extra: HashMap<String, Value>` on response types preserves vendor fields without deserialization failure. Use `serde_json::value::RawValue` for tool-call `arguments` so you can accept both string and object shapes. This is the pattern aider, continue, and codex-cli effectively use: wrap HTTP directly, define your own permissive types, live in the ambient chaos. When production LLM tools wrap HTTP directly, the reason is always the same: the SDK's type strictness is the abstraction leak.

**SSE: `eventsource-stream` on `reqwest::Response::bytes_stream()`.** Do **not** use `reqwest-eventsource` — its auto-reconnect semantics are actively wrong for LLM streams (would replay a partially-completed generation and double-charge). Filter `[DONE]` before JSON parsing. Tolerate missing `data:` prefix (some proxies drop it), keepalive comments (`: ping`), and chunks split across TCP frames (the crate handles buffering correctly; a naive `.split("\n\n")` does not). For Ollama native `/api/chat` NDJSON (one JSON per line, terminal `{"done": true}`, no `data:` prefix), wire a separate branch — but prefer hitting Ollama's `/v1/chat/completions` compat endpoint to unify the codepath.

**Token counting: server-authoritative plus heuristic preflight.** **`tiktoken-rs`** covers OpenAI tokenizers (cl100k_base, o200k_base, o200k_harmony for gpt-oss) and is actively maintained, but is wrong for Claude, Llama, Qwen, DeepSeek, which use completely different tokenizers. The real-world practice is what aider's docs state explicitly: **"Aider never enforces token limits; it only reports token limit errors from the API provider. The token counts that aider reports are estimates."** Continue, cursor, claude-code all behave the same way. Use `chars/3` with 20% safety margin for preflight gating; rely on the server's `usage` field (enable `stream_options.include_usage: true`) for authoritative accounting; ship a static context-window map (`model → max_context`) with user override. Feature-gate `tiktoken-rs` as a nicety for the OpenAI happy path.

### Systems and release

**SQLite: `rusqlite` with the `bundled` feature, called from `tokio::task::spawn_blocking`.** This is the canonical single-binary+embedded-DB recipe and adds ~600–900 KB for SQLite's amalgamation. `sqlx` sounds attractive for compile-time-checked queries and async-native behavior but it transitively links `libsqlite3-sys` — so you **cannot coexist with rusqlite** in the same tree without pinning lockstep — and its compile-time query checks require a real DB file at build time, which is friction in a solo-builder CI. More fundamentally, SQLite is single-writer at the kernel level; async buys you nothing over a single writer task wrapped in `spawn_blocking`. **`sled` is effectively dead** (last stable 0.34.7 in 2021, the planned "marble" rewrite has not landed). `turso/Limbo` is still alpha in 2026. `redb` 3.0 is a legitimate KV alternative but abandons SQL. `sea-orm` is pure ORM tax with no payoff for an append-log. Use `rusqlite_migration` or `refinery` for schema evolution.

**Subprocess: `tokio::process::Command` directly, not `duct`.** `duct` kills grandchildren by default (useful) and handles argv cleanly, but it's sync-first; wrapping its blocking I/O in `spawn_blocking` to stream output is worse than writing the ~50 lines of tokio::process plumbing yourself. Two reader tasks push stdout/stderr into your event channel; `tokio::select!` between `child.wait()` and `CancellationToken::cancelled()` gives you clean graceful shutdown. On Unix, use `setsid` via `unsafe { pre_exec(...) }` plus `killpg(-pid, SIGTERM)` for process-tree kill. On Windows, job objects — or accept the grandchild leak for MVP. **Default to `.env_clear().envs(explicit_map)`** rather than env inheritance; CI tools that inherit the runner's env leak secrets or misconfigure tools downstream. Never `sh -c <formatted>` with user input — argv-form always.

**Binary size: `lto = "fat"`, `codegen-units = 1`, `strip = "symbols"`, `panic = "abort"`, `opt-level = 3`.** Expected final size for reqwest+rustls+clap+tokio+serde+rusqlite-bundled: 10–14 MB per platform, 4–5 MB download after cargo-dist's tarball compression. `panic = "abort"` loses `catch_unwind` but keeps backtrace symbols accessible via `RUST_BACKTRACE=1` — and plugin panics should never live in your process anyway (run plugins out-of-process). Run `cargo bloat --release --crates` weekly; add it as a `cargo xtask` from week 1. **Feature-flag discipline from day 1 is the single biggest lever** — specifically `reqwest`, `tokio` (avoid `"full"`; enumerate: `rt-multi-thread`, `macros`, `process`, `io-util`, `net`, `fs`, `sync`, `time`, `signal`), and your own crate. Retrofitting feature flags is miserable; adding them upfront is free.

**Cross-platform build matrix:** ship both `x86_64-unknown-linux-musl` (fully-static, scratch-container-friendly) and `x86_64-unknown-linux-gnu` (glibc-only workloads, oldest LTS runner). For aarch64-linux, cross-compile from ubuntu-x64 via `cargo-zigbuild` (what uv and cargo-dist do). macOS: both arches natively on `macos-14` (M-series). Windows: `x86_64-pc-windows-msvc` on `windows-2022`.

**Release: `dist` (the tool formerly named cargo-dist) + `release-plz` + GitHub Artifact Attestations.** This is what uv, ruff, jj, and oxc all do in some combination. cargo-dist/axodotdev has a thinning bus factor since Astral forked internally in late 2024, but remains the de-facto Rust-aware release tool and generates installers for shell/PowerShell/Homebrew/npm/Scoop/MSI. release-plz handles version/changelog/tag via Conventional Commits and has effectively replaced `cargo-smart-release`. **Adopt both in week 1 even for `v0.0.1-dev`** — the setup cost is ~30 min and it pays back immediately. GitHub Artifact Attestations give you SLSA v1.0 Build L2 for free via `actions/attest-build-provenance`; users verify with `gh attestation verify`. Defer Apple notarization and Windows Authenticode signing — ruff, uv, and jj don't sign either, and your SRE audience will accept the `xattr -d` or Homebrew path.

**Plugin protocol: don't build one in MVP. Design the wire format now.** The plugin question decomposes into *when* and *how*. When: not in MVP. How: **subprocess + JSON over stdin/stdout**, modeled on `terraform-plugin`'s go-plugin pattern but simpler (no gRPC unless you later need bidirectional streaming). The case against WASM for this domain is concrete: your plugins want to shell out, make HTTP calls, touch the filesystem, and spawn subprocesses — all of which WASI sandboxing fights you on. Wasmtime startup, WASM toolchain prerequisites for plugin authors, and WASI network/fs limits are real friction; the 5-ms subprocess invocation overhead is invisible next to LLM latency. Dynamic loading (libloading, abi_stable, stabby) has no stable Rust ABI, inflicts version-skew debugging nightmares, and buys you nothing here. Zellij's WASM plugin system is impressive prior art but solves a different problem — in-process UI extensions with heavy render latency sensitivity.

The seam you build in week 1 is a **pure-data JSON request/response contract**: `NodeRequest` as a serde-tagged enum (`#[serde(tag = "kind")]`, `#[non_exhaustive]`, `schema_version: u32`), `NodeResponse` symmetrically. Built-in nodes implement `async fn execute(req) -> resp`. The subprocess plugin contract later is literally `stdin.write(serde_json::to_vec(&req)); stdout.read(&mut buf); serde_json::from_slice(&buf)`. Version the wire from v0 and you can't paint yourself in.

---

## 2. OpenAI-compatible endpoint compatibility matrix

The short version: **the request URL and the top-level envelope match. Everything interesting past that is per-vendor.** Treat each provider as a named backend with a declared capability flag set, never as a drop-in. The matrix below is the minimum fact base to design against.

| Feature | OpenAI | Anthropic /v1 compat | OpenRouter | vLLM | llama.cpp | Ollama /v1 | Ollama /api/chat | LM Studio | Groq | Together | Fireworks |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `/v1/chat/completions` | reference | ✓ ("test/eval") | ✓ | ✓ | ✓ | ✓ (exp) | native only | ✓ | ✓ | ✓ | ✓ |
| SSE `data:` + `[DONE]` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | NDJSON, no `[DONE]` | ✓ | ✓ | ✓ | ✓ |
| Tool calling (OpenAI `tools`) | ✓ strict | partial — `strict` ignored | model-dependent | needs `--enable-auto-tool-choice --tool-call-parser` | needs `--jinja` + capable template | streaming+tools broken (#9092) | native shape | model-dep | mostly ✓ | mostly ✓ | mostly ✓ |
| Multiple `system` messages | ✓ | **concatenated** into single top-level system | ✓ | ✓ | ✓ | ✓ | n/a | ✓ | ✓ | ✓ | ✓ |
| `max_tokens` vs `max_completion_tokens` | o1/o3/gpt-5 **require** `max_completion_tokens` | both accepted | both, `max_tokens` deprecated | `max_tokens`, recent builds accept both | `max_tokens` | both | `num_predict` native | `max_tokens` | `max_tokens` | `max_tokens` | `max_tokens` |
| Reasoning content | hidden; only token count | ✗ not returned via compat | `reasoning`/`reasoning_details[]` normalized | `reasoning_content` **only if** `--reasoning-parser` flag | `reasoning_content` with `--jinja` + template | inline `<think>…</think>` or separate field | `thinking` on some | backend-dep | `reasoning` field | model-dep | `reasoning_content` |
| Model naming | `gpt-4o`, `o3`, `gpt-5` | Anthropic names (`claude-sonnet-4-5`) | `vendor/model` slug | HF repo id or `--served-model-name` | GGUF path or `--alias` | tag; `ollama cp` to alias | same | LM Studio id | vendor slug | vendor slug | `accounts/fireworks/…` |
| `response_format: {type: "json_object"}` | ✓ | **silently ignored** | ✓ | ✓ (guided_json) | ✓ (GBNF) | ✓ | `format: "json"` | ✓ | ✓ | ✓ | ✓ |
| `response_format: json_schema strict` | ✓ | **ignored** — schema not guaranteed | model-dep | ✓ (xgrammar) | ✓ | partial | partial | partial | regression on gpt-oss-120b | model-dep | ✓ (schema in prompt too) |
| `/v1/embeddings` | ✓ | ✗ | ✓ | ✓ | ✓ (diff shape) | ✓ | native | ✓ | ✗ | ✓ | ✓ |
| `stream_options.include_usage` | ✓ | ✓ | ✓ | ✓ | recent | historically ignored (#4448), now lands | n/a native always sends | ✓ | ✓ | ✓ | ✓ |
| Temperature range | 0–2; reasoning models forced to 1 | **clamped ≤ 1** | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| `seed`, `logit_bias`, `logprobs`, `n>1`, freq/pres penalty | ✓ | **silently ignored**; `n` must be 1 | most ✓ | most ✓ | most ✓ | subset | — | most ✓ | rejected 400 | partial | partial |

### Lowest-common-denominator strategy

The minimal subset that works across all providers without feature detection is: `model`, `messages` (single leading `system` then alternating `user`/`assistant`, **string content only, not content-parts arrays**), `stream: true`, `max_tokens` (not `max_completion_tokens`), `temperature` in `[0,1]`, `top_p`. That's the request. Response parsing reliably yields `choices[0].message.content` or `choices[0].delta.content`, `choices[0].finish_reason`, and `usage.{prompt,completion,total}_tokens`. Treat end-of-stream OR `finish_reason != null` as terminal rather than relying on `[DONE]`.

What you **give up** under the LCD is: Structured Outputs (use prompt-engineered JSON + retry-on-parse), tool/function calling (gate per-provider plus per-model allowlist), reasoning content extraction, `max_completion_tokens` (required only for OpenAI reasoning models — handle via capability flag), multi-modal content parts, `n>1`/`logprobs`/`logit_bias`/`seed`/penalties, and reliable streaming-usage. Each of these becomes an **opt-in capability flag per provider** set by config, never sniffed at runtime.

### Client design implication

The `ProviderConfig` declares the feature set. The client's `RequestBuilder` reads the config and omits fields the provider can't handle. The SSE parser accepts dialect (OpenAiSse | OllamaNdjson) from config. The response DTO uses `#[serde(flatten)] extra: HashMap<String, Value>` to preserve vendor fields without deserialize failure. The reasoning-content field name is declared per provider (`reasoning_content` | `reasoning` | strip `<think>…</think>` from content | None). **Don't detect, declare** — the same principle litellm built its business on.

### Record/replay canonical form

Record request body + response as **raw SSE events, preserved as chunks** (not concatenated). Concatenation loses `finish_reason` timing, usage-chunk position, keepalives, and dialect. On-disk format: NDJSON where line 1 is metadata (`provider`, `base_url`, capability snapshot, request headers sans auth) and subsequent lines are raw events. This format is git-diffable, greppable, and bit-for-bit deterministic on replay. Skip or fuzzy-compare usage counts on regression (tokenizer drift across model updates).

---

## 3. Architecture diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│                         yourtool (binary)                           │
│                    crates/yourtool-cli/src/main.rs                  │
└─────────────────────────────────────────────────────────────────────┘
         │  clap derive parses argv → Command enum → dispatches to core
         ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       yourtool-core (library)                       │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────────┐   │
│  │ config::load │→ │ schema::YAML │→ │ template::MiniJinja env  │   │
│  │ figment      │  │ → schemars   │  │  (auto_escape=None)      │   │
│  │ CLI>Env>File │  │ → jsonschema │  │  context = pipeline vars │   │
│  └──────────────┘  └──────────────┘  └──────────────────────────┘   │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────┐    │
│  │                  Execution Engine                           │    │
│  │   ┌──────────────┐     ┌────────────────────────────┐       │    │
│  │   │  DAG planner │ ──▶ │ Scheduler (JoinSet)        │       │    │
│  │   └──────────────┘     │ per-node CancellationToken │       │    │
│  │                        │ timeout + graceful shutdown│       │    │
│  │                        └────────────┬───────────────┘       │    │
│  │                                     ▼                       │    │
│  │   ┌───────────────────────────────────────────────────┐     │    │
│  │   │ NodeRegistry  (trait NodeExecutor + kind → impl)  │     │    │
│  │   │ built-in: llm | shell | http | parse | assert     │     │    │
│  │   │ external: NodeKind::External { cmd, args } (stub) │     │    │
│  │   └───────────────────────────┬───────────────────────┘     │    │
│  │                               │                             │    │
│  │         ┌─────────────────────┼────────────────────┐        │    │
│  │         ▼                     ▼                    ▼        │    │
│  │   ┌──────────────┐     ┌──────────────┐     ┌────────────┐  │    │
│  │   │ LlmExecutor  │     │ ShellExecutor│     │  others    │  │    │
│  │   │ uses         │     │ tokio::process│    │  ...       │  │    │
│  │   │ trait        │     │ streaming     │    │            │  │    │
│  │   │ LlmTransport │     │ stdout/stderr │    │            │  │    │
│  │   └──────┬───────┘     └──────┬────────┘    └────────────┘  │    │
│  │          │                    │                             │    │
│  └──────────┼────────────────────┼─────────────────────────────┘    │
│             │                    │                                  │
│             │                    │  Events flow out ───────────┐    │
│             ▼                    ▼                             ▼    │
│  ┌──────────────────────────┐    ┌──────────────────────────────┐   │
│  │   LlmTransport (trait)   │    │    EventLog actor            │   │
│  │   default: reqwest+SSE   │    │    mpsc in → broadcast out   │   │
│  │   seam: RecordReplay impl│    │    seq-indexed envelope      │   │
│  └─────────────┬────────────┘    └──────────────┬───────────────┘   │
│                │                                │                   │
│                ▼                                ▼                   │
│  ┌──────────────────────────┐    ┌──────────────────────────────┐   │
│  │ BudgetEnforcer (preflight│    │ Subscribers (broadcast):     │   │
│  │ chars/3 + provider usage │    │  • stderr log formatter      │   │
│  │ per-run + per-pipeline   │    │  • JSON log sink (file)      │   │
│  │ caps; returns Err if over│    │  • EventSink (trait, opt-in) │   │
│  │ → events: BudgetExceeded)│    │    ─ InMemorySink (default)  │   │
│  └──────────────────────────┘    │    ─ SqliteSink (feature)    │   │
│                                  │    ─ future: OTLP exporter   │   │
│                                  └──────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘

  SEAMS preserving the Option B path:
  ① LlmTransport trait = record/replay layer plugs in without touching callers
  ② EventSink trait = SQLite durability plugs in without touching the scheduler
  ③ NodeKind::External = subprocess plugin transport plugs in later
  ④ EventEnvelope { schema_version, seq, event }, #[non_exhaustive] on Event
```

The four seams above are the only architectural commitments that matter. Everything else is refactorable.

---

## 4. Proposed directory structure

**Single Cargo workspace with two crates.** The jj-vcs/jj project is the closest architectural analog in the Rust ecosystem: a Git-compatible VCS at ~50–100k LOC that ships exactly two workspace crates (`jj-lib/` and `jj-cli/`) and holds the line there. uv's 60-crate workspace is a sign of scale (Astral has a large team and hundreds of thousands of LOC); copying it for a solo 20-30k-LOC project is cargo-cult architecture. Tokio itself famously *merged* from many small crates back into one in 2020 because cross-crate version coordination was more painful than monolithic builds. The 61% of Rust developers who hit the single-crate wall at ~8k LOC are feeling actual pain — not imaginary — but the answer is 2 crates, not 15.

The split earns its keep by (1) keeping the binary's tokio/clap build out of library consumers' dependency graphs, (2) enabling future library embedding (another tool imports `yourtool-core`), and (3) drawing a clean test boundary: CLI tests in `yourtool-cli/tests/`, library tests colocated with source.

```
yourtool/
├── Cargo.toml                      # workspace manifest, [workspace.dependencies]
├── Cargo.lock
├── rust-toolchain.toml             # pin rust version (e.g. 1.89)
├── rustfmt.toml
├── clippy.toml
├── dist-workspace.toml             # cargo-dist config
├── release-plz.toml
├── .github/
│   └── workflows/
│       ├── ci.yml                  # fmt, clippy, nextest, cargo bloat
│       ├── release.yml             # generated by `dist init` — do not hand-edit
│       └── schema.yml              # regenerate+publish JSON schema on tag
├── crates/
│   ├── yourtool-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs              # pub re-exports, crate-level docs
│   │       ├── config/
│   │       │   ├── mod.rs          # ProviderConfig, PipelineConfig types
│   │       │   └── layered.rs      # figment composition
│   │       ├── pipeline/
│   │       │   ├── mod.rs          # Pipeline, Node, Dep types
│   │       │   ├── schema.rs       # serde types, schemars derive
│   │       │   ├── load.rs         # YAML → validated Pipeline
│   │       │   └── template.rs     # MiniJinja env, context building
│   │       ├── execution/
│   │       │   ├── mod.rs          # Engine, Runner
│   │       │   ├── scheduler.rs    # JoinSet + CancellationToken
│   │       │   ├── dag.rs          # topo sort, ready-set
│   │       │   └── ctx.rs          # RunCtx { cancel, event_tx, ... }
│   │       ├── node/
│   │       │   ├── mod.rs          # trait NodeExecutor, NodeRequest/Response
│   │       │   ├── registry.rs     # HashMap<Kind, Arc<dyn NodeExecutor>>
│   │       │   ├── llm.rs          # LlmExecutor
│   │       │   ├── shell.rs        # ShellExecutor (tokio::process)
│   │       │   ├── http.rs         # HttpExecutor
│   │       │   ├── parse.rs        # ParseExecutor (jq/regex/json path)
│   │       │   └── assert.rs       # AssertExecutor (eval gate primitive)
│   │       ├── llm/
│   │       │   ├── mod.rs          # trait LlmTransport (record/replay seam)
│   │       │   ├── transport.rs    # reqwest + eventsource-stream impl
│   │       │   ├── dto.rs          # permissive serde DTOs (flatten extra)
│   │       │   ├── sse.rs          # SSE dialect parser (OpenAI | NDJSON)
│   │       │   ├── provider.rs     # ProviderConfig, capability flags
│   │       │   └── tokens.rs       # tiktoken feature + heuristic
│   │       ├── budget/
│   │       │   ├── mod.rs          # BudgetEnforcer trait + impl
│   │       │   └── policy.rs       # per-run, per-pipeline caps
│   │       ├── events/
│   │       │   ├── mod.rs          # Event enum, EventEnvelope
│   │       │   ├── log.rs          # EventLog actor (mpsc→broadcast)
│   │       │   └── sink.rs         # trait EventSink + InMemorySink
│   │       ├── state/              # optional, feature-gated "sqlite"
│   │       │   ├── mod.rs          # SqliteSink impl of EventSink
│   │       │   ├── migrations.rs
│   │       │   └── migrations/     # .sql files, rusqlite_migration
│   │       ├── error.rs            # thiserror enums
│   │       └── telemetry.rs        # tracing subscribers, OTel gate
│   │   └── tests/                  # integration tests (pipeline runs)
│   └── yourtool-cli/
│       ├── Cargo.toml              # [[bin]] name = "yourtool"
│       ├── src/
│       │   ├── main.rs             # ~30 lines: parse, dispatch, exit
│       │   ├── cli.rs              # clap derive Args
│       │   ├── commands/
│       │   │   ├── run.rs          # `yourtool run pipeline.yaml`
│       │   │   ├── validate.rs     # `yourtool validate pipeline.yaml`
│       │   │   ├── schema.rs       # `yourtool schema` → JSON Schema
│       │   │   ├── completions.rs  # `yourtool completions bash`
│       │   │   └── replay.rs       # `yourtool replay <run-id>` (Option B preview)
│       │   └── output.rs           # formatters: pretty / json / junit
│       └── tests/
│           ├── cli.rs              # assert_cmd + insta snapshots
│           └── fixtures/
│               ├── pipelines/
│               └── replays/
├── examples/
│   ├── hello-world.yaml            # one llm node, OpenAI
│   ├── ci-test-gate.yaml           # shell + llm + assert
│   └── multi-provider.yaml         # switch providers via config
├── schemas/
│   └── pipeline.schema.json        # generated from schemars, committed
├── docs/
│   ├── architecture.md             # this report's diagram
│   ├── providers.md                # capability matrix, per-provider config
│   ├── events.md                   # event schema + versioning policy
│   └── adr/                        # Architecture Decision Records
│       ├── 0001-openai-compat-only.md
│       ├── 0002-hand-rolled-llm-client.md
│       ├── 0003-event-log-from-day-one.md
│       └── 0004-defer-plugin-protocol.md
├── xtask/                          # optional; cargo-xtask pattern
│   └── src/main.rs                 # `cargo xtask bloat`, `cargo xtask schema`
└── README.md
```

**Where YAML schemas live and how they ship.** `schemas/pipeline.schema.json` is generated by `cargo xtask schema` (which calls `schemars::schema_for!(Pipeline)` inside `yourtool-core`), committed, and referenced from user pipelines via a `# yaml-language-server: $schema=https://raw.githubusercontent.com/you/yourtool/v0.1.0/schemas/pipeline.schema.json` comment. The binary also ships `yourtool schema` which prints it to stdout. `minijinja-embed` bakes any default prompt templates into the binary. Don't ship separate schema files next to the binary — one less thing users can lose.

---

## 5. Week-by-week implementation plan

### Weeks 1–2: Skeleton and provider layer

**Built:** the workspace, the core types, the hand-rolled LLM transport, YAML loading and validation, template rendering, and a running `llm`-only pipeline.

Files created: the entire `crates/yourtool-core/` tree under `llm/`, `events/`, `node/llm.rs`, `pipeline/`, `config/`, `error.rs`, `telemetry.rs`; in `yourtool-cli/`, `main.rs`, `cli.rs`, `commands/run.rs`, `commands/validate.rs`. `.github/workflows/ci.yml` runs fmt, clippy, nextest. `dist init` ships with a stub release workflow. `Cargo.toml` feature flags set from day one: `default-features = false` on reqwest, explicit tokio feature list, feature-gate `sqlite` and `tiktoken` off by default.

Decisions made and committed as ADRs: OpenAI-compat-only client strategy (ADR 0001); hand-rolled DTOs with `flatten extra` (ADR 0002); event log exists from day 1 but sinks only to memory (ADR 0003); plugins deferred (ADR 0004). Provider capability flag schema set and frozen. Event variants enumerated with `#[non_exhaustive]` and `schema_version: 1`.

**"Done" definition:** `yourtool run pipeline.yaml` executes a single-node pipeline that calls OpenAI, streams tokens to stderr, and emits a JSON event log to stdout. Same pipeline works against vLLM on `localhost:8000/v1` and Anthropic's OpenAI-compat endpoint by config change alone. Integration tests use a filesystem-backed `LlmTransport` replay; no live calls in CI.

### Weeks 3–4: Multi-node DAG, shell, budgets, and record/replay harness

**Built:** DAG scheduling with `JoinSet`, shell/http/parse/assert nodes, `BudgetEnforcer` preflight and runtime enforcement, the record/replay harness (write-time), and the CLI's pretty/json output formatters.

Files created: `execution/scheduler.rs`, `execution/dag.rs`, `node/shell.rs` (with stdout/stderr streaming + cancellation + `.env_clear()` default), `node/http.rs`, `node/parse.rs`, `node/assert.rs`, `budget/{mod,policy}.rs`, `llm/transport.rs` gains a `RecordingTransport<T: LlmTransport>` decorator that writes NDJSON replay files. `yourtool-cli/commands/replay.rs` hits this path. `insta` snapshots in `yourtool-cli/tests/`.

Decisions: assert-node evaluator language (pick: jmespath-style JSON path + simple comparison ops; skip Lua/Rhai — not the MVP scope). Budget semantics (preflight blocks by heuristic; runtime tallies server `usage`; both generate events). Run-ID format and directory layout (pick: `~/.yourtool/runs/<YYYY-MM-DD>/<ulid>/`).

**"Done" definition:** a 5-node pipeline (shell → llm → parse → assert, with one parallel branch) runs end-to-end, produces a complete event log, enforces a budget cap (pipeline errors with a typed `BudgetExceeded` event), and the recorded run replays deterministically via `yourtool run --replay <path>`. CI matrix builds linux-x64/arm64-musl, macos both arches, windows-x64. Binary sizes logged per run via `cargo bloat` in xtask; size under 15 MB stripped.

### Weeks 5–6: Polish, CI integration shims, documentation

**Built:** GitHub Actions + GitLab CI + Jenkins integration examples, JUnit XML output, structured error output suitable for parsing by CI, `yourtool validate` with miette-quality diagnostics, shell completions, schema export, and the end-to-end docs.

Files created: `examples/` expanded with realistic CI pipelines (test-gate, PR-comment-generator, flaky-test-analyzer); `docs/architecture.md` and `docs/providers.md`; `yourtool-cli/output.rs` gains JUnit XML and GitHub Actions workflow-command formatters (`::notice::`, `::error::` with file/line); completions generation in `commands/completions.rs`.

Decisions: pinned minimum supported Rust version. Public API surface of `yourtool-core` semver-frozen (even if 0.x). Telemetry: tracing JSON logs to stderr by default; `--otel-endpoint` ships disabled unless flagged.

**"Done" definition:** a user who has never seen the tool can paste a 5-line snippet into a GitHub Actions workflow, point at their own OpenAI key, and get an LLM-driven pipeline step running in under 10 minutes. Binary attestations verify via `gh attestation verify`. Documentation site deploys from `docs/`. Release-plz opens a v0.1.0 PR.

### Weeks 7–8 (optional): Launch

**Built:** a launch post with a concrete before/after DevOps use case (e.g., "LLM-generated test flake triage comment on every PR in 30 lines of YAML"), a Homebrew tap via cargo-dist, a demo recording, and a cold-email campaign to 5 DevOps/SRE practitioners for feedback.

**"Done" definition:** v0.1.0 released via `dist`, signed via attestations, announced. **Do not start on the plugin protocol or SQLite durability until feedback is in hand.** These are post-v0.1 and require the user feedback to scope correctly.

### Architectural seams preserved without implementation

The path to Option B is gated entirely by four trait boundaries that exist from week 1 but have only memory-backed implementations at v0.1.0:

First, `trait LlmTransport` in `core::llm` — the `RecordingTransport<T>` decorator pattern added in week 3-4 is already the record half of record/replay; the replay half is a file-reader impl. No scheduler or executor code changes when durable replay lands.

Second, `trait EventSink` in `core::events::sink` with an `InMemorySink` default. SQLite durability is a new `SqliteSink` behind the `sqlite` feature flag plus rusqlite + spawn_blocking + a bounded mpsc. The scheduler and node executors never mention SQLite.

Third, `NodeKind::External { command, args }` as a stub variant on the `NodeKind` enum. Enabling subprocess plugins is implementing one `NodeExecutor` for this variant; the wire format (serde-tagged `NodeRequest`/`NodeResponse` with `schema_version`) is already set.

Fourth, eval gates are just the `assert` node with richer evaluators — the `AssertExecutor` trait object is the seam; Python-style assertion-DSL or judge-model evaluators slot in as new implementations. Budget caps are already primitives via `BudgetEnforcer`; promoting them to user-configurable per-step policies is config work, not architectural.

---

## 6. Anti-patterns and pitfalls

The catalogue below is specific to solo Rust builders. Each item describes the smell, the simpler alternative, and the narrow circumstance where it's actually justified.

**Premature workspace splits** are the most common momentum-killer. 15-crate workspaces on 5k-LOC codebases replicate Astral's uv structure without uv's scale or team. The Tokio project famously merged from many small crates back to one in 2020 because cross-crate version coordination was worse than monolithic builds. jj-vcs holds 50k-100k LOC in exactly two crates. Default to one crate for MVPs under 5k LOC; split to two (`-core` library, `-cli` binary) when you add `lib.rs` that might be consumed externally; split further only when compile times actually hurt and a subtree has a cleanly independent feature gate. Justified when: genuine reusable library (`-core` embeddable elsewhere), hot-path subtree needs its own feature flags, build parallelism demonstrably bottlenecks.

**Proc-macro-heavy DSLs** kill debuggability more than they save keystrokes. Actix-web's macro routing, Bevy's ECS derive-soup, and countless home-grown DSL frameworks make rustc error messages incomprehensible and lock newcomers out of the codebase. For this project: resist the urge to build an `#[node]` or `#[agent]` derive macro. Keep node types as enum variants with manual impl blocks. A proc-macro is justified when you have 50+ consumers and the ergonomic win is massive — not at weeks 1–2 with zero users.

**`async-trait` when you don't need it.** Rust 2024 edition ships stable AFIT/RPITIT (since 1.75); for monomorphized generic code, native `async fn` in traits works. The crate is still needed for `Box<dyn Trait>` with async methods (not dyn-compatible natively). For a heterogeneous `NodeExecutor` registry stored as `Vec<Arc<dyn NodeExecutor>>`, `async-trait` is the correct call and the Box-per-call cost is invisible next to LLM latency. Use native AFIT for `LlmTransport` if it's only ever used generically; use `async-trait` the moment you need a trait object. `#[trait_variant::make(Trait: Send)]` is the hybrid if you want both. Anti-pattern is reaching for `async-trait` reflexively everywhere regardless of dispatch needs.

**Generic-heavy trait soup** where `Box<dyn Trait>` would be simpler. Pushing 8 generic parameters through the call graph to avoid one allocation per invocation is a classic Rust mistake. For a CI orchestrator where the inner loop is "make an HTTPS call to an LLM and wait 2 seconds for a response," the allocation cost of dyn dispatch is literally unmeasurable. Reserve generics for truly hot paths (event bus internals, tokenizer) and default to `Arc<dyn T>` elsewhere.

**Plugin systems before the first user** is the single biggest trap for this specific class of tool. Designing a plugin protocol is a multi-week project; maintaining one is forever. The architectural answer for weeks 1–8 is to ship the **wire format** (versioned serde-tagged `NodeRequest`/`NodeResponse` with `#[non_exhaustive]`) but zero plugin loader. First user feedback will rewrite your plugin API requirements entirely.

**Bikeshedding error hierarchies.** 10 thiserror enums per module with `#[from]` on every variant is a smell. The ratio to aim for is one error enum per subsystem (config, pipeline, execution, llm, budget, events, state), `#[from]` sparingly, `#[source]` for chaining, `anyhow::Result` in the binary with `.context("while loading {}", path)` at layer boundaries. `miette` only where you're rendering to a user ("at line 42, field is invalid") — not for every internal error. Dwarfing your types with diagnostics annotations your users never see is pure compile-time cost.

**Premature async on CPU-bound code.** Don't make the DAG planner, YAML parser, or schema validator async. They're nanoseconds; the overhead of async dispatch outweighs the work. Keep async at the I/O boundaries (LLM calls, subprocess, file I/O) and synchronous for the plumbing.

**Compile-time budget blown by feature laziness.** `tokio = { features = ["full"] }`, `reqwest = "0.13"` (with defaults), and `serde` without-thinking are the three worst offenders. "full" pulls in every tokio module even though your CI tool doesn't need `tracing-opentelemetry-bridge` or `process-signal-reaper` etc. reqwest defaults include `native-tls`, `cookies`, `brotli`, `system-proxy` — all unused, all compile-time and binary-size cost. Set `default-features = false` in week 1 and enumerate explicitly. The retrofit from feature-lazy to feature-strict is painful; the greenfield cost is near zero.

**Cargo feature unification surprises.** Feature flags on your own crate can be enabled by any transitive dep that depends on you. Mark features `optional` deliberately, use `dep:` prefixes in feature definitions to avoid accidentally exposing implementation details as features, and test the cartesian product of your public features in CI. The reqwest-codex-cli TLS unification issue (where both `rustls` and `native-tls` got enabled) is the canonical cautionary tale.

**The serde_yaml → serde_yml trap.** When you search for a YAML parser in 2026, the obvious answer (`serde_yml`) is under a RUSTSEC advisory and should not be used. Tutorials and LLM-generated code will point you at it. Write this explicitly in your ADR 0001 or whatever, and use `serde_yaml_ng`.

**Actor frameworks for things that aren't actor systems.** A CI orchestrator has 5–10 long-lived tasks (scheduler, event log, UI tailer, SQLite writer, HTTP client pool). `tokio::spawn` loops owning their channels are correct; `actix`, `ractor`, `kameo`, `xtra` add paradigm tax for zero architectural benefit. Actor frameworks earn their keep when you need supervision trees with auto-restart-and-backoff semantics or genuine location-transparent message passing. You need neither.

**OpenTelemetry in week 1.** OTel Rust has been "almost stable" for three years. Version skew between `tracing-opentelemetry` and `opentelemetry` crates produces confusing compile errors; metric/log APIs are still moving. Ship v0.1.0 with `tracing` JSON logs to stderr. Add OTel as a v0.2 feature behind a flag. Your SRE users can pipe JSON logs to their existing stack today.

**Trying to make async-openai fit.** async-openai is a well-built library for OpenAI. Bending it to handle vLLM's `reasoning_content`, Anthropic compat's silently-ignored `strict`, Ollama's NDJSON, OpenRouter's `reasoning_details[]`, and llama.cpp's template-dependent tool-call shapes via BYOT is strictly more code than hand-rolling. The reason aider, continue, and codex-cli all wrap HTTP directly with permissive types isn't parochialism — it's that the type strictness of the SDK **is the abstraction leak** when "OpenAI-compatible" means seven different partially-overlapping things in 2026.

---

## Closing synthesis

The architecture that satisfies the Option-A-evolves-to-Option-B brief in 6–8 weeks solo is not clever. It is boring in the specific ways that successful Rust tools are boring: two-crate workspace like jj, permissive hand-rolled serde DTOs like aider, JoinSet scheduling like every modern tokio codebase, `rusqlite` bundled like every single-binary tool that ships embedded SQLite, `dist` + `release-plz` like uv and ruff, deferred plugins like every solo project that eventually shipped plugins successfully. The durability path is secured by four trait boundaries added in week 1 — `LlmTransport`, `EventSink`, `NodeKind::External` variant, `#[non_exhaustive]` event envelopes — none of which require an implementation at v0.1.0.

The biggest risk is not architectural; it is the accumulating research debt of "OpenAI-compatible" being a moving target across seven vendors. Build the provider capability-flag config as a first-class surface, write the integration test matrix against real endpoints (OpenAI + Anthropic compat + local Ollama + a cloud aggregator) early, and treat every user-reported provider bug as a capability-flag addition, never as a branching `if provider == "..."` in the hot path. That discipline, more than any crate choice, is what converts a portfolio project into a company.