//! YAML → validated `Pipeline`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::error::PipelineError;

use super::checks;
use super::graph::DepGraph;
use super::schema::Pipeline;

/// Highest `Pipeline.version` this build understands. Bumped whenever
/// the pipeline schema changes in a backwards-incompatible way so an
/// older build refuses a newer pipeline rather than misinterpreting it,
/// and a newer build can branch on version when reading older shapes.
pub const CURRENT_PIPELINE_VERSION: u32 = 1;

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
///
/// A structural problem (version, empty pipeline, duplicate id, unknown
/// `needs:`, cycle) returns at once, since later checks need a sound
/// graph. Everything else is collected, so one run reports every problem.
///
/// Errors: `PipelineError::InvalidGraph` for graph problems,
/// `PipelineError::Validation` for the rest.
pub fn validate(pipeline: &Pipeline) -> Result<(), PipelineError> {
    // A version ahead of this build can carry shapes the executor cannot
    // dispatch; one behind may apply semantics the author did not opt into.
    if pipeline.version != CURRENT_PIPELINE_VERSION {
        return Err(PipelineError::Validation(format!(
            "pipeline version {} is not supported by this build (expected {CURRENT_PIPELINE_VERSION})",
            pipeline.version,
        )));
    }
    if pipeline.nodes.is_empty() {
        return Err(PipelineError::Validation("pipeline has no nodes".into()));
    }
    let graph = DepGraph::build(&pipeline.nodes)?;

    let mut errors = Vec::new();
    validate_env_lists(pipeline, &mut errors);
    checks::validate_mcp_servers(pipeline, &mut errors);
    checks::validate_nodes(pipeline, &mut errors);
    checks::validate_agents(pipeline, &mut errors);
    checks::validate_templates(pipeline, &graph, &mut errors);

    match errors.len() {
        0 => Ok(()),
        1 => Err(PipelineError::Validation(errors.remove(0))),
        n => Err(PipelineError::Validation(format!(
            "{n} errors:\n  - {}",
            errors.join("\n  - ")
        ))),
    }
}

