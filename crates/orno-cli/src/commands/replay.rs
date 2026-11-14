//! `orno replay <bundle.ndjson>`: re-run a recorded pipeline from one
//! bundle file with no live LLM, MCP servers or env/secrets resolution.
//!
//! It reuses `orno run`'s engine, agent loop and node executors; the
//! differences are:
//! - the transport is `ReplayTransport`, seeded from the bundle's LLM tape;
//! - every tool handler is wrapped in `ReplayToolHandler`, seeded from the
//!   bundle's tool tape;
//! - each `mcp.*` tool an agent allows gets a stub carrying the recorded
//!   description and schema, so the replayed request matches the tape key;
//! - `RunInputs` comes from the bundle, with every secret value masked.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use orno_core::agent::loop_agent::wire_name_from_yaml;
use orno_core::events::{EventSink, Redactor, StreamingSink};
use orno_core::execution::{EngineConfig, RunInputs, new_run_id};
use orno_core::llm::recording::TapeEntry;
use orno_core::llm::{LlmTransport, OrnoChatTool, ReplayTransport, read_bundle};
use orno_core::node::shell::DenyShellExecutor;
use orno_core::pipeline;
use orno_core::tool::{ReplayMcpStubHandler, ReplayToolHandler, ToolHandler, builtin_handlers};

use super::run::agent::{BuildLoopAgentArgs, build_engine, build_loop_agent};

/// Same excerpt cap as `orno run`'s non-verbose mode, so replayed events
/// truncate prompts and responses the way the recording did.
const REPLAY_BODY_EXCERPT_BYTES: usize = 2048;

pub async fn run(bundle_path: &Path) -> Result<()> {
    let bundle = read_bundle(bundle_path)
        .with_context(|| format!("reading bundle `{}`", bundle_path.display()))?;

    let mut pipeline = pipeline::load::load_from_str(&bundle.pipeline_yaml)
        .with_context(|| "parsing pipeline from bundle")?;

    // Lifted before the entries move into the transport: the stubs need
    // each tool's recorded description and schema.
    let mcp_tool_defs = discover_mcp_tool_definitions(&bundle.llm_entries);

    // Without this, a wildcard recorded in `allowed_tools` stays literal
    // and fails pre-flight with `UnknownToolCalled`.
    let server_tool_names = derive_server_tools(&mcp_tool_defs, &pipeline);
    pipeline::load::expand_mcp_wildcards(&mut pipeline, &server_tool_names);
    disable_live_effects(&mut pipeline);

    // A tape miss surfaces as `LlmError::ReplayMiss`; there is no fallback
    // to the live API.
    let transport: Arc<dyn LlmTransport> = Arc::new(
        ReplayTransport::from_entries(bundle.llm_entries)
            .context("indexing LLM entries from bundle")?,
    );

    // `secrets.*` is never resolved on replay, so the redactor is a no-op.
    let redactor = Arc::new(Redactor::default());
    let tools = replay_tools(&pipeline, &mcp_tool_defs, &bundle.tool_entries, &redactor)?;

    // Typed handle so the broken flag can be read after the engine returns.
    let streaming_sink = Arc::new(StreamingSink::stdout());
    let sink: Arc<dyn EventSink> = streaming_sink.clone();
    let run_id = new_run_id();

    let loop_agent = build_loop_agent(BuildLoopAgentArgs {
        transport,
        builtin_tools: tools,
        pipeline: &pipeline,
        sink: sink.clone(),
        redactor,
        body_excerpt_max_bytes: REPLAY_BODY_EXCERPT_BYTES,
    });
    // Shell side effects are not in the bundle, so shell nodes are refused
    // rather than run live.
    let engine = build_engine(
        sink.clone(),
        loop_agent,
        Arc::new(DenyShellExecutor),
        EngineConfig::default(),
    );

    // Recorded namespaces stand in for the live env and secrets: vars come
    // back rendered, and secret values are the placeholder the recorded
    // requests were keyed with.
    let mut inputs = RunInputs::default();
    inputs.env = bundle.run_inputs.env;
    inputs.secrets = bundle.run_inputs.secrets;
    inputs.rendered_vars = Some(bundle.run_inputs.vars);

    // Nothing to flush or drain on replay, so SIGINT only closes the stream
    // and `main` maps `Interrupted` to exit 130.
    let outcome = tokio::select! {
        biased;
        () = super::sigint_with_warning() => {
            super::emit_interrupted(&sink, &run_id).await;
            return Err(super::Interrupted.into());
        }
        res = engine.run(&run_id, &pipeline, inputs) => res?,
    };

    super::finalize_run(&streaming_sink, &outcome, "replay")
}

