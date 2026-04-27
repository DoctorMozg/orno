//! `orno run <pipeline.yaml>` — load, resolve env/secrets, dispatch, stream.
//!
//! Assembles a `NodeRegistry` over `ShellExecutor` and `AgentExecutor`
//! (wrapping a `GenAiTransport`), threads them plus a
//! `TemplateEngine` into `Engine::run`, and prints every recorded
//! envelope as NDJSON on stdout.
//!
//! Sub-modules:
//! - `agent`   — builtin tool construction, tool-tape wrap, `LoopAgent` + `Engine` wiring
//! - `secrets` — dotenv parsing, inline `-e` handling, `resolve_inputs`
//! - `transport` — `GenAiTransport` / `ReplayTransport` / `RecordingTransport` selection

mod agent;
mod secrets;
mod transport;

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use orno_core::McpError;
use orno_core::events::{Event, EventSink, Redactor, StreamingSink};
use orno_core::execution::{EngineConfig, new_run_id};
use orno_core::llm::RecordingTransport;
use orno_core::mcp::{McpClient, McpTool, McpToolCallResult, RmcpClient};
use orno_core::pipeline;
use orno_core::pipeline::Pipeline;
use orno_core::pipeline::schema::McpServerConfig;
use orno_core::tool::{McpToolHandler, McpToolHandlerConfig, ToolHandler};
use serde_json::Value;

use agent::{
    BuildLoopAgentArgs, assemble_builtin_tools, build_engine, build_loop_agent, wrap_tool_tape,
};
use secrets::resolve_inputs;
use transport::build_transport;

/// Live `LlmTransport` paired with the optional `RecordingTransport`
/// wrapper kept around so the run path can call `flush()` after the
/// engine returns. Both `Arc`s point at the same value when recording
/// is enabled.
type TransportPair = (
    Arc<dyn orno_core::llm::LlmTransport>,
    Option<Arc<RecordingTransport>>,
);

/// Shared `BufWriter` handle the run path flushes after a successful
/// or interrupted run. `None` when neither `--record-tool-tape` nor
/// `--replay-tool-tape` is set.
type ToolTapeHandle = Arc<Mutex<std::io::BufWriter<std::fs::File>>>;

/// Tool-handler vector paired with the optional shared tape writer.
/// Returned by [`wrap_tool_tape`].
type ToolTapePair = (Vec<Arc<dyn ToolHandler>>, Option<ToolTapeHandle>);

/// All state produced by [`spawn_mcp_servers`]: live clients (drained
/// in declaration order at run end), per-tool handlers added to the
/// agent surface, and the per-server tool-name map used to expand
/// `mcp.<server>.*` wildcards.
type McpSpawn = (
    Vec<Arc<SharedMcpClient>>,
    Vec<Arc<dyn ToolHandler>>,
    BTreeMap<String, Vec<String>>,
);

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

pub async fn run(path: &Path, mut flags: RunFlags) -> Result<()> {
    let mut pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    let bundle_paths = derive_bundle_paths(flags.record_bundle.as_ref())?;
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

    // Reuse the engine's `max_output_bytes` for the LLM body excerpt
    // cap so a truncated stderr tail, a truncated HTTP error body, and
    // a truncated prompt/response excerpt all look alike to a log
    // reader.
    let body_excerpt_max_bytes = engine_config.max_output_bytes;

    let redactor = Arc::new(Redactor::new(&inputs.secrets));
    let (transport, recording_transport) =
        build_transport(&flags, &pipeline, &inputs.secrets, &redactor)?;
    let (mcp_clients, mcp_tools, server_tool_names) =
        spawn_mcp_servers(&pipeline, &sink, &run_id, body_excerpt_max_bytes).await?;

    // Expand `mcp.<server>.*` wildcards before agent construction so
    // the cloned `AgentConfig` each `SubagentHandler` receives carries
    // the expanded list. See `pipeline::load::expand_mcp_wildcards`.
    pipeline::load::expand_mcp_wildcards(&mut pipeline, &server_tool_names)
        .context("expanding MCP tool wildcards in agent allowed_tools")?;

    let builtin_tools = assemble_builtin_tools(&redactor, body_excerpt_max_bytes, mcp_tools);
    let (builtin_tools, tool_tape_to_flush) = wrap_tool_tape(builtin_tools, &flags, &redactor)?;
    let loop_agent = build_loop_agent(BuildLoopAgentArgs {
        transport,
        builtin_tools,
        pipeline: &pipeline,
        sink: sink.clone(),
        redactor,
        body_excerpt_max_bytes,
    });
    let engine = build_engine(sink.clone(), loop_agent, engine_config);

    let run_outcome = tokio::select! {
        biased;
        () = super::sigint_with_warning() => {
            handle_sigint(
                &sink,
                &run_id,
                recording_transport.as_ref(),
                tool_tape_to_flush.as_ref(),
                &mcp_clients,
            ).await;
            return Err(super::Interrupted.into());
        }
        res = engine.run(&run_id, &pipeline, inputs) => res?,
    };

    flush_and_assemble(
        recording_transport,
        tool_tape_to_flush.as_ref(),
        bundle_paths,
        path,
    )?;
    shutdown_mcp_servers(&sink, &run_id, &mcp_clients).await;
    finalize_run(&streaming_sink, &run_outcome)
}

