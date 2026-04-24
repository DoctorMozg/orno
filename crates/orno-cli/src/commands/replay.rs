//! `orno replay <bundle.ndjson>` — re-run a recorded pipeline from a
//! single bundle file. No live LLM, no MCP servers, no env/secrets
//! resolution: every external interaction was already captured into
//! the bundle by `orno run --record-bundle`.
//!
//! Replay reuses the same engine, agent loop, and node executors as
//! `orno run`. The only differences are:
//! - Transport is `ReplayTransport`, seeded from the bundle's LLM tape.
//! - Each builtin tool handler is wrapped in `ReplayToolHandler`,
//!   seeded from the bundle's tool tape.
//! - MCP servers are not spawned. For every `mcp.*` tool the pipeline
//!   declares in any agent's `allowed_tools`, a `ReplayMcpStubHandler`
//!   is registered with the recorded `OrnoChatTool` description and
//!   schema (lifted from the LLM tape). The stub is wrapped in
//!   `ReplayToolHandler` so actual dispatch reads from the tool tape
//!   exactly like a builtin would.
//! - `RunInputs` is empty: rendered prompts and resolved env values
//!   are already baked into the recorded LLM requests.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Weak};

use anyhow::{Context, Result};
use orno_core::agent::{Agent, LoopAgent, LoopAgentConfig};
use orno_core::events::{EventSink, Redactor, StreamingSink};
use orno_core::execution::{Engine, EngineConfig, RunInputs, new_run_id};
use orno_core::llm::recording::TapeEntry;
use orno_core::llm::{LlmTransport, OrnoChatTool, ReplayTransport, read_bundle};
use orno_core::node::NodeRegistry;
use orno_core::node::agent::AgentExecutor;
use orno_core::node::shell::ShellExecutor;
use orno_core::pipeline;
use orno_core::pipeline::template::TemplateEngine;
use orno_core::tool::{
    BashHandler, EditHandler, ReadHandler, ReplayMcpStubHandler, ReplayToolHandler,
    SetStateHandler, SubagentHandler, ToolHandler, WebFetchHandler, WriteHandler,
};

/// Default body-excerpt cap shared with `orno run`'s non-verbose mode.
/// Replay does not surface new failure tails (every payload was
/// recorded), but the agent loop still uses this constant for prompt
/// and response excerpt truncation so emitted events match the
/// original recording's truncation policy.
const REPLAY_BODY_EXCERPT_BYTES: usize = 2048;

