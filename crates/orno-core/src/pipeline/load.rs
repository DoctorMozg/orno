//! YAML → validated `Pipeline`.

use std::path::Path;

use crate::error::PipelineError;

use super::schema::Pipeline;

/// Load a pipeline from an on-disk YAML file and validate it.
pub fn load_from_path(path: &Path) -> Result<Pipeline, PipelineError> {
    let bytes = std::fs::read(path).map_err(|source| PipelineError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let pipeline: Pipeline = serde_yaml_ng::from_slice(&bytes).map_err(PipelineError::Parse)?;
    validate(&pipeline)?;
    Ok(pipeline)
}

/// Validate semantic constraints that serde alone cannot enforce.
#[expect(
    clippy::too_many_lines,
    reason = "validation covers all schema constraints; acceptable until v0.1.0 node types stabilize"
)]
pub fn validate(pipeline: &Pipeline) -> Result<(), PipelineError> {
    if pipeline.nodes.is_empty() {
        return Err(PipelineError::Validation("pipeline has no nodes".into()));
    }

    let mut ids = std::collections::HashSet::new();
    for node in &pipeline.nodes {
        if !ids.insert(node.id.as_str()) {
            return Err(PipelineError::Validation(format!(
                "duplicate node id `{}`",
                node.id
            )));
        }
    }

    for node in &pipeline.nodes {
        for dep in &node.needs {
            if !ids.contains(dep.as_str()) {
                return Err(PipelineError::Validation(format!(
                    "node `{}` depends on unknown `{}`",
                    node.id, dep
                )));
            }
        }
    }

    // Cycle detection via Kahn's algorithm. `DagWalker::new` performs
    // the same check at execution time, but `orno validate` stops at
    // this function — without the inline pass, a cyclic pipeline would
    // pass validation and fail only at runtime. Keeping the load layer
    // decoupled from execution means inlining the algorithm here rather
    // than calling into `DagWalker`.
    let n = pipeline.nodes.len();
    let id_to_idx: std::collections::HashMap<&str, usize> = pipeline
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (idx, node) in pipeline.nodes.iter().enumerate() {
        for dep in &node.needs {
            // Safe indexing: the preceding loop already rejected any
            // `needs:` target that is not a declared id.
            let dep_idx = id_to_idx[dep.as_str()];
            in_degree[idx] += 1;
            dependents[dep_idx].push(idx);
        }
    }
    let mut queue: std::collections::VecDeque<usize> =
        (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut visited = 0usize;
    while let Some(idx) = queue.pop_front() {
        visited += 1;
        for &dep in &dependents[idx] {
            in_degree[dep] -= 1;
            if in_degree[dep] == 0 {
                queue.push_back(dep);
            }
        }
    }
    if visited < n {
        let cycle_node = pipeline
            .nodes
            .iter()
            .enumerate()
            .find(|(i, _)| in_degree[*i] > 0)
            .map_or("unknown", |(_, node)| node.id.as_str());
        return Err(PipelineError::Validation(format!(
            "cycle detected involving `{cycle_node}`"
        )));
    }

    // Validate per-agent tool allowlists.
    // – `mcp.<server>.*` and `mcp.<server>.<tool>` must name a server declared
    //   in `Pipeline.mcp_servers`.
    // – `subagent.<child>` must name an agent declared in `Pipeline.agents`;
    //   compose-down requires the child's effect policy to be no more
    //   permissive than the parent's (ADR 0006 §compose-down).
    for (agent_name, agent_config) in &pipeline.agents {
        for tool in &agent_config.allowed_tools {
            if let Some(rest) = tool.strip_prefix("mcp.") {
                let server = rest.split('.').next().unwrap_or("");
                if !pipeline.mcp_servers.contains_key(server) {
                    return Err(PipelineError::Validation(format!(
                        "agent `{agent_name}` references MCP tool `{tool}` \
                         with unknown server `{server}`"
                    )));
                }
            } else if let Some(child_name) = tool.strip_prefix("subagent.") {
                let child = pipeline.agents.get(child_name).ok_or_else(|| {
                    PipelineError::Validation(format!(
                        "agent `{agent_name}` references unknown subagent \
                             `{child_name}`"
                    ))
                })?;

                if !agent_config.policy.allow_mutations && child.policy.allow_mutations {
                    return Err(PipelineError::Validation(format!(
                        "agent `{agent_name}` (allow_mutations=false) cannot \
                         delegate to `{child_name}` (allow_mutations=true)"
                    )));
                }
                if !agent_config.policy.allow_network && child.policy.allow_network {
                    return Err(PipelineError::Validation(format!(
                        "agent `{agent_name}` (allow_network=false) cannot \
                         delegate to `{child_name}` (allow_network=true)"
                    )));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::pipeline::schema::{
        AgentConfig, AgentPolicy, McpServerConfig, McpStdioConfig, Node, NodeKind, OnParseError,
        Pipeline, ShellNode,
    };

    fn shell_node(id: &str, needs: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "true".to_string(),
                args: Vec::new(),
                stdin: None,
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            timeout: None,
        }
    }

    fn pipeline(nodes: Vec<Node>) -> Pipeline {
        Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            nodes,
        }
    }

    #[test]
    fn empty_pipeline_is_rejected() {
        let Err(PipelineError::Validation(msg)) = validate(&pipeline(Vec::new())) else {
            panic!("expected Validation error on empty pipeline");
        };
        assert!(
            msg.contains("no nodes"),
            "error message should explain the empty case: {msg}",
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let p = pipeline(vec![shell_node("a", &[]), shell_node("a", &[])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on duplicate ids");
        };
        assert!(
            msg.contains("duplicate") && msg.contains('a'),
            "error message should name the duplicate id: {msg}",
        );
    }

    #[test]
    fn unknown_needs_is_rejected() {
        let p = pipeline(vec![shell_node("b", &["nowhere"])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on unknown dep");
        };
        assert!(
            msg.contains("nowhere"),
            "error message should name the missing dep: {msg}",
        );
    }

    #[test]
    fn well_formed_pipeline_is_accepted() {
        let p = pipeline(vec![shell_node("a", &[]), shell_node("b", &["a"])]);
        assert!(validate(&p).is_ok(), "valid pipeline should pass");
    }

    fn base_policy(allow_mutations: bool, allow_network: bool) -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1000,
            max_tool_calls: 5,
            max_subagent_depth: 1,
            allow_mutations,
            allow_network,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
        }
    }

    fn agent_config(
        allowed_tools: Vec<String>,
        allow_mutations: bool,
        allow_network: bool,
    ) -> AgentConfig {
        AgentConfig {
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
            system: None,
            allowed_tools,
            policy: base_policy(allow_mutations, allow_network),
        }
    }

    fn stdio_mcp_server() -> McpServerConfig {
        McpServerConfig::Stdio(McpStdioConfig {
            command: vec!["echo".to_string()],
            env: BTreeMap::new(),
        })
    }

    #[test]
    fn mcp_tool_with_unknown_server_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "a".to_string(),
            agent_config(vec!["mcp.ghost.tool".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for unknown MCP server");
        };
        assert!(
            msg.contains("ghost"),
            "error should name the unknown server: {msg}"
        );
    }

    #[test]
    fn mcp_wildcard_with_known_server_is_accepted() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(vec!["mcp.fs.*".to_string()], false, true),
        );
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn subagent_referencing_unknown_agent_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.ghost".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for unknown subagent");
        };
        assert!(
            msg.contains("ghost"),
            "error should name the missing child: {msg}"
        );
    }

    #[test]
    fn subagent_more_permissive_mutations_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents
            .insert("child".to_string(), agent_config(Vec::new(), true, false));
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected compose-down validation error");
        };
        assert!(
            msg.contains("allow_mutations"),
            "error should name the policy dimension: {msg}"
        );
    }

    #[test]
    fn subagent_more_permissive_network_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents
            .insert("child".to_string(), agent_config(Vec::new(), false, true));
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected compose-down validation error");
        };
        assert!(
            msg.contains("allow_network"),
            "error should name the policy dimension: {msg}"
        );
    }

    #[test]
    fn subagent_same_or_more_restrictive_policy_is_accepted() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "child".to_string(),
            // same policy as parent
            agent_config(Vec::new(), false, false),
        );
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn cycle_is_rejected_by_validate() {
        // Two-node cycle: a -> b -> a. The existing `unknown needs`
        // check cannot catch this because both ids resolve; only the
        // Kahn pass notices the residual in-degree.
        let p = pipeline(vec![shell_node("a", &["b"]), shell_node("b", &["a"])]);
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on cyclic pipeline");
        };
        assert!(
            msg.contains("cycle"),
            "error message should mention the cycle: {msg}",
        );
    }
}
