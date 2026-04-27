//! `orno run <pipeline.yaml>` — load, resolve env/secrets, dispatch, stream.
//!
//! Assembles a `NodeRegistry` over `ShellExecutor` and `AgentExecutor`
//! (wrapping a `GenAiTransport`), threads them plus a
//! `TemplateEngine` into `Engine::run`, and prints every recorded
//! envelope as NDJSON on stdout.
//!
//! Env and secrets resolution lives in this module:
//! process env, `--env-file`, `--secrets-file`, and `-e` flags
//! combine into the two disjoint namespaces the engine sees.
//! Classification follows the pipeline's `secrets:` declaration,
//! never the source file — a secret name in `--env-file` is still
//! routed to `secrets.*`.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;

use orno_core::McpError;
use orno_core::agent::{Agent, LoopAgent, LoopAgentConfig};
use orno_core::events::{Event, EventSink, Redactor, StreamingSink};
use orno_core::execution::{Engine, EngineConfig, RunInputs, new_run_id};
use orno_core::llm::{
    DummyTransport, GenAiTransport, LlmTransport, RecordingTransport, ReplayTransport,
    ScriptedTransport,
};
use orno_core::mcp::{McpClient, McpTool, McpToolCallResult, RmcpClient};
use orno_core::node::NodeRegistry;
use orno_core::node::agent::AgentExecutor;
use orno_core::node::shell::ShellExecutor;
use orno_core::pipeline;
use orno_core::pipeline::Pipeline;
use orno_core::pipeline::schema::McpServerConfig;
use orno_core::pipeline::template::TemplateEngine;
use orno_core::tool::{
    BashHandler, EditHandler, McpToolHandler, McpToolHandlerConfig, ReadHandler,
    RecordingToolHandler, ReplayToolHandler, SetStateHandler, SubagentHandler, ToolHandler,
    WebFetchHandler, WriteHandler,
};
use serde_json::Value;

/// Test-only escape hatch: when set to `dummy`, `orno run` swaps
/// `GenAiTransport` for `DummyTransport` so integration tests can
/// snapshot the event stream without a live API key. When set to
/// `scripted`, tests can drive multi-turn agent + subagent loops
/// by pre-seeding the response queue via `ORNO_TEST_SCRIPTED_TAPE`
/// — required for record/replay coverage of subagent dispatch
/// (Phase 6). The var name is intentionally awkward — end users
/// should never set it.
const TEST_TRANSPORT_ENV: &str = "ORNO_TEST_LLM_TRANSPORT";

/// Maximum time spent draining MCP servers after SIGINT before the
/// SIGINT branch returns and lets `main` translate `Interrupted` into
/// exit code 130. Bounded so a hung MCP child cannot indefinitely
/// delay the operator's cancel; the OS reaps any stragglers when the
/// parent exits.
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Sentinel inserted into `RunFinished.failed_nodes` on SIGINT so a
/// downstream consumer reading the NDJSON stream can tell an
/// interrupted run apart from a node-level failure without needing
/// out-of-band exit-code context.
const INTERRUPTED_FAILED_NODE: &str = "<interrupted>";

/// Mutex-guarded wrapper that lets the orchestrator hold a single MCP
/// client behind `Arc<dyn McpClient>` for `McpToolHandler` dispatch while
/// still calling `initialize()` / `shutdown()` via the mutex. v0.1.0
/// executes tool calls serially, so the mutex never contends at runtime.
struct SharedMcpClient {
    server: String,
    inner: tokio::sync::Mutex<Box<dyn McpClient>>,
}

impl fmt::Debug for SharedMcpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedMcpClient")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl McpClient for SharedMcpClient {
    fn server_name(&self) -> &str {
        &self.server
    }
    async fn initialize(&mut self) -> Result<Vec<McpTool>, McpError> {
        self.inner.lock().await.initialize().await
    }
    async fn call_tool(&self, tool: &str, args: Value) -> Result<McpToolCallResult, McpError> {
        self.inner.lock().await.call_tool(tool, args).await
    }
    async fn shutdown(&mut self) -> Result<(), McpError> {
        self.inner.lock().await.shutdown().await
    }
}