/// Derive the `(bundle, llm_tmp, tool_tmp)` paths that back
/// `--record-bundle`. Returns `None` when bundling is disabled.
///
/// Each tmp path is reserved through `tempfile::Builder` so its final
/// component carries an unguessable random suffix — an attacker who
/// knows the bundle path can no longer pre-create a symlink at the
/// derived `.llm.tmp` / `.tool.tmp` site to redirect later writes. The
/// placeholder `NamedTempFile` is dropped before this function returns
/// (which deletes the on-disk file), leaving only the unique path: the
/// downstream openers (`RecordingTransport::create` uses `O_EXCL`,
/// `wrap_tool_tape` uses `OpenOptions`) can then create the real files
/// at those paths without colliding. The race window between drop and
/// re-create is irrelevant because the path is unguessable.
fn derive_bundle_paths(
    record_bundle: Option<&PathBuf>,
) -> Result<Option<(PathBuf, PathBuf, PathBuf)>> {
    let Some(bundle) = record_bundle else {
        return Ok(None);
    };
    let parent = bundle.parent().unwrap_or_else(|| Path::new("."));
    let llm = tempfile::Builder::new()
        .prefix("orno-llm-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "reserving temporary LLM tape path next to bundle `{}`",
                bundle.display()
            )
        })?
        .path()
        .to_path_buf();
    let tool = tempfile::Builder::new()
        .prefix("orno-tool-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "reserving temporary tool tape path next to bundle `{}`",
                bundle.display()
            )
        })?
        .path()
        .to_path_buf();
    Ok(Some((bundle.clone(), llm, tool)))
}

/// Spawn every MCP server declared in `pipeline.mcp_servers`, run the
/// initial handshake, and collect the resulting per-tool handlers plus
/// the per-server tool-name map used to expand `mcp.<server>.*`
/// wildcards. On a handshake failure: emits `McpServerCrashed`, drains
/// already-initialized clients in declaration order, emits the
/// CLI-owned `RunStarted` / `RunFinished { ok: false }` lifecycle pair
/// (since `Engine::run` will not run), and returns the original error
/// with context.
async fn spawn_mcp_servers(
    pipeline: &Pipeline,
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    body_excerpt_max_bytes: usize,
) -> Result<McpSpawn> {
    let mut mcp_clients: Vec<Arc<SharedMcpClient>> = Vec::new();
    let mut mcp_tools: Vec<Arc<dyn ToolHandler>> = Vec::new();
    // Per-server advertised tool names, captured so wildcard entries
    // (`mcp.<server>.*`) in agent allowlists can be expanded once every
    // server has handshaked. We can't expand at load time — the real
    // tool list is only known after `tools/list` returns.
    let mut server_tool_names: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (server_name, server_cfg) in &pipeline.mcp_servers {
        match init_one_mcp_server(
            server_name,
            server_cfg,
            sink,
            run_id,
            body_excerpt_max_bytes,
        )
        .await
        {
            Ok((shared, tools, tool_names)) => {
                mcp_tools.extend(tools);
                server_tool_names.insert(server_name.clone(), tool_names);
                mcp_clients.push(shared);
            },
            Err(e) => {
                // Drain already-initialized clients in declaration order
                // so a mid-init crash never leaks live MCP subprocesses.
                // Reuses the same shutdown sequence as the success path
                // at the bottom of `run()`.
                shutdown_mcp_servers(sink, run_id, &mcp_clients).await;
                // No nodes ran on this path, so the lifecycle aggregate
                // vectors are empty. The CLI owns the `RunStarted` /
                // `RunFinished { ok: false }` pair on the crash path
                // because `Engine::run` never gets a chance to (H2).
                emit_run_lifecycle_failure(sink, run_id, Vec::new(), Vec::new()).await;
                return Err(e);
            },
        }
    }

    Ok((mcp_clients, mcp_tools, server_tool_names))
}

