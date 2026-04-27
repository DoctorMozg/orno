//! `Subagent` tool — delegate a sub-task to a child agent loop.
//! One handler instance is registered per
//! `subagent.<child-agent-name>` entry in the parent agent's
//! `allowed_tools` list. `invoke` recursively dispatches back into the
//! shared [`LoopAgent`] with a child [`AgentRequest`] carrying the
//! child's own provider / model / policy / `allowed_tools` and a depth
//! counter `caller_depth + 1`.
//!
//! The handler holds a `Weak<LoopAgent>` back-pointer to break the
//! `Arc` cycle (`LoopAgent` → tools → `SubagentHandler` → `LoopAgent`).
//! The weak ref is upgraded per call; failure to upgrade means the
//! parent was dropped mid-dispatch, which is a caller bug — we surface
//! it as a `ToolError::Invocation` rather than panicking.
//!
//! Depth enforcement lives in `LoopAgent` (where policy lives), not
//! here — the handler is a single-purpose dispatcher. A parent that
//! gates a subagent call on `max_subagent_depth` emits
//! `Event::SubagentDepthExceeded` and never reaches
//! [`SubagentHandler::invoke`].

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{debug, instrument};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::agent::{Agent, AgentRequest, LoopAgent};
use crate::error::ToolError;
use crate::events::{Event, EventSink};
use crate::pipeline::{AgentConfig, AgentPolicy};

#[derive(Clone)]
pub struct SubagentHandler {
    /// YAML-facing tool name, e.g. `subagent.contributor_vibes`. This
    /// is what `ToolHandler::name()` returns and what appears in a
    /// parent agent's `allowed_tools` list. `LoopAgent` translates to
    /// the wire-safe form (dots → underscores) when presenting the
    /// tool schema to the LLM.
    yaml_name: String,
    /// Name of the child agent — the key in `Pipeline.agents` this
    /// handler delegates to. Carried separately from `yaml_name` so
    /// `Event::SubagentStarted / Completed / Failed` can surface the
    /// raw agent identifier without re-parsing the dotted form.
    child_agent_name: String,
    /// Configuration the child loop runs under. Cloned from
    /// `Pipeline.agents.<child>` at pipeline-load time; the handler
    /// owns its copy so a mid-run pipeline mutation cannot change
    /// the child's policy.
    child_config: AgentConfig,
    /// Effect class derived once from `child_config.policy` at
    /// construction. Pipeline-load compose-down already guarantees
    /// child.policy ≤ parent.policy on both axes, so returning the
    /// child's declared effect — rather than a conservative
    /// `MutationsAndNetwork` union — lets a read-only parent
    /// legitimately delegate to a read-only child.
    declared_effect: ToolEffect,
    /// Weak back-pointer to the parent [`LoopAgent`]. Upgraded per
    /// call. Using `Weak` — rather than `Arc` — breaks the cycle that
    /// would otherwise leak `LoopAgent` forever (the parent's `tools`
    /// vector keeps an `Arc<SubagentHandler>` alive, and an `Arc`
    /// back-pointer would complete the cycle).
    parent: Weak<LoopAgent>,
    /// Shared sink used to emit `Subagent*` lifecycle events so the
    /// parent's audit trail records the child's dispatch without
    /// requiring the child's own run to surface it through
    /// `NodeFinished`.
    sink: Arc<dyn EventSink>,
}

impl std::fmt::Debug for SubagentHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `child_config` and `sink` are intentionally omitted — the
        // first carries the full AgentConfig tree (noise in logs) and
        // the second is a trait-object with no useful `Debug`. The
        // `finish_non_exhaustive` form says so explicitly.
        f.debug_struct("SubagentHandler")
            .field("yaml_name", &self.yaml_name)
            .field("child_agent_name", &self.child_agent_name)
            .field("parent_alive", &(self.parent.strong_count() > 0))
            .finish_non_exhaustive()
    }
}

impl SubagentHandler {
    /// Build a handler that dispatches to the named child agent
    /// through the supplied `LoopAgent`. `yaml_name` must match the
    /// `allowed_tools` entry exactly — the CLI builds it as
    /// `format!("subagent.{child_agent_name}")` so the two forms stay
    /// aligned.
    #[must_use]
    pub fn new(
        yaml_name: String,
        child_agent_name: String,
        child_config: AgentConfig,
        parent: Weak<LoopAgent>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        let declared_effect = effect_from_policy(&child_config.policy);
        Self {
            yaml_name,
            child_agent_name,
            child_config,
            declared_effect,
            parent,
            sink,
        }
    }

    /// Accessor for the child agent's name. Used by `LoopAgent` when
    /// emitting `SubagentDepthExceeded` before the call reaches
    /// `invoke` — the parent already knows the handler by YAML name
    /// and needs the raw agent identifier for the event.
    #[must_use]
    pub fn child_agent_name(&self) -> &str {
        &self.child_agent_name
    }
}