#[expect(
    clippy::too_many_lines,
    reason = "replay() mirrors run()'s top-level orchestration; splitting it would add indirection without reducing conceptual load"
)]
pub async fn run(bundle_path: &Path) -> Result<()> {
    let bundle = read_bundle(bundle_path)
        .with_context(|| format!("reading bundle `{}`", bundle_path.display()))?;

    let mut pipeline = pipeline::load::load_from_str(&bundle.pipeline_yaml)
        .with_context(|| "parsing pipeline from bundle")?;

    // Lift MCP tool definitions from the recorded LLM tape before
    // moving the entries into the transport — the stub registration
    // block below needs the description and schema each declared
    // `mcp.*` tool was registered with so the replayed
    // `LlmRequest.tools` byte-equals the recorded one.
    let mcp_tool_defs = discover_mcp_tool_definitions(&bundle.llm_entries);

    // Expand `mcp.<server>.*` wildcards using the per-server tool lists
    // recovered from the recorded tape. Without this, an agent that
    // declared a wildcard at recording time would fail pre-flight on
    // replay with `UnknownToolCalled` because the post-load `allowed_tools`
    // would still contain the literal `mcp.<server>.*` string.
    let server_tool_names = derive_server_tools(&mcp_tool_defs, &pipeline);
    pipeline::load::expand_mcp_wildcards(&mut pipeline, &server_tool_names)
        .context("expanding MCP tool wildcards from recorded tape")?;

    // Replay transport is seeded directly from the bundle entries; a
    // tape miss surfaces as `LlmError::ReplayMiss` at call time
    // (ADR 0005 §5).
    let transport: Arc<dyn LlmTransport> =
        Arc::new(ReplayTransport::from_entries(bundle.llm_entries));

    // No live secrets in replay — `secrets.*` values are not resolved
    // at replay time, so the redactor is a no-op. Constructing one
    // anyway keeps `LoopAgent`'s constructor signature unchanged and
    // costs nothing at runtime (`Redactor::is_noop()` short-circuits).
    let redactor = Arc::new(Redactor::default());
    let body_excerpt_max_bytes = REPLAY_BODY_EXCERPT_BYTES;

    let sink: Arc<dyn EventSink> = Arc::new(StreamingSink::stdout());
    let run_id = new_run_id();

    let tool_entries = bundle.tool_entries;

    // Each builtin handler gets its own `ReplayToolHandler` wrapping
    // the same tape entries. The replay handler keys on
    // `(tool_name, call_id, args)` so siblings sharing the entry list
    // never collide — a `Bash` call and a `Read` call with the same
    // call_id index different tape rows.
    let mut builtin_tools: Vec<Arc<dyn ToolHandler>> = vec![
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(BashHandler),
            tool_entries.clone(),
        )),
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(ReadHandler),
            tool_entries.clone(),
        )),
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(WriteHandler),
            tool_entries.clone(),
        )),
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(EditHandler),
            tool_entries.clone(),
        )),
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(WebFetchHandler),
            tool_entries.clone(),
        )),
        Arc::new(ReplayToolHandler::from_entries(
            Arc::new(SetStateHandler::new(
                redactor.clone(),
                body_excerpt_max_bytes,
            )),
            tool_entries.clone(),
        )),
    ];

    // MCP replay: no live `McpClient` is spawned, but the agent loop
    // still needs a registered handler for every `mcp.*` tool the
    // pipeline declares — otherwise the pre-flight check fails with
    // `UnknownToolCalled`. We synthesize one `ReplayMcpStubHandler`
    // per declared tool, lifting its description and schema from the
    // recorded LLM tape (already collected above) so the outgoing
    // `LlmRequest.tools` byte-equals what was recorded (a mismatch
    // would change the tape key and surface as `LlmError::ReplayMiss`).
    // Tools that appear in `allowed_tools` but never in any recorded
    // request get an empty stub: pre-flight passes, and any actual
    // call would surface a clear miss.
    let mut mcp_seen: HashSet<String> = HashSet::new();
    for cfg in pipeline.agents.values() {
        for tool in &cfg.allowed_tools {
            if !tool.starts_with("mcp.") || !mcp_seen.insert(tool.clone()) {
                continue;
            }
            let wire = yaml_to_wire_name(tool);
            let (description, schema) = mcp_tool_defs
                .iter()
                .find(|def| def.name == wire)
                .map_or_else(
                    || (String::new(), serde_json::json!({})),
                    |def| (def.description.clone(), def.schema.clone()),
                );
            builtin_tools.push(Arc::new(ReplayToolHandler::from_entries(
                Arc::new(ReplayMcpStubHandler::new(tool.clone(), description, schema)),
                tool_entries.clone(),
            )));
        }
    }

    // ADR 0006 cyclic construction: each `SubagentHandler` keeps a
    // `Weak<LoopAgent>` back-pointer. Same pattern as `orno run` so
    // recorded subagent dispatches replay through the same code path
    // they were captured on.
    let event_sink = sink.clone();
    let transport_for_loop = transport.clone();
    let redactor_for_loop = redactor.clone();
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
            transport: transport_for_loop,
            sink: event_sink.clone(),
            redactor: redactor_for_loop,
            body_excerpt_max_bytes,
            tools,
        })
    });

    let agent: Arc<dyn Agent> = loop_agent;
    let mut registry = NodeRegistry::new();
    registry.register("shell", Arc::new(ShellExecutor));
    registry.register("agent", Arc::new(AgentExecutor::from_agent(agent)));
    let registry = Arc::new(registry);
    let templates = Arc::new(TemplateEngine::new());
    let engine = Engine::new(sink, registry, templates, EngineConfig::default());

    // RunInputs is empty: rendered prompts already include the env /
    // secrets values they were captured with, so re-resolving here
    // would only let drift sneak in. Templates referencing `env.*` or
    // `secrets.*` re-render against an empty namespace at replay; for
    // pipelines with first-turn templates that read env, the original
    // rendered values still flow through the recorded `LlmRequest`,
    // not through a re-render against the live process.
    engine.run(&run_id, &pipeline, RunInputs::default()).await?;
    Ok(())
}

