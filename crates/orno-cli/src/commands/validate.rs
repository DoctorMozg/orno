use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use orno_core::pipeline;
use orno_core::pipeline::schema::{AgentConfig, McpServerConfig, NodeKind, Pipeline};

const KNOWN_BUILTINS: &[&str] = &["Bash", "Read", "Edit", "Write", "WebFetch", "SetState"];

/// Builtins that perform writes outside the agent's own context (filesystem
/// edits or shell side effects). `Bash` is included because its declared
/// effect is `MutationsAndNetwork`.
fn builtin_requires_mutations(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "Bash")
}

/// Builtins that talk to the network. `Bash` is included for the same
/// reason as above.
fn builtin_requires_network(name: &str) -> bool {
    matches!(name, "WebFetch" | "Bash")
}

/// Builtins that write into the agent's own scoped state (`ToolEffect::ContextSelf`,
/// gated by `AgentPolicy.allow_context_writes`).
fn builtin_requires_context_writes(name: &str) -> bool {
    name == "SetState"
}

pub fn run(path: &Path) -> Result<()> {
    let pipeline = pipeline::load::load_from_path(path)
        .with_context(|| format!("loading pipeline `{}`", path.display()))?;

    let (errors, warnings) = validate_pipeline(&pipeline);

    for w in &warnings {
        eprintln!("warning: {w}");
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

fn validate_pipeline(pipeline: &Pipeline) -> (Vec<String>, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

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
        validate_agent(agent_name, cfg, pipeline, &mut errors, &mut warnings);
    }

    validate_mcp_server_names(&pipeline.mcp_servers, &mut errors);

    (errors, warnings)
}

fn validate_agent(
    agent_name: &str,
    cfg: &AgentConfig,
    pipeline: &Pipeline,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
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
            errors,
        );
    }

    // Cross-check builtin tool effects against the agent's policy.
    // Silent runtime denials become explicit validation failures —
    // matches the strictness contract's "everything is bounded and visible" intent.
    for tool in &cfg.allowed_tools {
        if builtin_requires_mutations(tool) && !cfg.policy.allow_mutations {
            errors.push(format!(
                "agent `{agent_name}` allowed_tools: `{tool}` requires \
                 mutations but allow_mutations is false"
            ));
        }
        if builtin_requires_network(tool) && !cfg.policy.allow_network {
            errors.push(format!(
                "agent `{agent_name}` allowed_tools: `{tool}` requires \
                 network but allow_network is false"
            ));
        }
        if builtin_requires_context_writes(tool) && !cfg.policy.allow_context_writes {
            errors.push(format!(
                "agent `{agent_name}` allowed_tools: `{tool}` requires \
                 context writes but allow_context_writes is false"
            ));
        }
    }

    // Domain lists with `allow_network=false` are dormant — they only
    // become live if a later flag flip enables network. Surface as a
    // warning so the dormant config is intentional, not silent.
    if !cfg.policy.allow_network
        && (!cfg.policy.allowed_domains.is_empty() || !cfg.policy.blocked_domains.is_empty())
    {
        warnings.push(format!(
            "agent `{agent_name}`: allowed_domains/blocked_domains is set \
             but allow_network=false — the domain list will never apply"
        ));
    }
}