/// Parsed CLI flags consumed by the env/secrets resolver.
#[derive(Debug, Default)]
pub struct RunFlags {
    pub inline_env: Vec<String>,
    pub env_files: Vec<PathBuf>,
    pub secrets_files: Vec<PathBuf>,
    /// Threaded into `EngineConfig.verbose`. CLI controls what
    /// counts as "verbose"; the engine just shapes its output.
    pub verbose: bool,
    /// Threaded into `EngineConfig.max_output_bytes`. The CLI
    /// resolves the default-vs-verbose policy before this struct
    /// is built.
    pub max_output_bytes: usize,
    /// Threaded into `EngineConfig.max_node_output_bytes`. Caps a
    /// shell node's captured stdout/stderr per stream. Default 8 MiB
    /// (resolved by the CLI before this struct is built).
    pub max_node_output_bytes: usize,
    /// When `Some`, wrap the live transport in `RecordingTransport`
    /// and flush to this path at run end. Mutually exclusive with
    /// `replay_tape`.
    pub record_tape: Option<PathBuf>,
    /// When `Some`, use `ReplayTransport` instead of the live transport.
    /// Mutually exclusive with `record_tape`.
    pub replay_tape: Option<PathBuf>,
    /// When `Some`, wrap each tool handler in `RecordingToolHandler`
    /// and flush to this path at run end.
    pub record_tool_tape: Option<PathBuf>,
    /// When `Some`, wrap each tool handler in `ReplayToolHandler`
    /// so invocations return cached results.
    pub replay_tool_tape: Option<PathBuf>,
    /// When `Some`, record the full run (LLM tape + tool tape +
    /// pipeline YAML) into a single bundle file at this path. The
    /// run still uses the live transport and tool handlers — only
    /// the post-run assembly step writes the bundle. Mutually
    /// exclusive with `record_tape` / `replay_tape` /
    /// `record_tool_tape` / `replay_tool_tape` at the CLI layer; the
    /// runner uses temp paths internally for the two component tapes.
    pub record_bundle: Option<PathBuf>,
}

