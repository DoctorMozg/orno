//! Tool-handler seam. Every agent-callable tool routes through
//! a `ToolHandler` impl. Policy gates (`allow_mutations`, `allow_network`,
//! domain lists) run in `LoopAgent` before the handler sees the call —
//! handlers assume they are already cleared to act.

pub mod bash;
pub mod edit;
mod fs_util;
pub mod mcp_handler;
pub mod net_guard;
mod path_guard;
pub mod read;
pub mod record;
pub mod replay;
pub mod set_state;
pub mod subagent;
pub mod web_fetch;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::agent::BudgetScope;
use crate::error::ToolError;
use crate::events::Redactor;
use crate::pipeline::template::RenderScope;
use net_guard::DomainRules;

pub use bash::BashHandler;
pub use edit::EditHandler;
pub use mcp_handler::{McpToolHandler, McpToolHandlerConfig, ReplayMcpStubHandler};
pub use read::ReadHandler;
pub use record::{RecordingToolHandler, ToolTapeEntry};
pub use replay::ReplayToolHandler;
pub use set_state::SetStateHandler;
pub use subagent::SubagentHandler;
pub use web_fetch::WebFetchHandler;
pub use write::WriteHandler;

/// Declared effect class for a tool handler. Used by `LoopAgent` to
/// gate tool calls against `AgentPolicy` (bounded effects) and by
/// `orno plan` for static analysis. Enforcement lives in `LoopAgent`,
/// not in the handler itself.
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
    /// Mutates `nodes.<self>.state.*` via the `SetState` builtin.
    /// Requires `allow_context_writes`. Does not imply `Mutations` —
    /// external side effects (fs/process) still require that flag.
    /// Confined to the current node.
    ContextSelf,
}

impl ToolEffect {
    /// True when the effect needs `AgentPolicy.allow_mutations`.
    #[must_use]
    pub fn requires_mutations(self) -> bool {
        matches!(self, Self::Mutations | Self::MutationsAndNetwork)
    }

    /// True when the effect needs `AgentPolicy.allow_network`.
    #[must_use]
    pub fn requires_network(self) -> bool {
        matches!(self, Self::Network | Self::MutationsAndNetwork)
    }

    /// True when the effect needs `AgentPolicy.allow_context_writes`.
    #[must_use]
    pub fn requires_context_writes(self) -> bool {
        self == Self::ContextSelf
    }
}

/// Every builtin tool name with its declared effect. Pipeline validation
/// reads this table; [`builtin_handlers`] must stay in step with it, which
/// a unit test enforces.
pub const BUILTIN_TOOLS: &[(&str, ToolEffect)] = &[
    ("Bash", ToolEffect::MutationsAndNetwork),
    ("Read", ToolEffect::ReadOnly),
    ("Write", ToolEffect::Mutations),
    ("Edit", ToolEffect::Mutations),
    ("WebFetch", ToolEffect::Network),
    ("SetState", ToolEffect::ContextSelf),
];

/// Declared effect of the builtin named `name`, or `None` when it is not a builtin.
#[must_use]
pub fn builtin_effect(name: &str) -> Option<ToolEffect> {
    BUILTIN_TOOLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, effect)| *effect)
}

/// One handler per [`BUILTIN_TOOLS`] entry. `SetState` shares the run-level
/// redactor so `secrets.*` leaves are scrubbed before state reaches the wire.
#[must_use]
pub fn builtin_handlers(
    redactor: &Arc<Redactor>,
    body_excerpt_max_bytes: usize,
) -> Vec<Arc<dyn ToolHandler>> {
    vec![
        Arc::new(BashHandler),
        Arc::new(ReadHandler),
        Arc::new(WriteHandler),
        Arc::new(EditHandler),
        Arc::new(WebFetchHandler::default()),
        Arc::new(SetStateHandler::new(
            Arc::clone(redactor),
            body_excerpt_max_bytes,
        )),
    ]
}