/// Reject MCP server names where one is an underscore-prefixed extension of
/// another. At replay time, `mcp.<server>.<tool>` names are flattened by
/// replacing dots with underscores (see `replay::yaml_to_wire_name`); a pair
/// like `foo` and `foo_bar` produces wire names that cannot be unambiguously
/// routed back to a single server.
fn validate_mcp_server_names(
    mcp_servers: &BTreeMap<String, McpServerConfig>,
    errors: &mut Vec<String>,
) {
    let server_names: Vec<&str> = mcp_servers.keys().map(String::as_str).collect();
    for (i, a) in server_names.iter().enumerate() {
        for b in &server_names[i + 1..] {
            if a.starts_with(&format!("{b}_")) || b.starts_with(&format!("{a}_")) {
                errors.push(format!(
                    "MCP server names `{a}` and `{b}` share an \
                     underscore-prefix relationship that produces \
                     ambiguous wire-form tool names at replay time. \
                     Rename one server."
                ));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use orno_core::pipeline::schema::{AgentPolicy, McpStdioConfig, OnParseError};

    fn empty_pipeline() -> Pipeline {
        Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            nodes: Vec::new(),
        }
    }

    fn base_policy() -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1000,
            max_tool_calls: 5,
            max_subagent_depth: 0,
            max_tool_output_bytes: None,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
            roots: Vec::new(),
            max_message_history_bytes: None,
        }
    }

    fn agent_with(allowed_tools: &[&str], policy: AgentPolicy) -> AgentConfig {
        AgentConfig {
            model: "test-model".into(),
            provider: "test-provider".into(),
            system: None,
            allowed_tools: allowed_tools.iter().map(|s| (*s).to_string()).collect(),
            policy,
        }
    }

    fn stdio_mcp_server() -> McpServerConfig {
        McpServerConfig::Stdio(McpStdioConfig {
            command: vec!["echo".to_string()],
            env: BTreeMap::new(),
        })
    }

    #[test]
    fn webfetch_without_network_is_an_error() {
        let mut p = empty_pipeline();
        p.agents
            .insert("a".into(), agent_with(&["WebFetch"], base_policy()));
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("WebFetch") && e.contains("allow_network is false")),
            "expected network-gate error, got: {errors:?}"
        );
    }

    #[test]
    fn edit_without_mutations_is_an_error() {
        let mut p = empty_pipeline();
        p.agents
            .insert("a".into(), agent_with(&["Edit"], base_policy()));
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Edit") && e.contains("allow_mutations is false")),
            "expected mutations-gate error, got: {errors:?}"
        );
    }

    #[test]
    fn bash_without_mutations_fires_mutations_gate() {
        let mut p = empty_pipeline();
        let mut policy = base_policy();
        // Network on so only the mutations gate trips for this assertion.
        policy.allow_network = true;
        p.agents.insert("a".into(), agent_with(&["Bash"], policy));
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Bash") && e.contains("allow_mutations is false")),
            "expected Bash to trip mutations gate, got: {errors:?}"
        );
    }

    #[test]
    fn bash_without_network_fires_network_gate() {
        let mut p = empty_pipeline();
        let mut policy = base_policy();
        // Mutations on so only the network gate trips for this assertion.
        policy.allow_mutations = true;
        p.agents.insert("a".into(), agent_with(&["Bash"], policy));
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Bash") && e.contains("allow_network is false")),
            "expected Bash to trip network gate, got: {errors:?}"
        );
    }

    #[test]
    fn setstate_without_context_writes_is_an_error() {
        let mut p = empty_pipeline();
        p.agents
            .insert("a".into(), agent_with(&["SetState"], base_policy()));
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("SetState") && e.contains("allow_context_writes is false")),
            "expected context-writes-gate error, got: {errors:?}"
        );
    }

    #[test]
    fn dormant_allowed_domains_is_a_warning_not_an_error() {
        let mut p = empty_pipeline();
        let mut policy = base_policy();
        policy.allowed_domains = vec!["x.com".to_string()];
        p.agents.insert("a".into(), agent_with(&[], policy));
        let (errors, warnings) = validate_pipeline(&p);
        assert!(
            errors.is_empty(),
            "dormant domain config must not be an error, got: {errors:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("allow_network=false")),
            "expected dormant-domain warning, got: {warnings:?}"
        );
    }

    #[test]
    fn mcp_servers_with_underscore_prefix_overlap_are_rejected() {
        let mut p = empty_pipeline();
        p.mcp_servers.insert("foo".into(), stdio_mcp_server());
        p.mcp_servers.insert("foo_bar".into(), stdio_mcp_server());
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("foo") && e.contains("foo_bar") && e.contains("Rename")),
            "expected MCP prefix-collision error, got: {errors:?}"
        );
    }

    #[test]
    fn mcp_servers_with_disjoint_names_are_accepted() {
        let mut p = empty_pipeline();
        p.mcp_servers.insert("foo".into(), stdio_mcp_server());
        p.mcp_servers.insert("bar".into(), stdio_mcp_server());
        let (errors, _) = validate_pipeline(&p);
        assert!(
            errors.is_empty(),
            "disjoint MCP server names should validate cleanly, got: {errors:?}"
        );
    }
}