#[expect(
    clippy::too_many_lines,
    reason = "run() is the top-level orchestrator for orno run; splitting it adds indirection without reducing conceptual load"
)]
pub async fn run(path: &Path, mut flags: RunFlags) -> Result<()> {
    let mut pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    // `--record-bundle` records both tapes into temp files that sit
    // next to the eventual bundle path, then assembles them into a
    // single NDJSON bundle after the engine flushes. The temp paths
    // live in the bundle's parent directory so a successful run
    // produces the bundle on the same filesystem (avoids cross-mount
    // rename surprises) and a failed run leaves diagnosable artifacts
    // adjacent to where the user expected the bundle.
    let bundle_paths = flags.record_bundle.as_ref().map(|bundle| {
        let parent = bundle.parent().unwrap_or_else(|| Path::new("."));
        let stem = bundle.file_name().map_or_else(
            || std::ffi::OsString::from("orno-bundle"),
            ToOwned::to_owned,
        );
        let mut llm = parent.to_path_buf();
        let mut tool = parent.to_path_buf();
        let mut llm_name = stem.clone();
        llm_name.push(".llm.tmp");
        let mut tool_name = stem;
        tool_name.push(".tool.tmp");
        llm.push(llm_name);
        tool.push(tool_name);
        (bundle.clone(), llm, tool)
    });
    if let Some((_, llm_tmp, tool_tmp)) = &bundle_paths {
        flags.record_tape = Some(llm_tmp.clone());
        flags.record_tool_tape = Some(tool_tmp.clone());
    }

    let engine_config = EngineConfig {
        verbose: flags.verbose,
        max_output_bytes: flags.max_output_bytes,
        max_node_output_bytes: flags.max_node_output_bytes,
    };

    let inputs = resolve_inputs(&pipeline, &flags)?;

    // Keep a typed handle to `StreamingSink` so the CLI can read its
    // `is_broken` flag after `engine.run` returns. The engine only
    // sees the trait-object form. Both Arcs point at the same value
    // so a write failure latched during `record` is observable here.
    let streaming_sink = Arc::new(StreamingSink::stdout());
    let sink: Arc<dyn EventSink> = streaming_sink.clone();

    // Mint `run_id` immediately after the sink so every lifecycle
    // envelope (MCP init included) carries a valid correlation id, and
    // so the CLI can emit a `RunStarted`/`RunFinished` pair on the
    // MCP-init crash path before `Engine::run` ever runs.
    let run_id = new_run_id();

    let base_transport: Arc<dyn LlmTransport> = match std::env::var(TEST_TRANSPORT_ENV).as_deref() {
        Ok("dummy") => Arc::new(DummyTransport),
        Ok("scripted") => {
            // Misconfiguration of the test-only `scripted` transport
            // surfaces as a structured CLI error (non-zero exit) rather
            // than a process panic — keeps stack traces out of test
            // logs and lets the integration harness assert on a clean
            // anyhow message.
            let tape_path = std::env::var("ORNO_TEST_SCRIPTED_TAPE").map_err(|_| {
                anyhow!("ORNO_TEST_SCRIPTED_TAPE must be set when ORNO_TEST_LLM_TRANSPORT=scripted")
            })?;
            let json = std::fs::read_to_string(&tape_path)
                .with_context(|| format!("reading scripted tape `{tape_path}`"))?;
            let responses: Vec<orno_core::llm::LlmResponse> = serde_json::from_str(&json)
                .with_context(|| {
                    format!("parsing scripted tape `{tape_path}` as Vec<LlmResponse>")
                })?;
            Arc::new(ScriptedTransport::new(responses))
        },
        _ => Arc::new(
            GenAiTransport::from_agents(&pipeline.agents, &inputs.secrets)
                .context("constructing LLM transport from pipeline agents")?,
        ),
    };

    // Build the redactor once from the resolved `secrets.*` map. Share it
    // across recording transports and the agent executor; both hold Arc refs
    // to the same value list, so redaction is consistent everywhere.
    let redactor = Arc::new(Redactor::new(&inputs.secrets));

    // --replay-tape: swap the live transport for a tape reader. A tape
    // miss is a hard error — no fallback to the live API.
    // --record-tape: wrap the live transport to record (req, resp) pairs.
    // We keep an Arc<RecordingTransport> alongside so we can flush after
    // engine.run() — the trait does not expose flush().
    let mut recording_transport: Option<Arc<RecordingTransport>> = None;
    let transport: Arc<dyn LlmTransport> = if let Some(path) = &flags.replay_tape {
        let replay = ReplayTransport::load(path)
            .with_context(|| format!("loading replay tape `{}`", path.display()))?;
        Arc::new(replay)
    } else if let Some(path) = &flags.record_tape {
        let rec = Arc::new(
            RecordingTransport::create(base_transport, path, redactor.clone())
                .with_context(|| format!("creating record tape `{}`", path.display()))?,
        );
        recording_transport = Some(rec.clone());
        rec
    } else {
        base_transport
    };

    let mut registry = NodeRegistry::new();
    registry.register(
        "shell",
        Arc::new(ShellExecutor::with_config(
            sink.clone(),
            engine_config.max_node_output_bytes,
        )),
    );

    // Reuse the engine's `max_output_bytes` for the LLM body excerpt
    // cap so a truncated stderr tail, a truncated HTTP error body, and
    // a truncated prompt/response excerpt all look alike to a log
    // reader. `SetStateHandler` uses the same cap for its
    // whole-state serialize-and-measure check so an oversize write
    // is comparable to an oversize excerpt.
    let body_excerpt_max_bytes = engine_config.max_output_bytes;

    // Spawn MCP servers declared in `Pipeline.mcp_servers`.
    // Each server initializes once before the engine runs. `McpToolHandler`
    // instances are built per-tool and added to the agent's tool surface.
    // On failure, `McpServerCrashed` is emitted and the run aborts.
    let mut mcp_clients: Vec<Arc<SharedMcpClient>> = Vec::new();
    let mut mcp_tools: Vec<Arc<dyn ToolHandler>> = Vec::new();
    // Per-server advertised tool names, captured so wildcard entries
    // (`mcp.<server>.*`) in agent allowlists can be expanded once every
    // server has handshaked. We can't expand at load time — the real
    // tool list is only known after `tools/list` returns.
    let mut server_tool_names: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (server_name, server_cfg) in &pipeline.mcp_servers {
        let transport_label = match server_cfg {
            McpServerConfig::Stdio(_) => "stdio",
            McpServerConfig::Http(_) => "http",
            _ => "unknown",
        };
        sink.record(Event::McpServerStarting {
            run_id: run_id.clone(),
            server: server_name.clone(),
            transport: transport_label.to_string(),
        })
        .await;

        let raw: Box<dyn McpClient> = match server_cfg {
            McpServerConfig::Stdio(cfg) => {
                Box::new(RmcpClient::new_stdio(server_name.clone(), cfg))
            },
            McpServerConfig::Http(cfg) => Box::new(RmcpClient::new_http(server_name.clone(), cfg)),
            _ => {
                bail!("unsupported MCP transport for server `{server_name}`");
            },
        };
        let shared = Arc::new(SharedMcpClient {
            server: server_name.clone(),
            inner: tokio::sync::Mutex::new(raw),
        });

        // Drop the MutexGuard before entering the match so `shared` can be
        // moved into `mcp_clients` inside the Ok arm.
        let init_result = shared.inner.lock().await.initialize().await;
        match init_result {
            Ok(tools) => {
                let tool_count = u32::try_from(tools.len()).unwrap_or(u32::MAX);
                sink.record(Event::McpServerHandshaked {
                    run_id: run_id.clone(),
                    server: server_name.clone(),
                    tool_count,
                })
                .await;

                let mut tool_names: Vec<String> = Vec::with_capacity(tools.len());
                for tool in &tools {
                    tool_names.push(tool.name.clone());
                    mcp_tools.push(Arc::new(McpToolHandler::new(
                        McpToolHandlerConfig {
                            yaml_name: format!("mcp.{server_name}.{}", tool.name),
                            server: server_name.clone(),
                            tool: tool.name.clone(),
                            description: tool.description.clone(),
                            schema: tool.schema.clone(),
                            body_excerpt_max_bytes,
                        },
                        shared.clone() as Arc<dyn McpClient>,
                        sink.clone(),
                    )));
                }
                server_tool_names.insert(server_name.clone(), tool_names);

                mcp_clients.push(shared);
            },
            Err(e) => {
                sink.record(Event::McpServerCrashed {
                    run_id: run_id.clone(),
                    server: server_name.clone(),
                    reason: e.to_string(),
                })
                .await;
                // Drain already-initialized clients in declaration order
                // so a mid-init crash never leaks live MCP subprocesses.
                // Reuses the same shutdown sequence as the success path
                // at the bottom of `run()`.
                shutdown_mcp_servers(&sink, &run_id, &mcp_clients).await;
                // No nodes ran on this path, so the lifecycle aggregate
                // vectors are empty. The CLI owns the `RunStarted` /
                // `RunFinished { ok: false }` pair on the crash path
                // because `Engine::run` never gets a chance to (H2).
                emit_run_lifecycle_failure(&sink, &run_id, Vec::new(), Vec::new()).await;
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("MCP server `{server_name}` failed to initialize"));
            },
        }
    }

    // Expand `mcp.<server>.*` wildcards in every agent's `allowed_tools`
    // against the tool list each server actually advertised. Done before
    // `LoopAgent` construction so the cloned `AgentConfig` each
    // `SubagentHandler` receives carries the expanded list and the
    // agent loop's pre-flight `find_handler` lookup sees only exact
    // names. See `pipeline::load::expand_mcp_wildcards` for the dedupe
    // and ordering contract.
    pipeline::load::expand_mcp_wildcards(&mut pipeline, &server_tool_names)
        .context("expanding MCP tool wildcards in agent allowed_tools")?;

    // Built-in tool set: Bash, Read, Edit, Write, WebFetch, SetState. `LoopAgent`
    // gates each call against the per-agent `AgentPolicy.allowed_tools`
    // list, so an agent that does not opt into a handler cannot reach
    // it — the registration here is the availability ceiling, not the
    // default. `SetStateHandler` shares the run-level redactor so
    // `secrets.*` leaves are scrubbed before state reaches the wire.
    let mut builtin_tools: Vec<Arc<dyn ToolHandler>> = vec![
        Arc::new(BashHandler),
        Arc::new(ReadHandler),
        Arc::new(WriteHandler),
        Arc::new(EditHandler),
        Arc::new(WebFetchHandler::default()),
        Arc::new(SetStateHandler::new(
            redactor.clone(),
            body_excerpt_max_bytes,
        )),
    ];
    builtin_tools.extend(mcp_tools);

    // Tool tape wiring: wrap every handler so all calls are recorded to or
    // replayed from a single NDJSON file. One shared BufWriter so all tool
    // results land in order in the same file (RecordingToolHandler::with_shared_tape).
    let mut tool_tape_to_flush: Option<Arc<Mutex<std::io::BufWriter<std::fs::File>>>> = None;
    if let Some(path) = &flags.record_tool_tape {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("creating tool tape `{}`", path.display()))?;
        let shared = Arc::new(Mutex::new(std::io::BufWriter::new(file)));
        let path_buf = path.clone();
        builtin_tools = builtin_tools
            .into_iter()
            .map(|h| {
                Arc::new(RecordingToolHandler::with_shared_tape(
                    h,
                    shared.clone(),
                    path_buf.clone(),
                    redactor.clone(),
                )) as Arc<dyn ToolHandler>
            })
            .collect();
        tool_tape_to_flush = Some(shared);
    } else if let Some(path) = &flags.replay_tool_tape {
        let mut replay_tools = Vec::with_capacity(builtin_tools.len());
        for h in builtin_tools {
            let name = h.name().to_string();
            let replay = ReplayToolHandler::load(h, path)
                .with_context(|| format!("loading tool tape for handler `{name}`"))?;
            replay_tools.push(Arc::new(replay) as Arc<dyn ToolHandler>);
        }
        builtin_tools = replay_tools;
    }

    // Build the `LoopAgent` inside `Arc::new_cyclic` so each
    // `SubagentHandler` can hold a `Weak<LoopAgent>` back-pointer into
    // the same agent its tool vector lives on. A plain `Arc` would
    // complete a cycle (LoopAgent → tools → SubagentHandler → LoopAgent)
    // and leak the agent forever; the `Weak` form breaks the cycle while
    // keeping dispatch O(1) on the hot path.
    //
    // One handler per entry in `pipeline.agents`: the YAML form
    // `subagent.<name>` is the same string the parent's `allowed_tools`
    // references, so registration key = handler name = allowlist entry.
    let event_sink = sink.clone();
    let loop_agent: Arc<LoopAgent> = Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
        let mut tools = builtin_tools.clone();
        for (name, cfg) in &pipeline.agents {
            tools.push(Arc::new(SubagentHandler::new(
                format!("subagent.{name}"),
                name.clone(),
                cfg.clone(),
                weak.clone(),
                event_sink.clone(),
            )));
        }
        LoopAgent::new(LoopAgentConfig {
            transport,
            sink: event_sink.clone(),
            redactor,
            body_excerpt_max_bytes,
            tools,
        })
    });

    let agent: Arc<dyn Agent> = loop_agent;
    registry.register("agent", Arc::new(AgentExecutor::from_agent(agent)));
    let registry = Arc::new(registry);

    let templates = Arc::new(TemplateEngine::new());

    let engine = Engine::new(sink.clone(), registry, templates, engine_config);

    // SIGINT handling: race `engine.run` against `ctrl_c`. On a clean
    // exit we proceed with the existing post-run flow. On SIGINT we
    // emit a sentinel `RunFinished { ok: false, failed_nodes:
    // ["<interrupted>"] }`, flush the recording handles so the tape
    // captures whatever did reach disk, drain MCP servers under a 5s
    // timeout (so a stuck child cannot prevent shutdown), and bubble
    // up `Interrupted` — `main` downcasts that sentinel error and
    // translates it to exit code 130 so a wrapping shell can tell
    // operator cancellation apart from a normal pipeline failure
    // (which exits 1).
    let run_outcome = tokio::select! {
        biased;
        () = super::sigint_with_warning() => {
            sink.record(Event::RunFinished {
                run_id: run_id.clone(),
                ok: false,
                failed_nodes: vec![INTERRUPTED_FAILED_NODE.to_string()],
                skipped_nodes: Vec::new(),
            })
            .await;
            if let Some(rec) = &recording_transport
                && let Err(e) = rec.flush()
            {
                tracing::warn!(error = ?e, "flushing LLM tape on SIGINT failed");
            }
            if let Some(tape) = &tool_tape_to_flush
                && let Ok(mut guard) = tape.lock()
                && let Err(e) = guard.flush()
            {
                tracing::warn!(error = ?e, "flushing tool tape on SIGINT failed");
            }
            let shutdown_fut = shutdown_mcp_servers(&sink, &run_id, &mcp_clients);
            if tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, shutdown_fut).await.is_err() {
                tracing::warn!(
                    timeout_secs = MCP_SHUTDOWN_TIMEOUT.as_secs(),
                    "MCP shutdown exceeded grace window on SIGINT"
                );
            }
            return Err(super::Interrupted.into());
        }
        res = engine.run(&run_id, &pipeline, inputs) => res?,
    };

    if let Some(rec) = recording_transport {
        rec.flush().context("flushing LLM tape after run")?;
    }

    if let Some(tape) = &tool_tape_to_flush {
        tape.lock()
            .expect("tool tape mutex poisoned")
            .flush()
            .context("flushing tool tape after run")?;
    }

    // Bundle assembly: combine the two component tapes plus the
    // verbatim pipeline YAML into a single NDJSON file. Done after
    // both tapes are flushed so the bundle reader sees fully-written
    // sources. Temp tape files are removed only after the bundle
    // writes successfully so a mid-assembly failure leaves the raw
    // tapes diagnosable on disk.
    if let Some((bundle_path, llm_tmp, tool_tmp)) = bundle_paths {
        let pipeline_yaml = std::fs::read_to_string(path)
            .with_context(|| format!("reading pipeline YAML `{}` for bundle", path.display()))?;
        orno_core::llm::write_bundle(
            &pipeline_yaml,
            Some(&llm_tmp),
            Some(&tool_tmp),
            &bundle_path,
        )
        .with_context(|| format!("writing bundle `{}`", bundle_path.display()))?;
        // Best-effort cleanup: a failure to remove a temp file is not
        // fatal — the bundle is already written and a stale `.tmp`
        // sitting next to it is a recoverable mess, not a data loss.
        drop(std::fs::remove_file(&llm_tmp));
        drop(std::fs::remove_file(&tool_tmp));
    }

    // Shut down MCP servers in declaration order. Best-effort:
    // a failing shutdown emits a warning but does not abort a successful run.
    shutdown_mcp_servers(&sink, &run_id, &mcp_clients).await;

    // The streaming sink swallows write errors so that a single
    // EPIPE during the run does not poison every subsequent
    // `record` call — but we still owe the operator a non-zero
    // exit. Checked after MCP shutdown so the shutdown envelopes
    // had a chance to fail too. Done last so a clean run still
    // reports `Ok(())`.
    if streaming_sink.is_broken() {
        bail!("event stream write failed (downstream closed?); run output is incomplete");
    }

    // Stream-level failure → process-level failure. Until this point
    // the CLI exited 0 even when nodes failed, so wrappers like
    // `orno run | tee log` could not branch on success. The diagnostic
    // names the specific failed nodes so the operator does not have
    // to re-read the NDJSON to find them. Skipped descendants are
    // visible in the stream as `node_skipped` envelopes; we do not
    // re-list them here to keep the CLI message tight.
    if !run_outcome.ok {
        let names = if run_outcome.failed_nodes.is_empty() {
            "<unknown>".to_string()
        } else {
            run_outcome.failed_nodes.join(", ")
        };
        bail!("run failed: nodes [{names}] reported ok:false");
    }

    Ok(())
}