#[async_trait]
impl ToolHandler for SubagentHandler {
    fn name(&self) -> &str {
        &self.yaml_name
    }
    fn description(&self) -> &str {
        // Static phrasing keeps the prompt-surface deterministic. The
        // child agent name is captured in the YAML tool identifier the
        // LLM already sees, so it does not need to be repeated here.
        "Delegate a sub-task to a child agent. Provide `prompt` describing the task."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task to delegate to the sub-agent.",
                }
            },
            "required": ["prompt"]
        })
    }
    fn effect(&self) -> ToolEffect {
        // Derived from the child agent's policy at construction.
        // Pipeline-load compose-down already guarantees child ≤ parent,
        // so the parent's gate never has to over-approximate with
        // `MutationsAndNetwork`.
        self.declared_effect
    }

    #[instrument(
        skip(self, args),
        fields(
            tool.name = %self.yaml_name,
            subagent.child = %self.child_agent_name,
            subagent.depth = inv.depth + 1,
            tool.call_id = %inv.call_id,
        ),
    )]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: self.yaml_name.clone(),
                message: "missing or non-string `prompt`".to_string(),
            })?
            .to_string();

        let parent = self.parent.upgrade().ok_or_else(|| ToolError::Invocation {
            name: self.yaml_name.clone(),
            source: Box::new(std::io::Error::other(
                "parent LoopAgent was dropped before subagent dispatch — this is a construction bug",
            )),
        })?;

        let child_depth = inv.depth + 1;

        self.sink
            .record(Event::SubagentStarted {
                run_id: inv.run_id.to_string(),
                parent_node_id: inv.node_id.to_string(),
                child_agent: self.child_agent_name.clone(),
                depth: child_depth,
            })
            .await;

        let child_req = AgentRequest {
            agent_name: self.child_agent_name.clone(),
            initial_prompt: Arc::from(prompt),
            system: self.child_config.system.as_deref().map(Arc::from),
            provider: Arc::from(self.child_config.provider.as_str()),
            model: Arc::from(self.child_config.model.as_str()),
            policy: self.child_config.policy.clone(),
            allowed_tools: self.child_config.allowed_tools.clone(),
            depth: child_depth,
            // Hand the parent loop's local token counter to the child
            // request so the child's per-LLM-response usage reaches the
            // parent. `token_budget_share` is `Some` for any tool call
            // dispatched through `LoopAgent`; it is `None` only for
            // direct handler unit tests, which never construct a
            // `SubagentHandler` to begin with.
            parent_token_counter: inv.token_budget_share.clone(),
        };

        debug!(
            child_agent = %self.child_agent_name,
            child_depth,
            "entering subagent loop",
        );

        match parent.run(inv.run_id, inv.node_id, child_req).await {
            Ok(output) => {
                self.sink
                    .record(Event::SubagentCompleted {
                        run_id: inv.run_id.to_string(),
                        parent_node_id: inv.node_id.to_string(),
                        child_agent: self.child_agent_name.clone(),
                        depth: child_depth,
                        iterations: output.iterations,
                        total_tokens: output.total_tokens,
                    })
                    .await;
                Ok(output.content)
            },
            Err(err) => {
                let rendered = format!("{err:#}");
                self.sink
                    .record(Event::SubagentFailed {
                        run_id: inv.run_id.to_string(),
                        parent_node_id: inv.node_id.to_string(),
                        child_agent: self.child_agent_name.clone(),
                        depth: child_depth,
                        error: rendered.clone(),
                    })
                    .await;
                // Effect-based denials are non-terminal: a subagent
                // failure is fed back to the parent LLM as a
                // tool-result string. The parent decides whether to
                // retry, adapt, or give up — orno does not pre-decide.
                Ok(format!(
                    "error: subagent `{}` failed: {rendered}",
                    self.child_agent_name,
                ))
            },
        }
    }
}

/// Map a child agent's `(allow_mutations, allow_network)` pair to the
/// `ToolEffect` variant the parent's policy gate should evaluate (ADR
/// 0027). The four-quadrant table is the full contract — any future
/// `AgentPolicy` boolean that gates an effect class must surface here
/// or in a dedicated helper, never silently.
fn effect_from_policy(policy: &AgentPolicy) -> ToolEffect {
    match (policy.allow_mutations, policy.allow_network) {
        (false, false) => ToolEffect::ReadOnly,
        (true, false) => ToolEffect::Mutations,
        (false, true) => ToolEffect::Network,
        (true, true) => ToolEffect::MutationsAndNetwork,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::OnParseError;

    fn policy(allow_mutations: bool, allow_network: bool) -> AgentPolicy {
        AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1_000,
            max_tool_calls: 1,
            max_subagent_depth: 0,
            max_tool_output_bytes: None,
            allow_mutations,
            allow_network,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: OnParseError::Fail,
            roots: Vec::new(),
            max_message_history_bytes: None,
        }
    }

    #[test]
    fn effect_from_policy_four_quadrants() {
        // One row per combination of the two booleans. A new
        // row must be added here before a new `AgentPolicy` flag can
        // widen the effect surface at the subagent boundary.
        let cases: &[(bool, bool, ToolEffect)] = &[
            (false, false, ToolEffect::ReadOnly),
            (true, false, ToolEffect::Mutations),
            (false, true, ToolEffect::Network),
            (true, true, ToolEffect::MutationsAndNetwork),
        ];
        for &(mutations, network, expected) in cases {
            let got = effect_from_policy(&policy(mutations, network));
            assert_eq!(
                got, expected,
                "expected effect {expected:?} for (allow_mutations={mutations}, allow_network={network}), got {got:?}",
            );
        }
    }
}
