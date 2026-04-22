//! Tool-handler seam (ADR 0008). Every agent-callable tool routes through
//! a `ToolHandler` impl. Policy gates (`allow_mutations`, `allow_network`,
//! domain lists) run in `LoopAgent` before the handler sees the call —
//! handlers assume they are already cleared to act.

pub mod bash;
pub mod edit;
pub mod read;
pub mod subagent;
pub mod web_fetch;
pub mod write;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ToolError;

pub use bash::BashHandler;
pub use edit::EditHandler;
pub use read::ReadHandler;
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
}

/// Per-call context threaded from `LoopAgent` into `ToolHandler::invoke`.
/// Bundled into a struct so the handler surface stays below the project's
/// four-parameter threshold and so additions (tracing spans, cancellation
/// tokens, …) land without churning every handler signature.
///
/// `SubagentHandler` uses `depth` to bound recursion (ADR 0006) and
/// `run_id` / `node_id` to emit `SubagentStarted` / `SubagentCompleted`
/// events on the shared sink. Builtin handlers typically ignore every
/// field but `call_id`.
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
}

// Explicit `'a` on the impl binds the `&'a str` parameter in `for_test`
// to the `Self` return type. Eliding to `impl ToolInvocation<'_>` would
// sever that link, so the explicit form stays with a targeted allow.
#[allow(clippy::elidable_lifetime_names)]
impl<'a> ToolInvocation<'a> {
    /// Shorthand used by handler unit tests that do not care about run
    /// / node identity. Keeps test bodies from repeating the same four
    /// placeholder fields in every assertion.
    #[cfg(test)]
    pub(crate) fn for_test(call_id: &'a str) -> Self {
        Self {
            run_id: "run_test",
            node_id: "n",
            call_id,
            depth: 0,
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