/// Emit the `RunStarted` + `RunFinished { ok: false, .. }` pair the CLI
/// owes when a pre-engine failure (e.g. MCP init crash) prevents
/// `Engine::run` from ever running. Keeps the event stream well-formed
/// for downstream consumers — every successful run still gets exactly
/// one `RunStarted` from the engine; a pre-engine-crash run gets
/// exactly one from the CLI.
async fn emit_run_lifecycle_failure(
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    failed_nodes: Vec<String>,
    skipped_nodes: Vec<String>,
) {
    sink.record(Event::RunStarted {
        run_id: run_id.to_string(),
    })
    .await;
    sink.record(Event::RunFinished {
        run_id: run_id.to_string(),
        ok: false,
        failed_nodes,
        skipped_nodes,
    })
    .await;
}

/// Drain MCP servers in declaration order, emitting the
/// `McpServerShuttingDown` / `McpServerExited` envelope pair for each
/// one. Used by the success path, the MCP-init crash recovery path,
/// and the SIGINT path so all three keep the same wire-level
/// shutdown framing — anything observing the stream sees the same
/// shape regardless of why the run is winding down. Best-effort:
/// a failing `shutdown()` only surfaces as a `tracing::warn!`; the
/// process exits anyway.
async fn shutdown_mcp_servers(
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    clients: &[Arc<SharedMcpClient>],
) {
    for client in clients {
        sink.record(Event::McpServerShuttingDown {
            run_id: run_id.to_string(),
            server: client.server.clone(),
        })
        .await;
        if let Err(e) = client.inner.lock().await.shutdown().await {
            tracing::warn!(
                server = %client.server,
                error = ?e,
                "MCP server shutdown failed",
            );
        }
        sink.record(Event::McpServerExited {
            run_id: run_id.to_string(),
            server: client.server.clone(),
        })
        .await;
    }
}

