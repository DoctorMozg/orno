//! YAML → validated `Pipeline`.

use std::collections::{BTreeMap, HashSet};
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

/// Parse a pipeline from an in-memory YAML string and validate it.
///
/// Mirrors `load_from_path` for callers that already hold the YAML
/// bytes — `orno replay` reads the pipeline body out of a bundle
/// file's `pipeline_yaml` line rather than re-opening the original
/// source on disk.
pub fn load_from_str(yaml: &str) -> Result<Pipeline, PipelineError> {
    let pipeline: Pipeline = serde_yaml_ng::from_str(yaml).map_err(PipelineError::Parse)?;
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

    let mut ids = HashSet::new();
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
    //   in `Pipeline.mcp_servers`. The wildcard form (`*`) is validated here
    //   but expanded later by the orchestrator (`expand_mcp_wildcards`) once
    //   the server has handshaked and advertised its real tool list.
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

/// Expand `mcp.<server>.*` wildcards in every agent's `allowed_tools`
/// against the tool list each server actually advertised at handshake.
///
/// `validate` accepts the wildcard form structurally but does not expand
/// it — the real tool list is only known after each MCP server has
/// initialized. The orchestrator (`commands/run.rs`) calls this once the
/// `tools/list` results are in hand; `commands/replay.rs` calls it with
/// the per-server tool names lifted from the recorded LLM tape so a
/// replayed run sees the same expanded allowlist the recording did.
///
/// Mutates `pipeline.agents` in place. After this returns, no
/// `allowed_tools` entry contains a wildcard, so the agent loop's
/// pre-flight `find_handler` check sees only exact names.
///
/// `server_tools` maps each MCP server name to its advertised tool list
/// (raw tool names from `tools/list`, not the wire form). Servers
/// declared in `pipeline.mcp_servers` but absent from `server_tools` —
/// e.g. a recording with no MCP traffic — are treated as having an
/// empty tool list; their wildcards expand to nothing rather than an
/// error so a replay of a never-called server stays a no-op.
///
/// Dedupe semantics: when an agent declares both `mcp.<server>.*` and
/// an explicit `mcp.<server>.<tool>`, the wildcard expansion does not
/// re-emit `<tool>`. The first occurrence (in declaration order) wins;
/// later duplicates are silently skipped.
///
/// Order semantics: every entry — explicit name, wildcard expansion,
/// builtin, subagent — keeps its position in declaration order. Wildcard
/// expansion lands at the wildcard's position. This makes the
/// post-expansion list readable top-down for users who later inspect
/// the run-time state via tracing.
pub fn expand_mcp_wildcards(
    pipeline: &mut Pipeline,
    server_tools: &BTreeMap<String, Vec<String>>,
) -> Result<(), PipelineError> {
    for agent_config in pipeline.agents.values_mut() {
        let mut expanded: Vec<String> = Vec::with_capacity(agent_config.allowed_tools.len());
        let mut seen: HashSet<String> = HashSet::new();

        for tool in &agent_config.allowed_tools {
            // Wildcard form: `mcp.<server>.*`. Anything else (including
            // `mcp.<server>.<tool>`) takes the literal-keep branch.
            if let Some(rest) = tool.strip_prefix("mcp.")
                && let Some(server_name) = rest.strip_suffix(".*")
            {
                if let Some(tools) = server_tools.get(server_name) {
                    for tool_name in tools {
                        let yaml_name = format!("mcp.{server_name}.{tool_name}");
                        if seen.insert(yaml_name.clone()) {
                            expanded.push(yaml_name);
                        }
                    }
                }
                continue;
            }

            if seen.insert(tool.clone()) {
                expanded.push(tool.clone());
            }
        }

        agent_config.allowed_tools = expanded;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn server_tools(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(server, tools)| {
                (
                    (*server).to_string(),
                    tools.iter().map(|s| (*s).to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn wildcard_expands_to_advertised_tools_in_position() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(
                vec![
                    "Bash".to_string(),
                    "mcp.fs.*".to_string(),
                    "WebFetch".to_string(),
                ],
                false,
                true,
            ),
        );

        let tools = server_tools(&[("fs", &["read_file", "list_directory"])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["a"].allowed_tools,
            vec![
                "Bash".to_string(),
                "mcp.fs.read_file".to_string(),
                "mcp.fs.list_directory".to_string(),
                "WebFetch".to_string(),
            ],
            "wildcard should expand at its own position; non-MCP entries unchanged",
        );
    }

    #[test]
    fn wildcard_dedupes_against_explicit_entry() {
        // Mixed declaration: explicit `read_file` first, wildcard second.
        // The wildcard would otherwise re-emit `read_file`; dedupe must
        // keep only the explicit entry's position.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(
                vec!["mcp.fs.read_file".to_string(), "mcp.fs.*".to_string()],
                false,
                true,
            ),
        );

        let tools = server_tools(&[("fs", &["read_file", "list_directory"])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["a"].allowed_tools,
            vec![
                "mcp.fs.read_file".to_string(),
                "mcp.fs.list_directory".to_string(),
            ],
            "explicit first → keeps position; wildcard skips the duplicate",
        );
    }

    #[test]
    fn wildcard_for_server_with_zero_tools_drops_to_empty() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(
                vec!["Bash".to_string(), "mcp.fs.*".to_string()],
                false,
                false,
            ),
        );

        let tools = server_tools(&[("fs", &[])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["a"].allowed_tools,
            vec!["Bash".to_string()],
            "empty server tool list → wildcard expands to nothing, leaving builtins",
        );
    }

    #[test]
    fn wildcard_for_unrecorded_server_drops_to_empty() {
        // Replay-side scenario: the bundle's pipeline declares an MCP
        // server that the recording never actually exercised, so
        // `server_tools` doesn't have an entry for it. Expansion must
        // not error — it just removes the wildcard so the agent loop
        // sees a clean (possibly empty) `allowed_tools` slice.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers
            .insert("ghost".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(vec!["mcp.ghost.*".to_string()], false, false),
        );

        let tools: BTreeMap<String, Vec<String>> = BTreeMap::new();
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert!(
            p.agents["a"].allowed_tools.is_empty(),
            "unrecorded server → wildcard drops to nothing instead of erroring",
        );
    }

    #[test]
    fn explicit_mcp_entries_pass_through_unchanged() {
        // `mcp.<server>.<tool>` is not a wildcard; it must survive
        // expansion verbatim regardless of what `server_tools` says.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "a".to_string(),
            agent_config(vec!["mcp.fs.read_file".to_string()], false, false),
        );

        let tools = server_tools(&[("fs", &["list_directory"])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["a"].allowed_tools,
            vec!["mcp.fs.read_file".to_string()],
            "explicit name survives even when not in advertised tool list",
        );
    }

    #[test]
    fn non_mcp_entries_are_preserved() {
        // Builtin and `subagent.*` entries must pass through expansion
        // untouched. This is the load-bearing rule that lets us call
        // `expand_mcp_wildcards` unconditionally.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents
            .insert("child".to_string(), agent_config(Vec::new(), false, false));
        p.agents.insert(
            "parent".to_string(),
            agent_config(
                vec![
                    "Bash".to_string(),
                    "Read".to_string(),
                    "subagent.child".to_string(),
                ],
                false,
                false,
            ),
        );

        let tools = server_tools(&[("fs", &["read_file"])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["parent"].allowed_tools,
            vec![
                "Bash".to_string(),
                "Read".to_string(),
                "subagent.child".to_string(),
            ],
        );
    }

    #[test]
    fn wildcard_expansion_propagates_to_subagents() {
        // Subagents are constructed by cloning their `AgentConfig` from
        // `pipeline.agents`. If we expand wildcards in place before
        // SubagentHandler construction, the cloned config carries the
        // expanded list — this test pins that contract from the load
        // layer's side. Order: child entry first so we can iterate
        // `pipeline.agents` and confirm the child's wildcard expanded.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("fs".to_string(), stdio_mcp_server());
        p.agents.insert(
            "child".to_string(),
            agent_config(vec!["mcp.fs.*".to_string()], false, true),
        );
        p.agents.insert(
            "parent".to_string(),
            agent_config(
                vec!["subagent.child".to_string(), "mcp.fs.*".to_string()],
                false,
                true,
            ),
        );

        let tools = server_tools(&[("fs", &["read_file", "write_file"])]);
        expand_mcp_wildcards(&mut p, &tools).expect("expansion succeeds");

        assert_eq!(
            p.agents["child"].allowed_tools,
            vec![
                "mcp.fs.read_file".to_string(),
                "mcp.fs.write_file".to_string(),
            ],
        );
        assert_eq!(
            p.agents["parent"].allowed_tools,
            vec![
                "subagent.child".to_string(),
                "mcp.fs.read_file".to_string(),
                "mcp.fs.write_file".to_string(),
            ],
        );
    }
}