/// Translate a YAML-form tool name into the wire form `LoopAgent`
/// uses when building `OrnoChatTool` definitions for the LLM.
/// Mirrors `LoopAgent::to_wire_name` (private to that module) — this
/// duplicate keeps replay independent of agent-internal helpers and
/// is small enough that the cost is negligible.
fn yaml_to_wire_name(yaml_name: &str) -> String {
    if yaml_name.contains('.') {
        yaml_name.replace('.', "_")
    } else {
        yaml_name.to_string()
    }
}

/// Walk every recorded LLM request's `tools` list and collect the
/// `OrnoChatTool` definition for each MCP tool the agent loop sent
/// to the model. Returned in first-occurrence order so callers can
/// reconstruct the recorded tool order verbatim.
///
/// First-seen wins: every recorded iteration of an agent sends the
/// same `tools` list, so duplicates are expected and harmless.
///
/// Order matters here because `tape_key = blake3(json(LlmRequest))`
/// and `LlmRequest.tools` is a `Vec` — its position is part of the
/// hashed bytes. A `HashMap` return type previously scrambled tool
/// order across program runs (`HashMap` key iteration is randomized),
/// causing every replay to miss with `LlmError::ReplayMiss`. The
/// `Vec` return preserves the order each tool first appeared in
/// the tape, which is the order the agent loop sent them on the
/// recorded run.
fn discover_mcp_tool_definitions(llm_entries: &[TapeEntry]) -> Vec<OrnoChatTool> {
    let mut out: Vec<OrnoChatTool> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in llm_entries {
        for tool in &entry.req.tools {
            if tool.name.starts_with("mcp_") && seen.insert(tool.name.clone()) {
                out.push(tool.clone());
            }
        }
    }
    out
}