/// Spawn and handshake a single MCP server. Emits the
/// `McpServerStarting` / `McpServerHandshaked` envelopes on success
/// and `McpServerCrashed` on failure. Returns the shared client, the
/// per-tool handlers, and the advertised tool names. The caller is
/// responsible for draining already-initialized clients on error and
/// emitting the run-level lifecycle pair.
async fn init_one_mcp_server(
    server_name: &str,
    server_cfg: &McpServerConfig,
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    body_excerpt_max_bytes: usize,
) -> Result<(Arc<SharedMcpClient>, Vec<Arc<dyn ToolHandler>>, Vec<String>)> {
    let transport_label = match server_cfg {
        McpServerConfig::Stdio(_) => "stdio",
        McpServerConfig::Http(_) => "http",
        _ => "unknown",
    };
    sink.record(Event::McpServerStarting {
        run_id: run_id.to_string(),
        server: server_name.to_string(),
        transport: transport_label.to_string(),
    })
    .await;

    let raw: Box<dyn McpClient> = match server_cfg {
        McpServerConfig::Stdio(cfg) => {
            Box::new(RmcpClient::new_stdio(server_name.to_string(), cfg))
        },
        McpServerConfig::Http(cfg) => Box::new(RmcpClient::new_http(server_name.to_string(), cfg)),
        _ => {
            bail!("unsupported MCP transport for server `{server_name}`");
        },
    };
    let shared = Arc::new(SharedMcpClient {
        server: server_name.to_string(),
        inner: tokio::sync::Mutex::new(raw),
    });

    // Drop the MutexGuard before entering the match so `shared` can be
    // moved into the success tuple.
    let init_result = shared.inner.lock().await.initialize().await;
    let tools = match init_result {
        Ok(tools) => tools,
        Err(e) => {
            sink.record(Event::McpServerCrashed {
                run_id: run_id.to_string(),
                server: server_name.to_string(),
                reason: e.to_string(),
            })
            .await;
            return Err(anyhow::Error::from(e))
                .with_context(|| format!("MCP server `{server_name}` failed to initialize"));
        },
    };

    let tool_count = u32::try_from(tools.len()).unwrap_or(u32::MAX);
    sink.record(Event::McpServerHandshaked {
        run_id: run_id.to_string(),
        server: server_name.to_string(),
        tool_count,
    })
    .await;

    let (handlers, tool_names) =
        build_mcp_handlers(server_name, &tools, &shared, sink, body_excerpt_max_bytes);
    Ok((shared, handlers, tool_names))
}

/// Wrap every advertised MCP tool in an `McpToolHandler` bound to the
/// shared client. Returns the handlers in declaration order alongside
/// the captured tool names so the caller can populate the wildcard
/// expansion map.
fn build_mcp_handlers(
    server_name: &str,
    tools: &[McpTool],
    shared: &Arc<SharedMcpClient>,
    sink: &Arc<dyn EventSink>,
    body_excerpt_max_bytes: usize,
) -> (Vec<Arc<dyn ToolHandler>>, Vec<String>) {
    let mut handlers: Vec<Arc<dyn ToolHandler>> = Vec::with_capacity(tools.len());
    let mut tool_names: Vec<String> = Vec::with_capacity(tools.len());
    for tool in tools {
        tool_names.push(tool.name.clone());
        handlers.push(Arc::new(McpToolHandler::new(
            McpToolHandlerConfig {
                yaml_name: format!("mcp.{server_name}.{}", tool.name),
                server: server_name.to_string(),
                tool: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.schema.clone(),
                body_excerpt_max_bytes,
            },
            shared.clone() as Arc<dyn McpClient>,
            sink.clone(),
        )));
    }
    (handlers, tool_names)
}

