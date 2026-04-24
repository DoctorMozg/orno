//! Tool-handler seam (ADR 0008). Every agent-callable tool routes through
//! a `ToolHandler` impl. Policy gates (`allow_mutations`, `allow_network`,
//! domain lists) run in `LoopAgent` before the handler sees the call —
//! handlers assume they are already cleared to act.

pub mod bash;
pub mod edit;
pub mod mcp_handler;
pub mod read;
pub mod recording;
pub mod set_state;
pub mod subagent;
pub mod web_fetch;
pub mod write;

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;

pub use bash::BashHandler;
pub use edit::EditHandler;
pub use mcp_handler::{McpToolHandler, McpToolHandlerConfig, ReplayMcpStubHandler};
pub use read::ReadHandler;
pub use recording::{RecordingToolHandler, ReplayToolHandler, ToolTapeEntry};
pub use set_state::SetStateHandler;
pub use subagent::SubagentHandler;
pub use web_fetch::WebFetchHandler;
pub use write::WriteHandler;

/// Declared effect class for a tool handler. Used by `LoopAgent` to
/// gate tool calls against `AgentPolicy` (ADR 0005 §3) and by future
/// `orno plan` tooling for static analysis. Enforcement lives in
/// `LoopAgent`, not in the handler itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolEffect {
    /// Read-only. No side effects on the file system or network.
    ReadOnly,
    /// Modifies local state (files, processes). Requires `allow_mutations`.
    Mutations,
    /// Issues network requests. Requires `allow_network` + domain checks.
    Network,
    /// Both mutations and network. Requires both policies.
    MutationsAndNetwork,
    /// Mutates `nodes.<self>.state.*` via the `SetState` builtin (ADR
    /// 0025). Requires `allow_context_writes`. Does not imply
    /// `Mutations` — external side effects (fs/process) still require
    /// that flag. Confined to the current node; cross-node state is
    /// ADR 0026's territory.
    ContextSelf,
}

/// Per-call context threaded from `LoopAgent` into `ToolHandler::invoke`.
/// Bundled into a struct so the handler surface stays below the project's
/// four-parameter threshold and so additions (tracing spans, cancellation
/// tokens, …) land without churning every handler signature.
///
/// `SubagentHandler` uses `depth` to bound recursion (ADR 0006) and
/// `run_id` / `node_id` to emit `SubagentStarted` / `SubagentCompleted`
/// events on the shared sink. `SetStateHandler` (ADR 0025) uses
/// `state_handle` to persist its writes without relying on global
/// state. Builtin handlers that care about none of these typically
/// ignore every field but `call_id`.
#[derive(Debug, Clone, Copy)]
pub struct ToolInvocation<'a> {
    /// Run identifier the parent agent is executing under (`run_<ULID>`).
    pub run_id: &'a str,
    /// DAG node id the parent agent is bound to.
    pub node_id: &'a str,
    /// `call_id` from the LLM's tool-call turn. Used to pair the tool's
    /// `ToolResult` message in the next assistant turn.
    pub call_id: &'a str,
    /// Subagent recursion depth of the caller. The root `kind: agent`
    /// node executes at depth `0`. A subagent call entered from a depth
    /// `N` agent dispatches the child at depth `N + 1`, bounded by
    /// `AgentPolicy.max_subagent_depth` (ADR 0006).
    pub depth: u32,
    /// Writable handle to the current node's `state` buffer (ADR 0025).
    /// `Some` only when the node's `allow_context_writes` policy is set
    /// and the handler is dispatched through `LoopAgent`. `SetState`
    /// reaches for this; every other handler leaves it alone.
    pub state_handle: Option<StateHandle<'a>>,
}

/// Borrow-scoped pointer to a single node's `state` buffer. Held for
/// the duration of a tool dispatch and released immediately after.
/// Created by `LoopAgent`; constructed nowhere else so the buffer
/// lifetime is controlled at one point in the crate.
///
/// The underlying `Mutex` is `std::sync::Mutex` rather than
/// `tokio::sync::Mutex` because the lock is taken only for the synchronous
/// read-modify-write inside `SetStateHandler::invoke` — never held across
/// an `.await`.
#[derive(Debug, Clone, Copy)]
pub struct StateHandle<'a> {
    pub(crate) buf: &'a Mutex<Value>,
}

impl<'a> StateHandle<'a> {
    /// Wrap a borrowed `Mutex<Value>` into a handle. `pub(crate)` so
    /// only the agent crate can mint a handle; ADR 0025 confines writes
    /// to `LoopAgent`-dispatched flows.
    pub(crate) fn new(buf: &'a Mutex<Value>) -> Self {
        Self { buf }
    }
}

#[cfg(test)]
impl<'a> ToolInvocation<'a> {
    /// Shorthand used by handler unit tests that do not care about run
    /// / node identity. Keeps test bodies from repeating the same four
    /// placeholder fields in every assertion.
    pub(crate) fn for_test(call_id: &'a str) -> Self {
        Self {
            run_id: "run_test",
            node_id: "n",
            call_id,
            depth: 0,
            state_handle: None,
        }
    }
}

/// Every agent-callable tool implements this trait. The trait is object-safe
/// and used as `Arc<dyn ToolHandler>` in the executor's dispatch table.
///
/// Handlers must not check policy themselves — `LoopAgent` pre-clears the
/// call before invoking this. The `#[async_trait]` macro is required because
/// `async fn` in traits is not dyn-compatible without it (CLAUDE.md rule).
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Canonical tool name, used as the key in `AgentPolicy.allowed_tools`
    /// and as the `"name"` field in the JSON schema presented to the LLM.
    ///
    /// Returns `&str` (elided lifetime tied to `&self`) rather than
    /// `&'static str` so subagent handlers — whose names are dynamic
    /// `subagent.<agent-name>` strings built at pipeline load — can
    /// return a field reference without leaking a `Box::leak` string.
    fn name(&self) -> &str;

    /// Human-readable description forwarded to the LLM as the tool's
    /// `"description"` field.
    fn description(&self) -> &str;

    /// JSON Schema object describing the arguments this tool accepts.
    /// Presented to the LLM via `OrnoChatTool.schema` (ADR 0008).
    fn schema(&self) -> Value;

    /// Declared effect class. Used by `LoopAgent` to gate against the
    /// agent's `allow_mutations` / `allow_network` policy before dispatch.
    fn effect(&self) -> ToolEffect;

    /// Execute the tool with the per-call `inv` context and JSON `args`.
    /// Returns the tool output as a plain string (forwarded as a
    /// `ToolResult` message in the next LLM turn).
    ///
    /// Must not check policy — callers are already cleared to call this.
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError>;
}
