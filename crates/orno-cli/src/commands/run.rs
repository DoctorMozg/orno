//! `orno run <pipeline.yaml>` — load, resolve env/secrets, dispatch, stream.
//!
//! Assembles a `NodeRegistry` over `ShellExecutor` and `AgentExecutor`
//! (wrapping a `GenAiTransport`), threads them plus a
//! `TemplateEngine` into `Engine::run`, and prints every recorded
//! envelope as NDJSON on stdout.
//!
//! Env and secrets resolution lives in this module (ADR 0020):
//! process env, `--env-file`, `--secrets-file`, and `-e` flags
//! combine into the two disjoint namespaces the engine sees.
//! Classification follows the pipeline's `secrets:` declaration,
//! never the source file — a secret name in `--env-file` is still
//! routed to `secrets.*`.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;

use orno_core::McpError;
use orno_core::agent::{Agent, LoopAgent, LoopAgentConfig};
use orno_core::events::{Event, EventSink, Redactor, StreamingSink};
use orno_core::execution::{Engine, EngineConfig, RunInputs, new_run_id};
use orno_core::llm::{
    DummyTransport, GenAiTransport, LlmTransport, RecordingTransport, ReplayTransport,
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
    BashHandler, EditHandler, McpToolHandler, McpToolHandlerConfig, ReadHandler, SetStateHandler,
    SubagentHandler, ToolHandler, WebFetchHandler, WriteHandler,
};
use serde_json::Value;

/// Test-only escape hatch: when set to `dummy`, `orno run` swaps
/// `GenAiTransport` for `DummyTransport` so integration tests can
/// snapshot the event stream without a live API key. The var name
/// is intentionally awkward — end users should never set it.
/// Record/replay tape wiring (Phase 7) will subsume this.
const TEST_TRANSPORT_ENV: &str = "ORNO_TEST_LLM_TRANSPORT";

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
    /// When `Some`, wrap the live transport in `RecordingTransport`
    /// and flush to this path at run end. Mutually exclusive with
    /// `replay_tape`.
    pub record_tape: Option<PathBuf>,
    /// When `Some`, use `ReplayTransport` instead of the live transport.
    /// Mutually exclusive with `record_tape`.
    pub replay_tape: Option<PathBuf>,
}

#[expect(
    clippy::too_many_lines,
    reason = "run() is the top-level orchestrator for orno run; splitting it adds indirection without reducing conceptual load"
)]
pub async fn run(path: &Path, flags: RunFlags) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    let engine_config = EngineConfig {
        verbose: flags.verbose,
        max_output_bytes: flags.max_output_bytes,
    };

    let inputs = resolve_inputs(&pipeline, &flags)?;

    let sink: Arc<dyn EventSink> = Arc::new(StreamingSink::stdout());

    let base_transport: Arc<dyn LlmTransport> = match std::env::var(TEST_TRANSPORT_ENV).as_deref() {
        Ok("dummy") => Arc::new(DummyTransport),
        _ => Arc::new(
            GenAiTransport::from_agents(&pipeline.agents, &inputs.secrets)
                .context("constructing LLM transport from pipeline agents")?,
        ),
    };

    // --replay-tape: swap the live transport for a tape reader. A tape
    // miss is a hard error — no fallback to the live API (ADR 0005 §5).
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
            RecordingTransport::create(base_transport, path)
                .with_context(|| format!("creating record tape `{}`", path.display()))?,
        );
        recording_transport = Some(rec.clone());
        rec
    } else {
        base_transport
    };

    // Build the redactor once from the resolved `secrets.*` map and
    // share it with the agent executor. The engine builds its own
    // instance inside `Engine::run` from the same secret map — the two
    // instances carry the same value list, so redaction is consistent
    // across agent-emitted `LlmRequestStarted` excerpts and
    // scheduler-emitted `NodeFailure` tails (ADR 0020 / 0024). Using
    // an `Arc` avoids cloning the secret-value list per `LlmRequest`.
    let redactor = Arc::new(Redactor::new(&inputs.secrets));

    let mut registry = NodeRegistry::new();
    registry.register("shell", Arc::new(ShellExecutor));

    // Reuse the engine's `max_output_bytes` for the LLM body excerpt
    // cap so a truncated stderr tail, a truncated HTTP error body, and
    // a truncated prompt/response excerpt all look alike to a log
    // reader (ADR 0023 / 0024). `SetStateHandler` (ADR 0025 §5) uses
    // the same cap for its whole-state serialize-and-measure check so
    // an oversize write is comparable to an oversize excerpt.
    let body_excerpt_max_bytes = engine_config.max_output_bytes;

    // run_id must be minted before MCP lifecycle events are emitted so
    // they carry a valid envelope correlation id.
    let run_id = new_run_id();

    // Spawn MCP servers declared in `Pipeline.mcp_servers` (ADR 0007).
    // Each server initializes once before the engine runs. `McpToolHandler`
    // instances are built per-tool and added to the agent's tool surface.
    // On failure, `McpServerCrashed` is emitted and the run aborts.
    let mut mcp_clients: Vec<Arc<SharedMcpClient>> = Vec::new();
    let mut mcp_tools: Vec<Arc<dyn ToolHandler>> = Vec::new();

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

                for tool in &tools {
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

                mcp_clients.push(shared);
            },
            Err(e) => {
                sink.record(Event::McpServerCrashed {
                    run_id: run_id.clone(),
                    server: server_name.clone(),
                    reason: e.to_string(),
                })
                .await;
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("MCP server `{server_name}` failed to initialize"));
            },
        }
    }

    // Built-in tool set per ADR 0008 + ADR 0025 (SetState). `LoopAgent`
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
        Arc::new(WebFetchHandler),
        Arc::new(SetStateHandler::new(
            redactor.clone(),
            body_excerpt_max_bytes,
        )),
    ];
    builtin_tools.extend(mcp_tools);

    // ADR 0006: build the `LoopAgent` inside `Arc::new_cyclic` so each
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

    engine.run(&run_id, &pipeline, inputs).await?;

    if let Some(rec) = recording_transport {
        rec.flush().context("flushing LLM tape after run")?;
    }

    // Shut down MCP servers in declaration order. Best-effort per ADR 0007:
    // a failing shutdown emits a warning but does not abort a successful run.
    for client in &mcp_clients {
        sink.record(Event::McpServerShuttingDown {
            run_id: run_id.clone(),
            server: client.server.clone(),
        })
        .await;
        if let Err(e) = client.inner.lock().await.shutdown().await {
            tracing::warn!(server = %client.server, error = ?e, "MCP server shutdown failed");
        }
        sink.record(Event::McpServerExited {
            run_id: run_id.clone(),
            server: client.server.clone(),
        })
        .await;
    }

    Ok(())
}

/// Resolve `env` and `secrets` per ADR 0020 precedence.
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
            bail!(
                "refusing to accept secret `{k}` via `-e`; use `--secrets-file` instead (ADR 0020)",
            );
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

/// Minimal dotenv parser: `KEY=VAL` per line, `#` comments, blank
/// lines skipped. No quoting, no variable expansion — ADR 0020
/// keeps the v0.1 surface intentionally narrow. Later duplicate
/// keys within the same file win on the consumer side via
/// `BTreeMap::insert`.
fn parse_dotenv(path: &Path) -> Result<Vec<(String, String)>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading env file `{}`", path.display()))?;
    let mut out = Vec::new();
    for (lineno, raw) in contents.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| {
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
        out.push((key.to_string(), v.to_string()));
    }
    Ok(out)
}
