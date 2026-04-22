//! Agent-loop seam (ADR 0005). Every `kind: agent` node routes through
//! an [`Agent`] implementation. The skeleton ships one impl,
//! [`LoopAgent`], which drives the bounded iteration loop with tool
//! dispatch and full five-dimension enforcement.
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

pub use loop_agent::{LoopAgent, LoopAgentConfig};

/// Input to an [`Agent`] call. Structurally mirrors
/// `crate::node::AgentNodeRequest` but is independent of `NodeRequest`
/// so the seam is not coupled to node dispatch — a future embedder can
/// drive `Agent` directly without constructing a `NodeRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRequest {
    /// Named reference from the pipeline's `agents:` map. Kept for
    /// diagnostics and subagent-event correlation.
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
    /// Tool allowlist. Each name must match a `ToolHandler` registered
    /// on the driving [`LoopAgent`]; an unknown name terminates with
    /// [`AgentError::UnknownToolCalled`].
    pub allowed_tools: Vec<String>,
    /// Subagent recursion depth this request executes at. The root
    /// `kind: agent` node runs at `0`; a subagent tool call entered
    /// from a depth `N` agent dispatches the child at `N + 1`. Bounded
    /// by `policy.max_subagent_depth` (ADR 0006). Serialized with a
    /// default so old in-memory construction sites stay source-compatible.
    #[serde(default)]
    pub depth: u32,
}

/// Successful agent-loop output. Returns the LLM's terminal text
/// answer, finish reason, and token usage from the final iteration.
/// Intermediate tool-call turns are recorded as events; this struct
/// carries only the final observable answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    pub content: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
    /// Number of agent loop iterations that ran before returning. Starts
    /// at `0` for a single-shot call that returned a text answer on the
    /// first LLM turn. Used by `SubagentCompleted` events so a parent
    /// agent can surface the child's loop cost without reparsing the
    /// event stream.
    #[serde(default)]
    pub iterations: u32,
    /// Cumulative token usage summed across every LLM turn in this
    /// run. Zero when the transport did not report usage. Carried for
    /// the same subagent-observability reason as `iterations`.
    #[serde(default)]
    pub total_tokens: u64,
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
