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

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::NodeError;
use crate::execution::context::Context;
use crate::pipeline::NodeKind;
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
/// `Pipeline.agents` before dispatch and materializes the policy + tool
/// allowlist here. Templated fields (`initial_prompt`, `system`) are
/// already rendered by the time they reach this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNodeRequest {
    pub agent: String,
    pub initial_prompt: String,
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
    async fn execute(&self, id: &str, req: NodeRequest) -> Result<NodeResponse, NodeError>;
}

/// Non-rendered translation from YAML-facing `NodeKind` into the
/// executor-facing `NodeRequest`. Used by tests and by the
/// rendering helper below. In the engine path, prefer `render_request`
/// which applies template context before wrapping.
#[must_use]
pub fn from_kind(kind: &NodeKind) -> NodeRequest {
    match kind {
        NodeKind::Agent(n) => NodeRequest::Agent(AgentNodeRequest {
            agent: n.agent.clone(),
            initial_prompt: n.initial_prompt.clone(),
        }),
        NodeKind::Shell(n) => NodeRequest::Shell(ShellNodeRequest {
            command: n.command.clone(),
            args: n.args.clone(),
        }),
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

/// Translate `NodeKind` → `NodeRequest` with every user-supplied
/// template field rendered through `TemplateEngine` in `ctx`.
/// Shell: renders `command` and every entry in `args`.
/// Agent: renders `initial_prompt`. (The `agent:` name field is
/// a schema reference, not a template.)
pub fn render_request(
    kind: &NodeKind,
    tmpl: &TemplateEngine,
    ctx: &Context,
) -> Result<NodeRequest, crate::error::PipelineError> {
    let ctx_json = ctx.snapshot_for_template();
    match kind {
        NodeKind::Agent(n) => {
            let initial_prompt =
                tmpl.render("agent.initial_prompt", &n.initial_prompt, &ctx_json)?;
            Ok(NodeRequest::Agent(AgentNodeRequest {
                agent: n.agent.clone(),
                initial_prompt,
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::pipeline::schema::{AgentNode, ShellNode};

    #[test]
    fn kind_str_matches_serde_tag() {
        // Registry lookup uses the same lowercase snake-case tag that serde
        // emits on NodeKind. If serde_rename changes, this test fails loudly.
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
    fn from_kind_preserves_shell_fields() {
        let kind = NodeKind::Shell(ShellNode {
            command: "echo".into(),
            args: vec!["hi".into(), "there".into()],
        });
        let NodeRequest::Shell(req) = from_kind(&kind) else {
            panic!("expected NodeRequest::Shell");
        };
        assert_eq!(req.command, "echo");
        assert_eq!(req.args, vec!["hi".to_string(), "there".to_string()]);
    }

    #[test]
    fn from_kind_preserves_agent_fields() {
        let kind = NodeKind::Agent(AgentNode {
            agent: "writer".into(),
            initial_prompt: "hello".into(),
        });
        let NodeRequest::Agent(req) = from_kind(&kind) else {
            panic!("expected NodeRequest::Agent");
        };
        assert_eq!(req.agent, "writer");
        assert_eq!(req.initial_prompt, "hello");
    }
}
