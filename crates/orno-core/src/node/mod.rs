//! Node executor trait and shared wire types.
//!
//! `NodeRequest` and `NodeResponse` are the serde-tagged contracts that
//! every node kind flows through. Built-in executors implement the same
//! trait; the subprocess plugin transport (deferred past v0.1 per ADR
//! 0017) will reuse the same wire format without reintroducing a
//! sibling kind.

pub mod agent;
pub mod registry;
pub mod shell;

pub use registry::NodeRegistry;

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{NodeError, PipelineError};
use crate::execution::context::Context;
use crate::pipeline::NodeKind;
use crate::pipeline::schema::{AgentConfig, AgentPolicy};
use crate::pipeline::template::TemplateEngine;

/// Input to a node. `#[non_exhaustive]` so we can grow the variant list
/// without breaking on-disk replays.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeRequest {
    Agent(AgentNodeRequest),
    Shell(ShellNodeRequest),
}

/// Runtime payload for a `kind: agent` node. Agents are referenced by
/// name in YAML; the scheduler resolves the name against
/// `Pipeline.agents` before dispatch and materializes the policy +
/// tool allowlist here, so `AgentExecutor` never needs to touch the
/// pipeline itself. Templated fields (`initial_prompt`, `system`) are
/// already rendered by the time they reach this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNodeRequest {
    /// Named reference, kept for diagnostics and future subagent
    /// depth-tracking. The scheduler resolves everything else.
    pub agent: String,
    /// First user message sent to the agent loop.
    pub initial_prompt: String,
    /// System prompt, already rendered. `None` when the agent config
    /// omits `system:`.
    #[serde(default)]
    pub system: Option<String>,
    /// `LlmTransport` provider key (`openai`, `anthropic`, `ollama`,
    /// `openrouter`). Routed to a registered `genai::Client` in
    /// `GenAiTransport`.
    pub provider: String,
    /// Provider-side model identifier.
    pub model: String,
    /// Strictness knobs (ADR 0005). Enforced inside the executor.
    pub policy: AgentPolicy,
    /// Tool allowlist. Empty in Phase 4; non-empty rejected with
    /// `NodeError::UnsupportedYet` until Phase 5 lands tool handlers.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellNodeRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResponse {
    pub node_id: String,
    pub output: serde_json::Value,
}

#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Dispatch a single node. `run_id` is the ULID-prefixed run
    /// identifier (ADR 0019) — executors that emit their own events
    /// (`AgentExecutor` and the planned `SubagentHandler`) need it to
    /// scope envelopes; shell and other non-emitting executors
    /// ignore it.
    async fn execute(
        &self,
        run_id: &str,
        id: &str,
        req: NodeRequest,
    ) -> Result<NodeResponse, NodeError>;
}

/// Translate `NodeKind` → `NodeRequest` with every user-supplied
/// template field rendered through `TemplateEngine` in `ctx`, and
/// every agent-config field materialized from `agents`.
///
/// Shell: renders `command` and every entry in `args`.
/// Agent: renders `initial_prompt` and the agent's `system` prompt,
/// then bundles provider, model, policy, and `allowed_tools` onto the
/// request so the executor is self-contained at dispatch time.
///
/// Errors:
/// - `PipelineError::UnknownAgent` if the node references a name not
///   present in `agents`.
/// - `PipelineError::Template` if any field fails to render.
pub fn render_request(
    kind: &NodeKind,
    tmpl: &TemplateEngine,
    ctx: &Context,
    agents: &BTreeMap<String, AgentConfig>,
) -> Result<NodeRequest, PipelineError> {
    let ctx_json = ctx.snapshot_for_template();
    match kind {
        NodeKind::Agent(n) => {
            let cfg = agents
                .get(&n.agent)
                .ok_or_else(|| PipelineError::UnknownAgent {
                    name: n.agent.clone(),
                })?;
            let initial_prompt =
                tmpl.render("agent.initial_prompt", &n.initial_prompt, &ctx_json)?;
            let system = match cfg.system.as_deref() {
                Some(s) => Some(tmpl.render("agent.system", s, &ctx_json)?),
                None => None,
            };
            Ok(NodeRequest::Agent(AgentNodeRequest {
                agent: n.agent.clone(),
                initial_prompt,
                system,
                provider: cfg.provider.clone(),
                model: cfg.model.clone(),
                policy: cfg.policy.clone(),
                allowed_tools: cfg.allowed_tools.clone(),
            }))
        }
        NodeKind::Shell(n) => {
            let command = tmpl.render("shell.command", &n.command, &ctx_json)?;
            let mut args = Vec::with_capacity(n.args.len());
            for (i, arg) in n.args.iter().enumerate() {
                args.push(tmpl.render(&format!("shell.args[{i}]"), arg, &ctx_json)?);
            }
            Ok(NodeRequest::Shell(ShellNodeRequest { command, args }))
        }
    }
}

