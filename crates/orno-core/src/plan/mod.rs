//! Static pipeline preview — `terraform plan` for agent pipelines.
//!
//! [`plan_pipeline`] folds a parsed [`Pipeline`] into an NDJSON-friendly
//! sequence of [`PlanOutput`] records: one [`PlanNode`] per node followed
//! by a single [`PlanSummary`]. No LLM calls, tool executions, or network
//! I/O happen here — the function only inspects the schema, validates the
//! DAG, and re-projects the declared budgets and effect knobs into a
//! shape consumers can stream.
//!
//! Consumers (the `orno plan` CLI, downstream review tooling) read each
//! line as a tagged union; serde's external-tagged form (`type` field)
//! makes "is this a node or the summary?" a constant-time check without
//! parsing the rest of the payload.

use serde::{Deserialize, Serialize};

use crate::error::PipelineError;
use crate::execution::walker::DagWalker;
use crate::pipeline::Pipeline;
use crate::pipeline::schema::NodeKind;

/// Per-node static preview. Captures everything a reviewer needs to
/// reason about what the node *would* do at runtime — its kind, its
/// upstream dependencies, the budgets it inherits from its referenced
/// agent, and the effect knobs that gate tool calls. Agent-only fields
/// are `None` / empty for shell nodes.
///
/// Fields mirror the on-disk YAML names so a reader can map a plan line
/// back to the source pipeline without consulting the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanNode {
    pub node_id: String,
    pub kind: String,
    pub depends_on: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub agent_name: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tools: Vec<String>,
    pub max_iterations: Option<u32>,
    pub max_total_tokens: Option<u64>,
    pub max_tool_calls: Option<u32>,
    pub allow_mutations: Option<bool>,
    pub allow_network: Option<bool>,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}

/// Aggregate preview emitted once after every [`PlanNode`]. Sums the
/// declared budgets across all referenced agents so a reviewer sees the
/// worst-case ceiling for the run without having to fold the per-node
/// records themselves. `dag_is_valid` is always `true` when this record
/// is emitted — [`plan_pipeline`] returns `Err` before reaching the
/// summary if DAG validation fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanSummary {
    pub total_nodes: usize,
    pub agent_nodes: usize,
    pub shell_nodes: usize,
    pub agents_used: Vec<String>,
    pub max_iterations_total: u64,
    pub max_tokens_total: u64,
    pub max_tool_calls_total: u64,
    pub mcp_servers: Vec<String>,
    pub dag_is_valid: bool,
}

/// NDJSON wire variant emitted by [`plan_pipeline`]. The `type` tag
/// discriminates `plan_node` from `plan_summary` so streaming consumers
/// can branch without parsing the inner payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PlanOutput {
    PlanNode(PlanNode),
    PlanSummary(PlanSummary),
}