/// A bundle must not mutate the host or reach the network whatever its
/// embedded YAML claims, so both flags are forced off on every agent.
fn disable_live_effects(pipeline: &mut pipeline::Pipeline) {
    let had_mutations = pipeline.agents.values().any(|a| a.policy.allow_mutations);
    let had_network = pipeline.agents.values().any(|a| a.policy.allow_network);
    for agent_cfg in pipeline.agents.values_mut() {
        agent_cfg.policy.allow_mutations = false;
        agent_cfg.policy.allow_network = false;
    }
    if had_mutations || had_network {
        tracing::info!(
            had_mutations,
            had_network,
            "replay: allow_mutations and allow_network overridden to false"
        );
    }
}

/// Builtin handlers plus one stub per declared `mcp.*` tool, each wrapped
/// in a `ReplayToolHandler` keyed on `(tool_name, call_id, args)` so
/// handlers sharing the tape never collide.
fn replay_tools(
    pipeline: &pipeline::Pipeline,
    mcp_tool_defs: &[OrnoChatTool],
    tool_entries: &[orno_core::tool::ToolTapeEntry],
    redactor: &Arc<Redactor>,
) -> Result<Vec<Arc<dyn ToolHandler>>> {
    let wrap = |handler: Arc<dyn ToolHandler>| -> Result<Arc<dyn ToolHandler>> {
        let replay = ReplayToolHandler::from_entries(handler, tool_entries.to_vec())
            .context("indexing tool entries from bundle")?;
        Ok(Arc::new(replay))
    };

    let mut tools = Vec::new();
    for handler in builtin_handlers(redactor, REPLAY_BODY_EXCERPT_BYTES) {
        tools.push(wrap(handler)?);
    }

    // An `allowed_tools` entry never seen in a recorded request gets an
    // empty stub: pre-flight passes and a real call reports a tape miss.
    let mut seen: HashSet<&str> = HashSet::new();
    for cfg in pipeline.agents.values() {
        for tool in &cfg.allowed_tools {
            if !tool.starts_with("mcp.") || !seen.insert(tool) {
                continue;
            }
            let wire = wire_name_from_yaml(tool);
            let (description, schema) = mcp_tool_defs
                .iter()
                .find(|def| def.name == wire)
                .map_or_else(
                    || (String::new(), serde_json::json!({})),
                    |def| (def.description.clone(), def.schema.clone()),
                );
            tools.push(wrap(Arc::new(ReplayMcpStubHandler::new(
                tool.clone(),
                description,
                schema,
            )))?);
        }
    }
    Ok(tools)
}

/// Collect the definition of each MCP tool the agent loop sent to the
/// model, in first-seen order. Order matters: the tape key hashes the
/// serialized `tools` array, so a reordered list makes every call miss.
fn discover_mcp_tool_definitions(llm_entries: &[TapeEntry]) -> Vec<OrnoChatTool> {
    let mut out: Vec<OrnoChatTool> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in llm_entries {
        for tool in &entry.tools {
            if tool.name.starts_with("mcp_") && seen.insert(tool.name.clone()) {
                out.push(tool.clone());
            }
        }
    }
    out
}

/// Recover the per-server tool list from recorded wire names so wildcards
/// can expand without a live server. `mcp_<server>_<tool>` is ambiguous
/// when server names contain underscores, so the longest declared server
/// prefix wins; names matching no declared server are warned about and
/// skipped.
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
        } else {
            let declared: Vec<&str> = pipeline.mcp_servers.keys().map(String::as_str).collect();
            tracing::warn!(
                tool.wire_name = %wire_name,
                mcp.declared_servers = ?declared,
                "MCP wire-form tool name does not match any declared \
                 server prefix; tool will not be replayable",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use orno_core::llm::{
        LlmResponse, OrnoChatTool,
        recording::{TapeEntry, TapeOutcome},
    };

    fn tape_entry_with_tools(tools: Vec<OrnoChatTool>) -> TapeEntry {
        TapeEntry {
            key: "openai:gpt-5:x".into(),
            seq: 0,
            tools,
            outcome: TapeOutcome::Ok {
                res: LlmResponse {
                    content: "ok".into(),
                    finish_reason: Some("stop".into()),
                    usage: None,
                    tool_calls: vec![],
                },
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

    #[test]
    fn derive_server_tools_warns_on_unmatched_wire_name() {
        use orno_core::pipeline::Pipeline;
        use orno_core::pipeline::schema::{McpServerConfig, McpStdioConfig};

        let mut servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
        // Declare server `fs` only; the tape carries a wire name from
        // server `gh` that does not match any declared prefix.
        servers.insert(
            "fs".to_string(),
            McpServerConfig::Stdio(McpStdioConfig {
                command: vec!["true".to_string()],
                env: BTreeMap::new(),
                call_timeout_secs: None,
            }),
        );
        let pipeline = Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            mcp_servers: servers,
            nodes: vec![],
        };
        let defs = vec![ornochat_tool("mcp_gh_pull_request", "x")];
        let out = derive_server_tools(&defs, &pipeline);
        // Test passes if the call did not panic and no spurious entry
        // was attributed to the declared `fs` server. The WARN emission
        // is observable via `cargo test -- --nocapture`.
        assert!(out.get("fs").is_none_or(Vec::is_empty));
    }
}
