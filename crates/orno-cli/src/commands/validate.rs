use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use orno_core::pipeline;
use orno_core::pipeline::schema::{AgentConfig, McpServerConfig, NodeKind};

const KNOWN_BUILTINS: &[&str] = &["Bash", "Read", "Edit", "Write", "WebFetch", "SetState"];

pub fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    let mut errors: Vec<String> = Vec::new();

    for node in &pipeline.nodes {
        if let NodeKind::Agent(agent_node) = &node.kind
            && !pipeline.agents.contains_key(&agent_node.agent)
        {
            errors.push(format!(
                "node `{}` references undeclared agent `{}`",
                node.id, agent_node.agent
            ));
        }
    }

    for (agent_name, cfg) in &pipeline.agents {
        if cfg.policy.max_iterations == 0 {
            errors.push(format!("agent `{agent_name}`: max_iterations must be > 0"));
        }
        if cfg.policy.max_total_tokens == 0 {
            errors.push(format!(
                "agent `{agent_name}`: max_total_tokens must be > 0"
            ));
        }
        if cfg.policy.max_tool_calls == 0 && !cfg.allowed_tools.is_empty() {
            errors.push(format!(
                "agent `{agent_name}`: max_tool_calls is 0 but agent has allowed_tools"
            ));
        }

        for tool in &cfg.allowed_tools {
            validate_tool_name(
                tool,
                agent_name,
                &pipeline.agents,
                &pipeline.mcp_servers,
                &mut errors,
            );
        }
    }

    if errors.is_empty() {
        println!(
            "ok: version={} nodes={} agents={} mcp_servers={}",
            pipeline.version,
            pipeline.nodes.len(),
            pipeline.agents.len(),
            pipeline.mcp_servers.len(),
        );
        Ok(())
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        anyhow::bail!("{} validation error(s)", errors.len())
    }
}

fn validate_tool_name(
    tool: &str,
    agent_name: &str,
    agents: &BTreeMap<String, AgentConfig>,
    mcp_servers: &BTreeMap<String, McpServerConfig>,
    errors: &mut Vec<String>,
) {
    if KNOWN_BUILTINS.contains(&tool) {
        return;
    }
    if let Some(subagent_name) = tool.strip_prefix("subagent.") {
        if !agents.contains_key(subagent_name) {
            errors.push(format!(
                "agent `{agent_name}` allowed_tools: `{tool}` references undeclared agent `{subagent_name}`"
            ));
        }
        return;
    }
    if let Some(rest) = tool.strip_prefix("mcp.") {
        if let Some(dot_pos) = rest.find('.') {
            let server = &rest[..dot_pos];
            if !mcp_servers.contains_key(server) {
                errors.push(format!(
                    "agent `{agent_name}` allowed_tools: `{tool}` references undeclared MCP server `{server}`"
                ));
            }
        } else {
            errors.push(format!(
                "agent `{agent_name}` allowed_tools: `{tool}` is not a valid mcp tool reference (expected `mcp.<server>.<tool>` or `mcp.<server>.*`)"
            ));
        }
        return;
    }
    errors.push(format!(
        "agent `{agent_name}` allowed_tools: `{tool}` is not a known builtin, subagent reference, or mcp tool"
    ));
}