/// Resolve `env` and `secrets` per the documented precedence order.
///
/// - `env.*`: `pass_env:` (process env) < `--env-file` (later files
///   shadow earlier) < `-e KEY=VAL` (last flag wins).
/// - `secrets.*`: process env for names in `secrets:` <
///   `--secrets-file` (later files shadow earlier).
///
/// Classification is by name. A binding whose key is declared in the
/// pipeline's `secrets:` block always lands in `secrets.*`, even when
/// it arrives via an env file. A secret on `-e` is refused outright —
/// `argv` leaks into `HISTFILE` and `ps`.
fn resolve_inputs(pipeline: &Pipeline, flags: &RunFlags) -> Result<RunInputs> {
    let declared_secrets: HashSet<&str> = pipeline.secrets.iter().map(String::as_str).collect();

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut secrets: BTreeMap<String, String> = BTreeMap::new();

    for name in &pipeline.pass_env {
        if let Ok(val) = std::env::var(name) {
            env.insert(name.clone(), val);
        }
    }

    for name in &pipeline.secrets {
        if let Ok(val) = std::env::var(name) {
            secrets.insert(name.clone(), val);
        }
    }

    for file in &flags.env_files {
        for (k, v) in parse_dotenv(file)? {
            if declared_secrets.contains(k.as_str()) {
                secrets.insert(k, v);
            } else {
                env.insert(k, v);
            }
        }
    }

    for file in &flags.secrets_files {
        for (k, v) in parse_dotenv(file)? {
            secrets.insert(k, v);
        }
    }

    for item in &flags.inline_env {
        let (k, v) = parse_inline_env(item)?;
        if declared_secrets.contains(k.as_str()) {
            bail!("refusing to accept secret `{k}` via `-e`; use `--secrets-file` instead");
        }
        env.insert(k, v);
    }

    let mut inputs = RunInputs::default();
    inputs.env = env;
    inputs.secrets = secrets;
    Ok(inputs)
}

