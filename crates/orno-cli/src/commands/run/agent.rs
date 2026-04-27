//! Agent and engine assembly: builtin tool construction, tool-tape
//! wrapping, `LoopAgent` construction via `Arc::new_cyclic`, and
//! `Engine` registration.

use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};

use orno_core::agent::{Agent, LoopAgent, LoopAgentConfig};
use orno_core::events::{EventSink, Redactor};
use orno_core::execution::Engine;
use orno_core::execution::EngineConfig;
use orno_core::llm::LlmTransport;
use orno_core::node::NodeRegistry;
use orno_core::node::agent::AgentExecutor;
use orno_core::node::shell::ShellExecutor;
use orno_core::pipeline::Pipeline;
use orno_core::pipeline::template::TemplateEngine;
use orno_core::tool::{
    BashHandler, EditHandler, ReadHandler, RecordingToolHandler, ReplayToolHandler,
    SetStateHandler, SubagentHandler, ToolHandler, WebFetchHandler, WriteHandler,
};

use super::{RunFlags, ToolTapePair};

/// Construct the built-in tool vector — `BashHandler`, `ReadHandler`,
/// `WriteHandler`, `EditHandler`, `WebFetchHandler`, `SetStateHandler`
/// — and append the per-MCP-server tool handlers already produced by
/// `spawn_mcp_servers`. `LoopAgent` gates each call against the
/// per-agent `AgentPolicy.allowed_tools` list, so an agent that does
/// not opt into a handler cannot reach it: this vector is the
/// availability ceiling, not the default. `SetStateHandler` shares
/// the run-level redactor so `secrets.*` leaves are scrubbed before
/// state reaches the wire.
pub(super) fn assemble_builtin_tools(
    redactor: &Arc<Redactor>,
    body_excerpt_max_bytes: usize,
    mcp_tools: Vec<Arc<dyn ToolHandler>>,
) -> Vec<Arc<dyn ToolHandler>> {
    let mut tools: Vec<Arc<dyn ToolHandler>> = vec![
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
    tools.extend(mcp_tools);
    tools
}

/// Register the `shell` and `agent` node executors against a fresh
/// `NodeRegistry`, build the template engine, and wire everything into
/// the final [`Engine`]. Consumes `engine_config` so the caller cannot
/// accidentally read tunables that have already been threaded into the
/// engine.
pub(super) fn build_engine(
    sink: Arc<dyn EventSink>,
    loop_agent: Arc<LoopAgent>,
    engine_config: EngineConfig,
) -> Engine {
    let mut registry = NodeRegistry::new();
    registry.register(
        "shell",
        Arc::new(ShellExecutor::with_config(
            sink.clone(),
            engine_config.max_node_output_bytes,
        )),
    );
    let agent: Arc<dyn Agent> = loop_agent;
    registry.register("agent", Arc::new(AgentExecutor::from_agent(agent)));
    let registry = Arc::new(registry);
    let templates = Arc::new(TemplateEngine::new());
    Engine::new(sink, registry, templates, engine_config)
}

/// Wrap every handler in `tools` for record/replay when the relevant
/// tape flag is set. Returns the (possibly wrapped) handler vector and
/// the shared `BufWriter` handle that the run path flushes after the
/// engine returns. `--record-tool-tape` and `--replay-tool-tape` are
/// mutually exclusive at the CLI layer.
pub(super) fn wrap_tool_tape(
    tools: Vec<Arc<dyn ToolHandler>>,
    flags: &RunFlags,
    redactor: &Arc<Redactor>,
) -> Result<ToolTapePair> {
    if let Some(path) = &flags.record_tool_tape {
        let mut opts = std::fs::OpenOptions::new();
        // O_EXCL: prevent a pre-planted symlink at the path from redirecting
        // writes to an attacker-controlled file, and force the caller to make
        // an explicit decision when a stale tape already exists at the path.
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Tool tapes capture full request/response payloads including
            // secrets — must not be readable by other local users.
            opts.mode(0o600);
        }
        let file = opts
            .open(path)
            .with_context(|| format!("creating tool tape `{}`", path.display()))?;
        let shared = Arc::new(Mutex::new(std::io::BufWriter::new(file)));
        let path_buf = path.clone();
        let wrapped: Vec<Arc<dyn ToolHandler>> = tools
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
        Ok((wrapped, Some(shared)))
    } else if let Some(path) = &flags.replay_tool_tape {
        let mut replay_tools = Vec::with_capacity(tools.len());
        for h in tools {
            let name = h.name().to_string();
            let replay = ReplayToolHandler::load(h, path)
                .with_context(|| format!("loading tool tape for handler `{name}`"))?;
            replay_tools.push(Arc::new(replay) as Arc<dyn ToolHandler>);
        }
        Ok((replay_tools, None))
    } else {
        Ok((tools, None))
    }
}

/// Inputs for [`build_loop_agent`]. Grouped to keep the call site
/// under the four-parameter threshold (CLAUDE.md convention).
pub(super) struct BuildLoopAgentArgs<'a> {
    pub transport: Arc<dyn LlmTransport>,
    pub builtin_tools: Vec<Arc<dyn ToolHandler>>,
    pub pipeline: &'a Pipeline,
    pub sink: Arc<dyn EventSink>,
    pub redactor: Arc<Redactor>,
    pub body_excerpt_max_bytes: usize,
}

/// Build the `LoopAgent` inside `Arc::new_cyclic` so each
/// `SubagentHandler` can hold a `Weak<LoopAgent>` back-pointer into
/// the same agent its tool vector lives on. A plain `Arc` would
/// complete a cycle (`LoopAgent` → tools → `SubagentHandler` →
/// `LoopAgent`) and leak the agent forever; the `Weak` form breaks
/// the cycle while keeping dispatch O(1) on the hot path.
///
/// One handler per entry in `pipeline.agents`: the YAML form
/// `subagent.<name>` is the same string the parent's `allowed_tools`
/// references, so registration key = handler name = allowlist entry.
pub(super) fn build_loop_agent(args: BuildLoopAgentArgs<'_>) -> Arc<LoopAgent> {
    let BuildLoopAgentArgs {
        transport,
        builtin_tools,
        pipeline,
        sink,
        redactor,
        body_excerpt_max_bytes,
    } = args;
    Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
        let mut tools = builtin_tools.clone();
        for (name, cfg) in &pipeline.agents {
            tools.push(Arc::new(SubagentHandler::new(
                format!("subagent.{name}"),
                name.clone(),
                cfg.clone(),
                weak.clone(),
                sink.clone(),
            )));
        }
        LoopAgent::new(LoopAgentConfig {
            transport,
            sink,
            redactor,
            body_excerpt_max_bytes,
            tools,
        })
    })
}