/// Statically analyze a pipeline and return a sequence of [`PlanOutput`]
/// values: one [`PlanNode`] per node in source order, then one
/// [`PlanSummary`].
///
/// DAG validation runs through [`DagWalker::new`] before any record is
/// produced — a cyclic graph or an unknown `needs:` target returns
/// `Err(PipelineError::InvalidGraph)` and the caller surfaces a non-zero
/// exit code at the CLI layer.
///
/// Agent nodes that reference an undeclared agent name (a load-time
/// validator gap) still emit a [`PlanNode`] with the agent-only fields
/// left as `None` / empty, so the preview surfaces the missing reference
/// rather than swallowing the node.
///
/// # Errors
///
/// Returns [`PipelineError::InvalidGraph`] when the DAG cannot be walked
/// (cycle, unknown `needs:` target).
pub fn plan_pipeline(pipeline: &Pipeline) -> Result<Vec<PlanOutput>, PipelineError> {
    DagWalker::new(pipeline)?;

    let mut outputs: Vec<PlanOutput> = Vec::with_capacity(pipeline.nodes.len() + 1);
    let mut totals = BudgetTotals::default();

    for node in &pipeline.nodes {
        let plan_node = match &node.kind {
            NodeKind::Agent(agent_node) => {
                totals.agent_nodes += 1;
                totals.agents_used.push(agent_node.agent.clone());
                let config = pipeline.agents.get(&agent_node.agent);
                if let Some(cfg) = config {
                    totals.max_iterations_total += u64::from(cfg.policy.max_iterations);
                    totals.max_tokens_total = totals
                        .max_tokens_total
                        .saturating_add(cfg.policy.max_total_tokens);
                    totals.max_tool_calls_total += u64::from(cfg.policy.max_tool_calls);
                }
                build_agent_plan_node(node, agent_node, config)
            },
            NodeKind::Shell(_) => {
                totals.shell_nodes += 1;
                build_shell_plan_node(node)
            },
        };
        outputs.push(PlanOutput::PlanNode(plan_node));
    }

    totals.agents_used.sort();
    totals.agents_used.dedup();

    let mut mcp_servers: Vec<String> = pipeline.mcp_servers.keys().cloned().collect();
    mcp_servers.sort();

    let summary = PlanSummary {
        total_nodes: pipeline.nodes.len(),
        agent_nodes: totals.agent_nodes,
        shell_nodes: totals.shell_nodes,
        agents_used: totals.agents_used,
        max_iterations_total: totals.max_iterations_total,
        max_tokens_total: totals.max_tokens_total,
        max_tool_calls_total: totals.max_tool_calls_total,
        mcp_servers,
        dag_is_valid: true,
    };
    outputs.push(PlanOutput::PlanSummary(summary));

    Ok(outputs)
}

#[derive(Default)]
struct BudgetTotals {
    agent_nodes: usize,
    shell_nodes: usize,
    agents_used: Vec<String>,
    max_iterations_total: u64,
    max_tokens_total: u64,
    max_tool_calls_total: u64,
}

fn build_agent_plan_node(
    node: &crate::pipeline::Node,
    agent_node: &crate::pipeline::AgentNode,
    config: Option<&crate::pipeline::AgentConfig>,
) -> PlanNode {
    PlanNode {
        node_id: node.id.clone(),
        kind: "agent".to_string(),
        depends_on: node.needs.clone(),
        timeout_secs: node.timeout,
        agent_name: Some(agent_node.agent.clone()),
        model: config.map(|c| c.model.clone()),
        provider: config.map(|c| c.provider.clone()),
        tools: config.map(|c| c.allowed_tools.clone()).unwrap_or_default(),
        max_iterations: config.map(|c| c.policy.max_iterations),
        max_total_tokens: config.map(|c| c.policy.max_total_tokens),
        max_tool_calls: config.map(|c| c.policy.max_tool_calls),
        allow_mutations: config.map(|c| c.policy.allow_mutations),
        allow_network: config.map(|c| c.policy.allow_network),
        allowed_domains: config
            .map(|c| c.policy.allowed_domains.clone())
            .unwrap_or_default(),
        blocked_domains: config
            .map(|c| c.policy.blocked_domains.clone())
            .unwrap_or_default(),
    }
}