fn parse_inline_env(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `KEY=VAL`, got `{s}`"))?;
    if k.is_empty() {
        bail!("empty key in `-e {s}`");
    }
    if v.is_empty() {
        tracing::warn!(key = %k, "empty value for `-e {k}=` — did you mean to unset this variable?");
    }
    Ok((k.to_string(), v.to_string()))
}

/// Dotenv parser supporting the dialect features common to bash-style
/// `.env` files: `KEY=VAL` per line, blank lines and full-line `#`
/// comments skipped, optional `export` prefix stripped, double-quoted
/// or single-quoted values (literal contents, no escape processing),
/// and inline `# comment` after an unquoted value when preceded by
/// whitespace. Variable expansion (`$OTHER`) is intentionally not
/// supported — the parser must not silently change a secret literal.
/// Later duplicate keys within the same file win on the consumer side
/// via `BTreeMap::insert`.
fn parse_dotenv(path: &Path) -> Result<Vec<(String, String)>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading env file `{}`", path.display()))?;
    let mut out = Vec::new();
    for (lineno, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip the optional `export ` prefix (literal — a single ASCII
        // space). Other whitespace (e.g. tabs) between `export` and the
        // key is uncommon in real .env files and easy to flag as the
        // user's typo by failing the `KEY=VAL` parse below.
        let body = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (k, raw_value) = body.split_once('=').ok_or_else(|| {
            anyhow!(
                "`{}` line {}: expected `KEY=VAL`, got `{}`",
                path.display(),
                lineno + 1,
                line,
            )
        })?;
        let key = k.trim();
        if key.is_empty() {
            bail!("`{}` line {}: empty key", path.display(), lineno + 1);
        }
        let value = parse_dotenv_value(raw_value).with_context(|| {
            format!(
                "`{}` line {}: malformed value for `{key}`",
                path.display(),
                lineno + 1,
            )
        })?;
        out.push((key.to_string(), value));
    }
    Ok(out)
}