/// Flush any record-mode tapes and, when `--record-bundle` is in use,
/// assemble the LLM tape, tool tape, and verbatim pipeline YAML into a
/// single NDJSON bundle. Bundle assembly happens after both component
/// tapes are flushed so the bundle reader sees fully-written sources.
/// Temp tape files are removed only after the bundle writes
/// successfully so a mid-assembly failure leaves the raw tapes
/// diagnosable on disk.
fn flush_and_assemble(
    recording_transport: Option<Arc<RecordingTransport>>,
    tool_tape_to_flush: Option<&ToolTapeHandle>,
    bundle_paths: Option<(PathBuf, PathBuf, PathBuf)>,
    pipeline_path: &Path,
) -> Result<()> {
    if let Some(rec) = recording_transport {
        rec.flush().context("flushing LLM tape after run")?;
    }

    if let Some(tape) = tool_tape_to_flush {
        tape.lock()
            .expect("tool tape mutex poisoned")
            .flush()
            .context("flushing tool tape after run")?;
    }

    if let Some((bundle_path, llm_tmp, tool_tmp)) = bundle_paths {
        let pipeline_yaml = std::fs::read_to_string(pipeline_path).with_context(|| {
            format!(
                "reading pipeline YAML `{}` for bundle",
                pipeline_path.display()
            )
        })?;
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

    Ok(())
}

/// Run the SIGINT cleanup sequence: emit the sentinel `RunFinished`,
/// flush both record-mode tapes (best-effort — a flush failure here
/// only logs a WARN), and drain MCP servers under
/// [`MCP_SHUTDOWN_TIMEOUT`]. The caller maps this to
/// `Err(super::Interrupted)` so `main` can translate the sentinel into
/// exit code 130.
async fn handle_sigint(
    sink: &Arc<dyn EventSink>,
    run_id: &str,
    recording_transport: Option<&Arc<RecordingTransport>>,
    tool_tape_to_flush: Option<&ToolTapeHandle>,
    mcp_clients: &[Arc<SharedMcpClient>],
) {
    sink.record(Event::RunFinished {
        run_id: run_id.to_string(),
        ok: false,
        failed_nodes: vec![INTERRUPTED_FAILED_NODE.to_string()],
        skipped_nodes: Vec::new(),
    })
    .await;
    if let Some(rec) = recording_transport
        && let Err(e) = rec.flush()
    {
        tracing::warn!(error = ?e, "flushing LLM tape on SIGINT failed");
    }
    if let Some(tape) = tool_tape_to_flush
        && let Ok(mut guard) = tape.lock()
        && let Err(e) = guard.flush()
    {
        tracing::warn!(error = ?e, "flushing tool tape on SIGINT failed");
    }
    let shutdown_fut = shutdown_mcp_servers(sink, run_id, mcp_clients);
    if tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, shutdown_fut)
        .await
        .is_err()
    {
        tracing::warn!(
            timeout_secs = MCP_SHUTDOWN_TIMEOUT.as_secs(),
            "MCP shutdown exceeded grace window on SIGINT"
        );
    }
}

/// Translate the engine's outcome and the streaming sink's broken
/// state into the CLI's process-level exit code. The streaming sink
/// swallows write errors during the run so a single EPIPE does not
/// poison every subsequent `record` — but a broken sink still owes
/// the operator a non-zero exit. Checked after MCP shutdown so the
/// shutdown envelopes had a chance to fail too.
fn finalize_run(
    streaming_sink: &Arc<StreamingSink>,
    run_outcome: &orno_core::execution::scheduler::RunOutcome,
) -> Result<()> {
    if streaming_sink.is_broken() {
        bail!("event stream write failed (downstream closed?); run output is incomplete");
    }

    // Skipped descendants are visible in the stream as `node_skipped`
    // envelopes; we do not re-list them here to keep the CLI message tight.
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