/// Recover the per-server tool list from recorded wire-form tool names
/// so wildcard expansion (`mcp.<server>.*`) can be replayed without a
/// live MCP server.
///
/// The wire form is `mcp_<server>_<tool>`; the YAML form is
/// `mcp.<server>.<tool>`. To reverse the mapping we need the server
/// boundary, which the wire form alone does not encode unambiguously
/// — both `(server="fs", tool="extra_read")` and `(server="fs_extra",
/// tool="read")` collapse to `mcp_fs_extra_read`. We resolve by taking
/// the longest server prefix declared in `pipeline.mcp_servers` that
/// matches; in v0.1 every demo uses single-token server names so the
/// ambiguity never bites in practice. Tools whose wire name does not
/// begin with `mcp_<S>_` for any declared server `S` are silently
/// skipped — they cannot have come from a server orno knows about.
fn derive_server_tools(
    mcp_tool_defs: &[OrnoChatTool],
    pipeline: &pipeline::Pipeline,
) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = pipeline
        .mcp_servers
        .keys()
        .map(|s| (s.clone(), Vec::new()))
        .collect();

    for def in mcp_tool_defs {
        let wire_name = &def.name;
        let Some(rest) = wire_name.strip_prefix("mcp_") else {
            continue;
        };
        // Longest-prefix match against the declared server names.
        let mut best: Option<(&str, &str)> = None;
        for server in pipeline.mcp_servers.keys() {
            let prefix = format!("{server}_");
            let Some(tool) = rest.strip_prefix(prefix.as_str()) else {
                continue;
            };
            if best.is_none_or(|(s, _)| server.len() > s.len()) {
                best = Some((server.as_str(), tool));
            }
        }
        if let Some((server, tool)) = best {
            out.entry(server.to_string())
                .or_default()
                .push(tool.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orno_core::llm::{LlmRequest, LlmResponse, OrnoChatTool, recording::TapeEntry};

    fn tape_entry_with_tools(tools: Vec<OrnoChatTool>) -> TapeEntry {
        TapeEntry {
            req: LlmRequest {
                provider: "openai".into(),
                model: "gpt-5".into(),
                prompt: "x".into(),
                system: None,
                temperature: None,
                max_tokens: None,
                messages: vec![],
                tools,
            },
            res: LlmResponse {
                content: "ok".into(),
                finish_reason: Some("stop".into()),
                usage: None,
                tool_calls: vec![],
            },
        }
    }

    fn ornochat_tool(name: &str, desc: &str) -> OrnoChatTool {
        OrnoChatTool {
            name: name.into(),
            description: desc.into(),
            schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn yaml_to_wire_name_replaces_dots_with_underscores() {
        assert_eq!(
            yaml_to_wire_name("mcp.filesystem.read_file"),
            "mcp_filesystem_read_file"
        );
    }

    #[test]
    fn yaml_to_wire_name_passes_through_dotless_names() {
        assert_eq!(yaml_to_wire_name("Bash"), "Bash");
    }

    #[test]
    fn discover_mcp_tool_definitions_keeps_only_mcp_tools() {
        let entries = vec![tape_entry_with_tools(vec![
            ornochat_tool("Bash", "Run a shell command"),
            ornochat_tool("mcp_fs_read_file", "Read a file"),
            ornochat_tool("mcp_fs_list_directory", "List a directory"),
        ])];
        let defs = discover_mcp_tool_definitions(&entries);
        assert_eq!(defs.len(), 2, "non-MCP tools must be filtered out");
        assert_eq!(
            defs.iter()
                .find(|t| t.name == "mcp_fs_read_file")
                .map(|t| t.description.as_str()),
            Some("Read a file"),
        );
        assert_eq!(
            defs.iter()
                .find(|t| t.name == "mcp_fs_list_directory")
                .map(|t| t.description.as_str()),
            Some("List a directory"),
        );
        assert!(defs.iter().all(|t| t.name != "Bash"));
    }

    #[test]
    fn discover_mcp_tool_definitions_preserves_first_request_order() {
        // Regression: `tape_key = blake3(json(LlmRequest))` and
        // `LlmRequest.tools` is a position-sensitive `Vec`. If
        // discovery reorders tools, the reconstructed request hashes
        // differently than the recorded one and every LLM call
        // replays as `ReplayMiss`. The previous HashMap-based
        // implementation surfaced exactly this on the
        // mcp-http-demo bundle.
        let entries = vec![tape_entry_with_tools(vec![
            ornochat_tool("mcp_fs_zeta", "z"),
            ornochat_tool("mcp_fs_alpha", "a"),
            ornochat_tool("mcp_fs_mid", "m"),
        ])];
        let defs = discover_mcp_tool_definitions(&entries);
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["mcp_fs_zeta", "mcp_fs_alpha", "mcp_fs_mid"]);
    }

    #[test]
    fn discover_mcp_tool_definitions_dedupes_across_iterations() {
        // Multiple iterations of the same agent send the same tools
        // list. We must keep one definition per wire name, not append
        // duplicates that would inflate the registered handler set.
        let tool = ornochat_tool("mcp_fs_read_file", "v1");
        let entries = vec![
            tape_entry_with_tools(vec![tool.clone()]),
            tape_entry_with_tools(vec![tool.clone()]),
            tape_entry_with_tools(vec![tool]),
        ];
        let defs = discover_mcp_tool_definitions(&entries);
        assert_eq!(defs.len(), 1);
    }

    #[test]
    fn discover_mcp_tool_definitions_first_seen_wins() {
        // If two recorded entries somehow disagree on the schema for
        // the same wire name (mid-run schema change?), we lock in the
        // first one — that matches the original recording's first
        // request, which is what the replay key was hashed against.
        let entries = vec![
            tape_entry_with_tools(vec![ornochat_tool("mcp_fs_read_file", "first")]),
            tape_entry_with_tools(vec![ornochat_tool("mcp_fs_read_file", "second")]),
        ];
        let defs = discover_mcp_tool_definitions(&entries);
        assert_eq!(
            defs.iter()
                .find(|t| t.name == "mcp_fs_read_file")
                .map(|t| t.description.as_str()),
            Some("first"),
        );
    }

    #[test]
    fn discover_mcp_tool_definitions_handles_empty_tape() {
        let defs = discover_mcp_tool_definitions(&[]);
        assert!(defs.is_empty());
    }
}