/// Decode the right-hand side of a `KEY=VAL` line according to the
/// dotenv dialect documented on `parse_dotenv`. Single- and
/// double-quoted forms are literal — no escape sequences are
/// interpreted, so a secret containing `\n` round-trips byte-for-byte.
fn parse_dotenv_value(raw: &str) -> Result<String> {
    let trimmed = raw.trim_start();
    // Empty value, or value area is only an inline comment.
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(String::new());
    }

    if let Some(rest) = trimmed.strip_prefix('"') {
        let end = rest
            .find('"')
            .ok_or_else(|| anyhow!("unterminated double-quoted value"))?;
        let value = &rest[..end];
        let tail = rest[end + 1..].trim_start();
        if !tail.is_empty() && !tail.starts_with('#') {
            bail!("trailing content after closing double-quote: `{tail}`");
        }
        return Ok(value.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        let end = rest
            .find('\'')
            .ok_or_else(|| anyhow!("unterminated single-quoted value"))?;
        let value = &rest[..end];
        let tail = rest[end + 1..].trim_start();
        if !tail.is_empty() && !tail.starts_with('#') {
            bail!("trailing content after closing single-quote: `{tail}`");
        }
        return Ok(value.to_string());
    }

    // Unquoted value. An inline comment starts at the first whitespace
    // followed by `#`; a `#` directly inside the token (e.g. a password
    // with `#` characters) is preserved.
    let cut = trimmed
        .find(" #")
        .or_else(|| trimmed.find("\t#"))
        .unwrap_or(trimmed.len());
    Ok(trimmed[..cut].trim_end().to_string())
}