/// Registry lookup key for a `NodeKind`. Matches the
/// `#[serde(rename_all = "snake_case")]` tag strings on `NodeKind`.
#[must_use]
pub fn kind_str(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent(_) => "agent",
        NodeKind::Shell(_) => "shell",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::pipeline::schema::{AgentNode, OnParseError, ShellNode};

    fn default_policy() -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1000,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            allow_mutations: false,
            allow_network: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
        }
    }

    #[test]
    fn kind_str_matches_serde_tag() {
        let agent = NodeKind::Agent(AgentNode {
            agent: "x".into(),
            initial_prompt: String::new(),
        });
        let shell = NodeKind::Shell(ShellNode {
            command: "true".into(),
            args: Vec::new(),
        });
        assert_eq!(kind_str(&agent), "agent");
        assert_eq!(kind_str(&shell), "shell");
    }

    #[test]
    fn render_request_resolves_agent_config() {
        let mut agents = BTreeMap::new();
        agents.insert(
            "writer".to_string(),
            AgentConfig {
                model: "gpt-5".into(),
                provider: "openai".into(),
                system: Some("be brief".into()),
                allowed_tools: Vec::new(),
                policy: default_policy(),
            },
        );
        let kind = NodeKind::Agent(AgentNode {
            agent: "writer".into(),
            initial_prompt: "hi".into(),
        });
        let tmpl = TemplateEngine::new();
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());

        let NodeRequest::Agent(req) = render_request(&kind, &tmpl, &ctx, &agents).unwrap() else {
            panic!("expected NodeRequest::Agent");
        };

        assert_eq!(req.agent, "writer");
        assert_eq!(req.provider, "openai");
        assert_eq!(req.model, "gpt-5");
        assert_eq!(req.initial_prompt, "hi");
        assert_eq!(req.system.as_deref(), Some("be brief"));
        assert_eq!(req.policy.max_iterations, 1);
    }

    #[test]
    fn render_request_unknown_agent_errors() {
        let agents: BTreeMap<String, AgentConfig> = BTreeMap::new();
        let kind = NodeKind::Agent(AgentNode {
            agent: "missing".into(),
            initial_prompt: String::new(),
        });
        let tmpl = TemplateEngine::new();
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());

        let err =
            render_request(&kind, &tmpl, &ctx, &agents).expect_err("unknown agent must error");
        match err {
            PipelineError::UnknownAgent { name } => assert_eq!(name, "missing"),
            other => panic!("expected UnknownAgent, got {other:?}"),
        }
    }

    #[test]
    fn render_request_renders_shell_fields() {
        let agents: BTreeMap<String, AgentConfig> = BTreeMap::new();
        let kind = NodeKind::Shell(ShellNode {
            command: "echo".into(),
            args: vec!["hi".into(), "there".into()],
        });
        let tmpl = TemplateEngine::new();
        let ctx = Context::new(BTreeMap::new(), BTreeMap::new(), BTreeMap::new());

        let NodeRequest::Shell(req) = render_request(&kind, &tmpl, &ctx, &agents).unwrap() else {
            panic!("expected NodeRequest::Shell");
        };
        assert_eq!(req.command, "echo");
        assert_eq!(req.args, vec!["hi".to_string(), "there".to_string()]);
    }
}