// A name in both lists would put the same value in the unredacted `env.*`
// namespace as well as `secrets.*`.
fn validate_env_lists(pipeline: &Pipeline, errors: &mut Vec<String>) {
    errors.extend(
        pipeline
            .pass_env
            .iter()
            .filter(|n| pipeline.secrets.contains(n))
            .map(|name| {
                format!(
                    "`{name}` is listed in both `pass_env` and `secrets`; keep it in `secrets` \
                     only so it stays redacted"
                )
            }),
    );
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
        AgentConfig, AgentPolicy, McpHttpConfig, McpServerConfig, McpStdioConfig, Node, NodeKind,
        OnParseError, Pipeline, ShellNode,
    };

    fn shell_node(id: &str, needs: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Shell(ShellNode {
                command: "true".to_string(),
                args: Vec::new(),
                stdin: None,
                env: BTreeMap::new(),
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
    fn name_in_both_pass_env_and_secrets_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.pass_env = vec!["GH_TOKEN".to_string(), "HOME".to_string()];
        p.secrets = vec!["GH_TOKEN".to_string()];
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on pass_env/secrets overlap");
        };
        assert!(
            msg.contains("GH_TOKEN") && msg.contains("pass_env"),
            "{msg}"
        );
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
        let Err(PipelineError::InvalidGraph { reason: msg }) = validate(&p) else {
            panic!("expected InvalidGraph error on duplicate ids");
        };
        assert!(
            msg.contains("duplicate") && msg.contains('a'),
            "error message should name the duplicate id: {msg}",
        );
    }

    #[test]
    fn unknown_needs_is_rejected() {
        let p = pipeline(vec![shell_node("b", &["nowhere"])]);
        let Err(PipelineError::InvalidGraph { reason: msg }) = validate(&p) else {
            panic!("expected InvalidGraph error on unknown dep");
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

    #[test]
    fn unsupported_pipeline_version_is_rejected() {
        // A pipeline marked `version: 999` may carry shapes this build
        // does not understand; refuse rather than dispatch nodes whose
        // semantics may have shifted between schema revisions.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.version = 999;
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error on unsupported pipeline version");
        };
        assert!(
            msg.contains("999") && msg.contains(&CURRENT_PIPELINE_VERSION.to_string()),
            "error should mention found and expected version: {msg}",
        );
    }

    #[test]
    fn mcp_stdio_with_empty_command_is_rejected() {
        // An empty argv would fail at spawn time with an opaque OS
        // error. Catching it during validation turns the failure into
        // an actionable YAML authoring error.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert(
            "broken".to_string(),
            McpServerConfig::Stdio(McpStdioConfig {
                command: Vec::new(),
                env: BTreeMap::new(),
                call_timeout_secs: None,
            }),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for empty stdio command");
        };
        assert!(
            msg.contains("broken") && msg.contains("command"),
            "error should name the server and mention `command`: {msg}",
        );
    }

    #[test]
    fn mcp_zero_call_timeout_is_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        let McpServerConfig::Stdio(mut stdio) = stdio_mcp_server() else {
            unreachable!("stdio_mcp_server builds a stdio config");
        };
        stdio.call_timeout_secs = Some(0);
        p.mcp_servers
            .insert("zero".to_string(), McpServerConfig::Stdio(stdio));
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for zero call_timeout_secs");
        };
        assert!(
            msg.contains("zero") && msg.contains("call_timeout_secs"),
            "error should name the server and the field: {msg}",
        );
    }

    fn base_policy(allow_mutations: bool, allow_network: bool) -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1000,
            max_tool_calls: 5,
            max_subagent_depth: 1,
            max_tool_output_bytes: None,
            allow_mutations,
            allow_network,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
            roots: Vec::new(),
            max_message_history_bytes: None,
            unconfined_shell: false,
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

    /// Build an `AgentConfig` with a fully custom `AgentPolicy`. Used by
    /// compose-down tests that need to vary fields beyond the
    /// `allow_mutations` / `allow_network` pair the shorthand exposes
    /// (depth, roots, context-writes).
    fn agent_config_with_policy(allowed_tools: Vec<String>, policy: AgentPolicy) -> AgentConfig {
        AgentConfig {
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
            system: None,
            allowed_tools,
            policy,
        }
    }

    fn stdio_mcp_server() -> McpServerConfig {
        McpServerConfig::Stdio(McpStdioConfig {
            command: vec!["echo".to_string()],
            env: BTreeMap::new(),
            call_timeout_secs: None,
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
        // Parent dispatches to child; mutation/network/context-write
        // policies are identical and the child's `max_subagent_depth` is
        // strictly less than the parent's (parent spends one unit of
        // recursion to enter the child, so child depth ≤ parent - 1).
        let mut p = pipeline(vec![shell_node("n", &[])]);
        let mut child_policy = base_policy(false, false);
        child_policy.max_subagent_depth = 0;
        p.agents.insert(
            "child".to_string(),
            agent_config_with_policy(Vec::new(), child_policy),
        );
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        assert!(validate(&p).is_ok());
    }

    #[test]
    fn validate_subagent_compose_down_allow_context_writes() {
        // Compose-down: a child cannot enable `allow_context_writes`
        // when the parent has it disabled — otherwise the parent's
        // ContextSelf gate could be bypassed by delegating.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        let mut child_policy = base_policy(false, false);
        child_policy.allow_context_writes = true;
        child_policy.max_subagent_depth = 0;
        p.agents.insert(
            "child".to_string(),
            agent_config_with_policy(Vec::new(), child_policy),
        );
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected compose-down validation error for allow_context_writes");
        };
        assert!(
            msg.contains("allow_context_writes"),
            "error should name the policy dimension: {msg}"
        );
    }

    #[test]
    fn validate_subagent_roots_must_be_subset() {
        // Compose-down: every child root must be a subdirectory of
        // some parent root. A sibling path (`/work/other` next to
        // `/work/parent`) is not — surface the offending path so the
        // operator can see which root needs to move.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        let mut parent_policy = base_policy(false, false);
        parent_policy.roots = vec![std::path::PathBuf::from("/work/parent")];
        let mut child_policy = base_policy(false, false);
        child_policy.roots = vec![std::path::PathBuf::from("/work/other")];
        child_policy.max_subagent_depth = 0;
        p.agents.insert(
            "child".to_string(),
            agent_config_with_policy(Vec::new(), child_policy),
        );
        p.agents.insert(
            "parent".to_string(),
            agent_config_with_policy(vec!["subagent.child".to_string()], parent_policy),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected compose-down validation error for roots subset");
        };
        assert!(
            msg.contains("/work/other"),
            "error should name the offending root: {msg}"
        );
    }

    #[test]
    fn validate_subagent_max_depth_decreases() {
        // Compose-down: a child's `max_subagent_depth` must be at
        // most parent - 1, since the parent spends one unit of
        // recursion to enter the child. Equal depths fail.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        // Both default to depth=1 from `base_policy`.
        p.agents
            .insert("child".to_string(), agent_config(Vec::new(), false, false));
        p.agents.insert(
            "parent".to_string(),
            agent_config(vec!["subagent.child".to_string()], false, false),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected compose-down validation error for max_subagent_depth");
        };
        assert!(
            msg.contains("max_subagent_depth"),
            "error should name the policy dimension: {msg}"
        );
    }

    fn http_mcp_server(url: &str) -> McpServerConfig {
        McpServerConfig::Http(McpHttpConfig {
            url: url.to_string(),
            auth: None,
            headers: BTreeMap::new(),
            call_timeout_secs: None,
        })
    }

    #[test]
    fn validate_mcp_http_rejects_non_loopback() {
        // Plain `http://` is restricted to loopback so credentials in
        // `Authorization` headers are not sent in the clear over the
        // network.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers
            .insert("remote".to_string(), http_mcp_server("http://example.com"));
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for non-loopback http://");
        };
        assert!(
            msg.contains("loopback") && msg.contains("https"),
            "error should explain that https is required for remote: {msg}"
        );
    }

    #[test]
    fn validate_mcp_http_allows_loopback() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert(
            "local".to_string(),
            http_mcp_server("http://localhost:5000"),
        );
        assert!(
            validate(&p).is_ok(),
            "http://localhost must pass validation"
        );
    }

    #[test]
    fn validate_mcp_https_allows_remote() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert(
            "remote".to_string(),
            http_mcp_server("https://api.example.com"),
        );
        assert!(
            validate(&p).is_ok(),
            "https:// to a public host must pass validation"
        );
    }

    #[test]
    fn validate_mcp_http_rejects_blocked_ip() {
        // The cloud metadata IP — exactly the kind of address an MCP
        // server URL should never target — even on https.
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert(
            "metadata".to_string(),
            http_mcp_server("https://169.254.169.254/"),
        );
        let Err(PipelineError::Validation(msg)) = validate(&p) else {
            panic!("expected Validation error for blocked IP range");
        };
        assert!(
            msg.contains("blocked range") || msg.contains("169.254"),
            "error should explain why the IP is blocked: {msg}"
        );
    }

    #[rstest::rstest]
    #[case::domains(&["api.test"], &[], false, Some("`allowed_domains`"))]
    #[case::roots(&[], &["."], false, Some("`roots`"))]
    #[case::both(&["api.test"], &["."], false, Some("`allowed_domains` and `roots`"))]
    #[case::acknowledged(&["api.test"], &["."], true, None)]
    #[case::no_confinement(&[], &[], false, None)]
    fn bash_next_to_confinement_needs_unconfined_shell(
        #[case] domains: &[&str],
        #[case] roots: &[&str],
        #[case] unconfined_shell: bool,
        #[case] rejected_for: Option<&str>,
    ) {
        let mut policy = base_policy(true, true);
        policy.allowed_domains = domains.iter().map(ToString::to_string).collect();
        policy.roots = roots.iter().map(Into::into).collect();
        policy.unconfined_shell = unconfined_shell;
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "triager".to_string(),
            agent_config_with_policy(vec!["Bash".to_string()], policy),
        );

        match (validate(&p), rejected_for) {
            (Ok(()), None) => {},
            (Err(PipelineError::Validation(msg)), Some(what)) => {
                assert!(
                    msg.contains("`triager`")
                        && msg.contains(what)
                        && msg.contains("unconfined_shell"),
                    "error should name the agent, the confinement and the escape hatch: {msg}",
                );
            },
            (other, expected) => panic!("expected {expected:?}, got {other:?}"),
        }
    }

    #[test]
    fn cycle_is_rejected_by_validate() {
        // Two-node cycle: a -> b -> a. The existing `unknown needs`
        // check cannot catch this because both ids resolve; only the
        // Kahn pass notices the residual in-degree.
        let p = pipeline(vec![shell_node("a", &["b"]), shell_node("b", &["a"])]);
        let Err(PipelineError::InvalidGraph { reason: msg }) = validate(&p) else {
            panic!("expected InvalidGraph error on cyclic pipeline");
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

    fn agent_node(id: &str, needs: &[&str], agent: &str, prompt: &str) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Agent(crate::pipeline::schema::AgentNode {
                agent: agent.to_string(),
                initial_prompt: prompt.to_string(),
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            timeout: None,
        }
    }

    fn shell_with_command(id: &str, needs: &[&str], command: &str) -> Node {
        let mut node = shell_node(id, needs);
        if let NodeKind::Shell(shell) = &mut node.kind {
            shell.command = command.to_string();
        }
        node
    }

    fn validation_message(p: &Pipeline) -> String {
        match validate(p) {
            Err(PipelineError::Validation(msg)) => msg,
            other => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn node_referencing_undeclared_agent_is_rejected() {
        let p = pipeline(vec![agent_node("n", &[], "ghost", "hi")]);
        let msg = validation_message(&p);
        assert!(msg.contains("node `n`") && msg.contains("ghost"), "{msg}");
    }

    #[rstest::rstest]
    #[case::webfetch_needs_network(&["WebFetch"], (false, false, false), "allow_network is false")]
    #[case::edit_needs_mutations(&["Edit"], (false, false, false), "allow_mutations is false")]
    #[case::bash_needs_mutations(&["Bash"], (false, true, false), "allow_mutations is false")]
    #[case::bash_needs_network(&["Bash"], (true, false, false), "allow_network is false")]
    #[case::setstate_needs_context_writes(&["SetState"], (false, false, false), "allow_context_writes is false")]
    #[case::unknown_tool(&["Teleport"], (true, true, true), "not a known builtin")]
    #[case::malformed_mcp_tool(&["mcp.srv"], (true, true, true), "not a valid mcp tool reference")]
    fn tool_gates_and_names_are_checked_at_load(
        #[case] tools: &[&str],
        #[case] (mutations, network, context_writes): (bool, bool, bool),
        #[case] expected: &str,
    ) {
        let mut policy = base_policy(mutations, network);
        policy.allow_context_writes = context_writes;
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "a".to_string(),
            agent_config_with_policy(tools.iter().map(|t| (*t).to_string()).collect(), policy),
        );
        let msg = validation_message(&p);
        assert!(msg.contains(expected), "expected `{expected}` in: {msg}");
    }

    #[rstest::rstest]
    #[case::iterations(0, 1000, 5, "max_iterations must be > 0")]
    #[case::tokens(1, 0, 5, "max_total_tokens must be > 0")]
    #[case::tool_calls(1, 1000, 0, "max_tool_calls is 0")]
    fn zero_budgets_are_rejected(
        #[case] iterations: u32,
        #[case] tokens: u64,
        #[case] tool_calls: u32,
        #[case] expected: &str,
    ) {
        let mut policy = base_policy(true, true);
        policy.max_iterations = iterations;
        policy.max_total_tokens = tokens;
        policy.max_tool_calls = tool_calls;
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "a".to_string(),
            agent_config_with_policy(vec!["Read".to_string()], policy),
        );
        let msg = validation_message(&p);
        assert!(msg.contains(expected), "{msg}");
    }

    #[test]
    fn every_problem_is_reported_in_one_run() {
        let mut p = pipeline(vec![agent_node("n", &[], "ghost", "hi")]);
        p.agents.insert(
            "a".to_string(),
            agent_config(vec!["WebFetch".to_string()], false, false),
        );
        let msg = validation_message(&p);
        assert!(
            msg.starts_with("2 errors:") && msg.contains("ghost") && msg.contains("WebFetch"),
            "{msg}"
        );
    }

    #[test]
    fn mcp_server_names_with_underscore_prefix_overlap_are_rejected() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers.insert("foo".to_string(), stdio_mcp_server());
        p.mcp_servers
            .insert("foo_bar".to_string(), stdio_mcp_server());
        let msg = validation_message(&p);
        assert!(
            msg.contains("foo") && msg.contains("foo_bar") && msg.contains("Rename"),
            "{msg}"
        );
    }

    #[test]
    fn dormant_domain_list_warns_without_failing() {
        let mut policy = base_policy(false, false);
        policy.allowed_domains = vec!["x.com".to_string()];
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "a".to_string(),
            agent_config_with_policy(Vec::new(), policy),
        );
        assert!(validate(&p).is_ok());
        let warnings = checks::warnings(&p);
        assert!(
            warnings.iter().any(|w| w.contains("allow_network=false")),
            "{warnings:?}"
        );
    }

    #[rstest::rstest]
    #[case::syntax_error("{{ invalid syntax ]}", "invalid template")]
    #[case::unclosed_block("{% if x %}no end", "invalid template")]
    fn template_syntax_errors_fail_at_load(#[case] command: &str, #[case] expected: &str) {
        let p = pipeline(vec![shell_with_command("n", &[], command)]);
        let msg = validation_message(&p);
        assert!(
            msg.contains(expected) && msg.contains("node `n` command"),
            "{msg}"
        );
    }

    #[rstest::rstest]
    #[case::non_ancestor(&[("a", &[][..]), ("b", &[][..])], "{{ nodes.a.stdout }}", "not an upstream dependency")]
    #[case::undeclared(&[("a", &[][..])], "{{ nodes.ghost.stdout }}", "names no declared node")]
    #[case::undeclared_with_chain(&[("a", &[][..]), ("b", &["a"][..])], "{{ nodes.c.stdout }}", "names no declared node")]
    fn node_references_must_be_ancestors(
        #[case] upstream: &[(&str, &[&str])],
        #[case] template: &str,
        #[case] expected: &str,
    ) {
        let mut nodes: Vec<Node> = upstream
            .iter()
            .map(|(id, needs)| shell_node(id, needs))
            .collect();
        nodes.push(shell_with_command("last", &[], template));
        let msg = validation_message(&pipeline(nodes));
        assert!(msg.contains(expected) && msg.contains("last"), "{msg}");
    }

    #[test]
    fn transitive_ancestor_reference_is_accepted() {
        let p = pipeline(vec![
            shell_node("a", &[]),
            shell_node("b", &["a"]),
            shell_with_command(
                "c",
                &["b"],
                "echo {{ nodes.a.stdout }} {{ nodes.b.exit_code }}",
            ),
        ]);
        validate(&p).expect("a is a transitive ancestor of c");
    }

    #[test]
    fn agent_prompt_and_system_references_are_checked() {
        let mut p = pipeline(vec![
            shell_node("a", &[]),
            agent_node("n", &[], "writer", "{{ nodes.a.stdout }}"),
        ]);
        let mut config = agent_config(Vec::new(), false, false);
        config.system = Some("{{ nodes.a.stdout }}".to_string());
        p.agents.insert("writer".to_string(), config);
        let msg = validation_message(&p);
        assert!(
            msg.contains("initial_prompt") && msg.contains("agent `writer` system"),
            "{msg}"
        );
    }

    #[rstest::rstest]
    #[case::hyphen("a-b")]
    #[case::leading_digit("1a")]
    #[case::dot("a.b")]
    #[case::space("a b")]
    #[case::empty("")]
    fn invalid_node_ids_are_rejected(#[case] id: &str) {
        let msg = validation_message(&pipeline(vec![shell_node(id, &[])]));
        assert!(msg.contains("[A-Za-z_][A-Za-z0-9_]*"), "{msg}");
    }

    #[test]
    fn zero_node_timeout_is_rejected() {
        let mut node = shell_node("slow", &[]);
        node.timeout = Some(0);
        let msg = validation_message(&pipeline(vec![node]));
        assert!(msg.contains("slow") && msg.contains("timeout"), "{msg}");
    }

    #[rstest::rstest]
    #[case::empty("")]
    #[case::blank("   ")]
    fn empty_shell_command_is_rejected(#[case] command: &str) {
        let msg = validation_message(&pipeline(vec![shell_with_command("n", &[], command)]));
        assert!(
            msg.contains("node `n`") && msg.contains("`command` is empty"),
            "{msg}"
        );
    }

    #[rstest::rstest]
    #[case::escape("/work/../etc")]
    #[case::relative("../outside")]
    fn parent_dir_in_roots_is_rejected(#[case] root: &str) {
        let mut policy = base_policy(false, false);
        policy.roots = vec![std::path::PathBuf::from(root)];
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.agents.insert(
            "a".to_string(),
            agent_config_with_policy(Vec::new(), policy),
        );
        let msg = validation_message(&p);
        assert!(msg.contains(root) && msg.contains("`..`"), "{msg}");
    }

    #[test]
    fn templated_mcp_url_is_deferred_to_render_time() {
        let mut p = pipeline(vec![shell_node("n", &[])]);
        p.mcp_servers
            .insert("web".to_string(), http_mcp_server("{{ env.MCP_URL }}/mcp"));
        validate(&p).expect("a templated url cannot be checked before it is rendered");
    }
}