#[cfg(test)]
mod parse_dotenv_tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Write `body` to a temp file and parse it through `parse_dotenv`,
    /// returning the parsed (key, value) pairs.
    fn parse(body: &str) -> Result<Vec<(String, String)>> {
        let mut tmp = NamedTempFile::new().expect("temp file");
        tmp.write_all(body.as_bytes()).expect("write");
        parse_dotenv(tmp.path())
    }

    #[test]
    fn plain_key_value_lines_parse() {
        let out = parse("FOO=bar\nBAZ=qux\n").expect("parse");
        assert_eq!(
            out,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn full_line_comments_and_blank_lines_are_skipped() {
        let out = parse("# comment\n\nFOO=bar\n# another\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn export_prefix_is_stripped() {
        let out = parse("export FOO=bar\nexport BAZ=qux\n").expect("parse");
        assert_eq!(
            out,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn double_quoted_value_strips_quotes_and_preserves_internal_whitespace() {
        let out = parse("FOO=\"  spaced value  \"\n").expect("parse");
        assert_eq!(
            out,
            vec![("FOO".to_string(), "  spaced value  ".to_string())]
        );
    }

    #[test]
    fn single_quoted_value_strips_quotes_and_preserves_internal_hash() {
        let out = parse("FOO='val#with#hash'\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "val#with#hash".to_string())]);
    }

    #[test]
    fn quoted_value_may_be_followed_by_inline_comment() {
        let out = parse("FOO=\"value\" # trailing comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "value".to_string())]);
    }

    #[test]
    fn unquoted_inline_comment_strips_at_first_space_hash() {
        let out = parse("FOO=bar # this is a comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn unquoted_value_keeps_internal_hash_without_preceding_whitespace() {
        // A value like `pa#word` has no space before the `#`, so the
        // `#` is part of the value, not the start of a comment.
        let out = parse("FOO=pa#word\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "pa#word".to_string())]);
    }

    #[test]
    fn empty_value_is_empty_string() {
        let out = parse("FOO=\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), String::new())]);
    }

    #[test]
    fn value_area_that_is_only_a_comment_yields_empty_string() {
        let out = parse("FOO=  # only comment\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), String::new())]);
    }

    #[test]
    fn unterminated_double_quote_is_rejected() {
        let err = parse("FOO=\"unterminated\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unterminated double-quoted"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("FOO"), "must name the offending key: {msg}");
    }

    #[test]
    fn unterminated_single_quote_is_rejected() {
        let err = parse("FOO='unterminated\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unterminated single-quoted"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn trailing_content_after_closing_quote_is_rejected() {
        let err = parse("FOO=\"value\" garbage\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("trailing content"), "unexpected error: {msg}");
    }

    #[test]
    fn missing_equals_sign_is_rejected_with_line_number() {
        let err = parse("FOO\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "must include line number: {msg}");
        assert!(msg.contains("KEY=VAL"), "must show expected form: {msg}");
    }

    #[test]
    fn empty_key_is_rejected() {
        let err = parse("=value\n").expect_err("must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("empty key"), "unexpected error: {msg}");
    }

    #[test]
    fn value_with_equals_sign_inside_keeps_after_first_equals() {
        let out = parse("FOO=key=value=more\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "key=value=more".to_string())]);
    }

    #[test]
    fn quoted_value_may_contain_equals_sign() {
        let out = parse("FOO=\"a=b=c\"\n").expect("parse");
        assert_eq!(out, vec![("FOO".to_string(), "a=b=c".to_string())]);
    }

    #[test]
    fn export_prefix_combines_with_quoted_value() {
        let out = parse("export TOKEN=\"sk-abc-123\"\n").expect("parse");
        assert_eq!(out, vec![("TOKEN".to_string(), "sk-abc-123".to_string())]);
    }
}
