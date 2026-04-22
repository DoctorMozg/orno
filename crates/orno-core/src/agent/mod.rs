//! Agent-loop seam (ADR 0005). Every `kind: agent` node routes through
//! an [`Agent`] implementation. Phase 4 ships exactly one impl,
//! [`LoopAgent`], that runs a single round-trip. Phase 5 extends
//! `LoopAgent` with real iteration, tool dispatch, and the full
//! five-dimension enforcement without changing this trait.
//!
//! The seam lives in its own module so `NodeExecutor` stays focused on
//! kind dispatch. `AgentExecutor` (`crate::node::agent`) is the adapter
//! that converts `NodeRequest::Agent` into [`AgentRequest`] and an
//! [`AgentOutput`] back into `NodeResponse` JSON.

pub mod loop_agent;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use crate::llm::Usage;
use crate::pipeline::schema::AgentPolicy;

pub use loop_agent::LoopAgent;

/// Input to an [`Agent`] call. Structurally mirrors
/// `crate::node::AgentNodeRequest` but is independent of `NodeRequest`
/// so the seam is not coupled to node dispatch — a future embedder can
/// drive `Agent` directly without constructing a `NodeRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    /// Named reference from the pipeline's `agents:` map. Kept for
    /// diagnostics and future subagent depth tracking; not load-bearing
    /// for the loop body itself.
    pub agent_name: String,
    /// First user message, already rendered.
    pub initial_prompt: String,
    /// System prompt, already rendered. `None` when the agent config
    /// omits `system:`.
    pub system: Option<String>,
    /// `LlmTransport` provider key.
    pub provider: String,
    /// Provider-side model identifier.
    pub model: String,
    /// Strictness knobs (ADR 0005). Enforced inside the agent impl.
    pub policy: AgentPolicy,
    /// Tool allowlist. Empty in Phase 4; non-empty rejected with
    /// [`AgentError::UnsupportedYet`] until Phase 5 lands tool handlers.
    pub allowed_tools: Vec<String>,
}

/// Successful agent-loop output. The single-shot Phase 4 body returns
/// exactly the triple the LLM gave us; Phase 5 will aggregate across
/// iterations but keep the same outward shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    pub content: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// Agent-loop contract. Every `kind: agent` node executes through an
/// implementation of this trait. The `run_id` / `node_id` pair scopes
/// events the impl emits into the shared `EventSink`.
#[async_trait]
pub trait Agent: Send + Sync {
    async fn run(
        &self,
        run_id: &str,
        node_id: &str,
        req: AgentRequest,
    ) -> Result<AgentOutput, AgentError>;
}