fn build_shell_plan_node(node: &crate::pipeline::Node) -> PlanNode {
    PlanNode {
        node_id: node.id.clone(),
        kind: "shell".to_string(),
        depends_on: node.needs.clone(),
        timeout_secs: node.timeout,
        agent_name: None,
        model: None,
        provider: None,
        tools: Vec::new(),
        max_iterations: None,
        max_total_tokens: None,
        max_tool_calls: None,
        allow_mutations: None,
        allow_network: None,
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::pipeline::schema::{
        AgentConfig, AgentNode, AgentPolicy, Node, NodeKind, OnParseError, Pipeline, ShellNode,
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

    fn agent_node(id: &str, agent: &str, needs: &[&str]) -> Node {
        Node {
            id: id.to_string(),
            kind: NodeKind::Agent(AgentNode {
                agent: agent.to_string(),
                initial_prompt: "hello".to_string(),
            }),
            needs: needs.iter().map(|s| (*s).to_string()).collect(),
            timeout: None,
        }
    }

    fn agent_config(model: &str) -> AgentConfig {
        AgentConfig {
            model: model.to_string(),
            provider: "openrouter".to_string(),
            system: None,
            allowed_tools: vec!["Read".to_string()],
            policy: AgentPolicy {
                max_iterations: 5,
                max_total_tokens: 10_000,
                max_tool_calls: 20,
                max_subagent_depth: 1,
                allow_mutations: false,
                allow_network: false,
                allow_context_writes: false,
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
                on_parse_error: OnParseError::Fail,
            },
        }
    }

    #[test]
    fn two_node_pipeline_emits_two_nodes_and_summary() {
        let mut agents = BTreeMap::new();
        agents.insert("writer".to_string(), agent_config("openai/gpt-5"));
        let pipeline = Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents,
            mcp_servers: BTreeMap::new(),
            nodes: vec![shell_node("a", &[]), agent_node("b", "writer", &["a"])],
        };

        let outputs = plan_pipeline(&pipeline).expect("valid pipeline plans");
        assert_eq!(outputs.len(), 3, "two nodes + one summary");

        let PlanOutput::PlanNode(first) = &outputs[0] else {
            panic!("first output must be a PlanNode");
        };
        assert_eq!(first.node_id, "a");
        assert_eq!(first.kind, "shell");
        assert!(first.agent_name.is_none());
        assert!(first.tools.is_empty());

        let PlanOutput::PlanNode(second) = &outputs[1] else {
            panic!("second output must be a PlanNode");
        };
        assert_eq!(second.node_id, "b");
        assert_eq!(second.kind, "agent");
        assert_eq!(second.agent_name.as_deref(), Some("writer"));
        assert_eq!(second.depends_on, vec!["a".to_string()]);
        assert_eq!(second.max_iterations, Some(5));
        assert_eq!(second.allow_mutations, Some(false));

        let PlanOutput::PlanSummary(summary) = &outputs[2] else {
            panic!("last output must be a PlanSummary");
        };
        assert_eq!(summary.total_nodes, 2);
        assert_eq!(summary.agent_nodes, 1);
        assert_eq!(summary.shell_nodes, 1);
        assert_eq!(summary.agents_used, vec!["writer".to_string()]);
        assert_eq!(summary.max_iterations_total, 5);
        assert_eq!(summary.max_tokens_total, 10_000);
        assert_eq!(summary.max_tool_calls_total, 20);
        assert!(summary.dag_is_valid);
    }

    #[test]
    fn cyclic_pipeline_returns_invalid_graph() {
        let pipeline = Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            nodes: vec![shell_node("a", &["b"]), shell_node("b", &["a"])],
        };

        let Err(PipelineError::InvalidGraph { reason }) = plan_pipeline(&pipeline) else {
            panic!("expected InvalidGraph on cyclic pipeline");
        };
        assert!(
            reason.contains('a') || reason.contains('b'),
            "reason should name an involved node id; got {reason:?}",
        );
    }

    #[test]
    fn agent_used_twice_appears_once_in_summary() {
        let mut agents = BTreeMap::new();
        agents.insert("writer".to_string(), agent_config("openai/gpt-5"));
        let pipeline = Pipeline {
            version: 1,
            vars: BTreeMap::new(),
            pass_env: Vec::new(),
            secrets: Vec::new(),
            agents,
            mcp_servers: BTreeMap::new(),
            nodes: vec![
                agent_node("a", "writer", &[]),
                agent_node("b", "writer", &["a"]),
            ],
        };

        let outputs = plan_pipeline(&pipeline).expect("valid pipeline plans");
        let PlanOutput::PlanSummary(summary) = outputs.last().expect("summary present") else {
            panic!("last output must be a PlanSummary");
        };
        assert_eq!(summary.agents_used, vec!["writer".to_string()]);
        // Budgets sum across both agent nodes that reference `writer`.
        assert_eq!(summary.max_iterations_total, 10);
        assert_eq!(summary.max_tokens_total, 20_000);
        assert_eq!(summary.max_tool_calls_total, 40);
    }
}