/// Per-call context threaded from `LoopAgent` into `ToolHandler::invoke`.
/// Bundled into a struct so the handler surface stays below the project's
/// four-parameter threshold and so additions (tracing spans, cancellation
/// tokens, …) land without churning every handler signature.
///
/// `SubagentHandler` uses `depth` to bound recursion and `run_id` /
/// `node_id` to emit `SubagentStarted` / `SubagentCompleted` events on
/// the shared sink. `SetStateHandler` uses `state_handle` to persist
/// its writes without relying on global state. Builtin handlers that
/// care about none of these typically ignore every field but `call_id`.
#[derive(Debug, Clone)]
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
    /// `AgentPolicy.max_subagent_depth`.
    pub depth: u32,
    /// Writable handle to the current node's `state` buffer. `Some` for
    /// every `LoopAgent` dispatch; `SetState` is gated by
    /// `allow_context_writes` before it runs, so other handlers never
    /// reach for it. `None` outside a `LoopAgent` (unit tests).
    pub state_handle: Option<StateHandle<'a>>,
    /// Budget scope of the calling loop. `SubagentHandler` hands it to
    /// the child loop so child and grandchild spend count against every
    /// ancestor's token and tool-call limits. `None` outside a
    /// `LoopAgent` (unit tests).
    pub budget: Option<Arc<BudgetScope>>,
    /// Template scope the calling node was rendered under, used by
    /// `SubagentHandler` to render the child's `system` and `roots`.
    pub render_scope: Option<&'a RenderScope>,
    /// Filesystem roots the file builtins (`Read` / `Write` / `Edit`)
    /// are jailed to. Borrowed from the agent's `AgentPolicy.roots`.
    /// Empty when the calling agent did not declare any root or when
    /// the handler is dispatched outside a `LoopAgent` (unit tests).
    /// File handlers reject paths that escape `roots[0]`; non-file
    /// handlers ignore this field entirely.
    pub roots: &'a [PathBuf],
    /// The calling agent's `allowed_domains` / `blocked_domains`.
    /// `WebFetch` re-applies them to every redirect hop, since the
    /// policy gate only sees the first URL. Empty outside a `LoopAgent`.
    pub domains: DomainRules<'a>,
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
    /// only the agent crate can mint a handle; writes are confined to
    /// `LoopAgent`-dispatched flows.
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
            budget: None,
            render_scope: None,
            roots: &[],
            domains: DomainRules::default(),
        }
    }

    /// Variant of `for_test` for handler tests that need to exercise
    /// the filesystem root-jail. Borrows a caller-owned `roots` slice
    /// so the test can build it from `tempfile::TempDir` paths.
    pub(crate) fn for_test_with_roots(call_id: &'a str, roots: &'a [PathBuf]) -> Self {
        Self {
            run_id: "run_test",
            node_id: "n",
            call_id,
            depth: 0,
            state_handle: None,
            budget: None,
            render_scope: None,
            roots,
            domains: DomainRules::default(),
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
    /// Presented to the LLM via `OrnoChatTool.schema`.
    fn schema(&self) -> Value;

    /// Declared effect class. Used by `LoopAgent` to gate against the
    /// agent's `allow_mutations` / `allow_network` policy before dispatch.
    fn effect(&self) -> ToolEffect;

    /// Returns `true` when this handler must only be invoked under a
    /// non-empty `roots` list. The agent loop refuses to start when any
    /// tool in `allowed_tools` returns `true` here and `policy.roots` is
    /// empty. Override this on every handler that performs filesystem I/O
    /// relative to `roots`; the default of `false` keeps third-party
    /// handlers compatible (BS3).
    fn requires_jail(&self) -> bool {
        false
    }

    /// Execute the tool with the per-call `inv` context and JSON `args`.
    /// Returns the tool output as a plain string (forwarded as a
    /// `ToolResult` message in the next LLM turn).
    ///
    /// Must not check policy — callers are already cleared to call this.
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError>;
}

#[cfg(test)]
mod builtin_table_tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn handlers_match_the_builtin_table() {
        let redactor = Arc::new(Redactor::new(&BTreeMap::new()));
        let handlers = builtin_handlers(&redactor, 1024);
        let actual: Vec<(&str, ToolEffect)> =
            handlers.iter().map(|h| (h.name(), h.effect())).collect();
        assert_eq!(actual, BUILTIN_TOOLS);
    }
}
