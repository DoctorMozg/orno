//! Lifecycle event variants.
//!
//! Split out of `mod.rs` so the enum can grow without pushing the
//! module file past the 300-LOC soft cap. The enum is the only public
//! surface here; re-exported from the parent module so consumer paths
//! stay `crate::events::Event`.

use serde::{Deserialize, Serialize};

use super::{LlmFailure, NodeFailure, SkipReason, Usage};

/// Lifecycle events emitted by the execution engine. Stays append-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    RunStarted {
        run_id: String,
    },
    NodeStarted {
        run_id: String,
        node_id: String,
        /// Discriminator string mirroring `NodeKind`'s serde tag
        /// (`"agent"` or `"shell"` today). Lets downstream consumers
        /// filter on kind without re-reading the pipeline YAML. Added
        /// in `schema_version: 2`.
        node_kind: String,
    },
    NodeFinished {
        run_id: String,
        node_id: String,
        /// Discriminator string mirroring `NodeKind`'s serde tag.
        /// Added in `schema_version: 2`.
        node_kind: String,
        ok: bool,
        /// Populated only when `ok: false`; encodes *why* the node
        /// failed so downstream tools (UIs, CI annotations, log
        /// pipelines) can surface a cause without parsing stderr.
        /// On `ok: true` this is `None` and serialized as `null`.
        /// `#[non_exhaustive]` on `NodeFailure` lets new variants
        /// land non-breakingly.
        failure: Option<NodeFailure>,
    },
    NodeSkipped {
        run_id: String,
        node_id: String,
        /// Discriminator string mirroring `NodeKind`'s serde tag for
        /// the *skipped* node, not the upstream that failed. Added in
        /// `schema_version: 2`.
        node_kind: String,
        reason: SkipReason,
    },
    BudgetExceeded {
        run_id: String,
        reason: String,
    },
    /// Emitted at the start of each agent iteration before the LLM
    /// transport is called. `iteration` is 0-based so a single-shot
    /// agent emits `iteration: 0`.
    AgentIterationStarted {
        run_id: String,
        node_id: String,
        /// Always `"agent"` today (only `kind: agent` nodes drive a
        /// `LoopAgent` iteration). Carried for symmetry with
        /// `NodeStarted` / `NodeFinished` and to keep filters
        /// uniform across the per-node and per-iteration envelopes.
        /// Added in `schema_version: 2`.
        node_kind: String,
        iteration: u32,
    },
    /// Emitted after each successful or denied tool call within an
    /// agent iteration. `input_excerpt` and `output_excerpt` are
    /// redacted and head-truncated at `body_excerpt_max_bytes` (same
    /// cap as `LlmRequestStarted` excerpts). On a denied
    /// call the `output_excerpt` carries the denial reason string.
    ToolCallRecorded {
        run_id: String,
        node_id: String,
        tool_name: String,
        call_id: String,
        input_excerpt: String,
        output_excerpt: String,
    },
    /// Emitted immediately before the transport is called. Carries
    /// provider + model identifiers plus redacted head excerpts of the
    /// rendered prompt and optional system prompt. The excerpts are
    /// passed through the per-run `Redactor` so rendered `secrets.*`
    /// values never reach the wire, and bounded by the engine's
    /// `max_output_bytes` so a megabyte-long prompt
    /// does not flood the event log — the same cap used for
    /// `LlmFailure::ApiError.body_excerpt` and shell stderr tails.
    /// `system_excerpt` is `None` when the agent config declared no
    /// system prompt, distinct from an empty string.
    LlmRequestStarted {
        run_id: String,
        node_id: String,
        provider: String,
        model: String,
        prompt_excerpt: String,
        system_excerpt: Option<String>,
    },
    /// Emitted immediately after a successful transport call. Carries
    /// the normalized `finish_reason`, token usage, and a redacted
    /// head excerpt of the model's response so downstream tools can
    /// surface what the model actually produced without folding the
    /// unbounded `NodeResponse.output` payload. Excerpt redaction and
    /// truncation follow the same rules as `LlmRequestStarted`.
    LlmResponseReceived {
        run_id: String,
        node_id: String,
        finish_reason: Option<String>,
        usage: Option<Usage>,
        content_excerpt: String,
    },
    /// Emitted when the transport call returned `Err` — paired with the
    /// preceding `LlmRequestStarted` so log pipelines can detect a
    /// dangling request without reconstructing it from a downstream
    /// `NodeFinished.failure`. Carries a typed `LlmFailure` so
    /// alerting can fire on auth or rate-limit classes specifically,
    /// not on the generic `ExecutorError` blob.
    LlmRequestFailed {
        run_id: String,
        node_id: String,
        provider: String,
        model: String,
        failure: LlmFailure,
    },
    /// Emitted after the last node settles. `failed_nodes` and
    /// `skipped_nodes` echo the per-node events in causal order so a
    /// single tail-line read of the stream summarizes the run's
    /// failure footprint without folding the full envelope log
    /// log. Both vectors are empty on a fully-green run.
    RunFinished {
        run_id: String,
        ok: bool,
        failed_nodes: Vec<String>,
        skipped_nodes: Vec<String>,
    },
    /// Emitted at the start of a subagent dispatch, before the child
    /// `LoopAgent::run` is entered. `parent_node_id` is the
    /// DAG node the caller is bound to; the child inherits it for its
    /// own event stream so a consumer filtering by `node_id` sees every
    /// turn the tree produced. `depth` is the child's depth
    /// (`caller_depth + 1`).
    SubagentStarted {
        run_id: String,
        parent_node_id: String,
        child_agent: String,
        depth: u32,
    },
    /// Emitted when a subagent dispatch returned successfully. Carries
    /// the child's final iteration count and cumulative token usage so
    /// the parent's audit trail records what the child loop cost
    /// without folding the child's `AgentOutput` into the wire format.
    SubagentCompleted {
        run_id: String,
        parent_node_id: String,
        child_agent: String,
        depth: u32,
        iterations: u32,
        total_tokens: u64,
    },
    /// Emitted when a subagent dispatch returned `AgentError`. The error
    /// is rendered with the full `Display` chain (`{:#}`) so downstream
    /// consumers see the cause without a follow-up query. The parent
    /// loop still feeds the failure back to its LLM as a denial-style
    /// `ToolResult` string under the effects strictness dimension; this event records the
    /// structured observability trail for the failure itself.
    SubagentFailed {
        run_id: String,
        parent_node_id: String,
        child_agent: String,
        depth: u32,
        error: String,
    },
    /// Emitted when the parent agent attempted a subagent dispatch that
    /// would exceed `AgentPolicy.max_subagent_depth`. The child is never
    /// entered; the parent's loop receives a denial-style `ToolResult`
    /// string and continues.
    SubagentDepthExceeded {
        run_id: String,
        parent_node_id: String,
        attempted_child_agent: String,
        depth_attempted: u32,
        max_depth: u32,
    },
    /// Emitted when a tool call was denied by the policy gate
    /// (`allow_mutations`, `allow_network`, `allow_context_writes`,
    /// or domain allow/block lists). Effect-based denials are
    /// non-terminal — the loop feeds a denial string back to the
    /// model as the tool's `ToolResult`; this envelope records the
    /// structured observability trail for the gate firing. Paired
    /// with a `ToolCallRecorded` envelope whose `output_excerpt`
    /// carries the same denial string.
    ToolDenied {
        run_id: String,
        node_id: String,
        tool_name: String,
        reason: String,
    },
    /// Emitted when a node's wall-clock budget (`Node.timeout`)
    /// elapsed before the executor returned. Paired with the
    /// matching `NodeFinished { failure: Some(NodeFailure::TimedOut)
    /// }` so downstream tools can surface the limit and the actual
    /// elapsed time. `limit_secs` echoes the declared budget;
    /// `elapsed_ms` is the measured wall-clock time to cancellation.
    NodeTimedOut {
        run_id: String,
        node_id: String,
        limit_secs: u64,
        elapsed_ms: u64,
    },
    /// Emitted before spawning an MCP server declared in
    /// `Pipeline.mcp_servers`. `transport` is `"stdio"` or `"http"`.
    McpServerStarting {
        run_id: String,
        server: String,
        transport: String,
    },
    /// Emitted after MCP `initialize` + `tools/list` complete.
    /// `tool_count` is the number of tools the server advertised.
    McpServerHandshaked {
        run_id: String,
        server: String,
        tool_count: u32,
    },
    /// Emitted immediately before a `tools/call` is issued. `input_excerpt`
    /// is the JSON arguments, redacted and head-truncated to the engine's
    /// `max_output_bytes` cap.
    McpToolCallSent {
        run_id: String,
        node_id: String,
        server: String,
        tool: String,
        call_id: String,
        input_excerpt: String,
    },
    /// Emitted immediately after a `tools/call` returns. `ok` discriminates
    /// MCP-protocol-reported tool errors from successes. `output_excerpt`
    /// is the content, redacted and head-truncated.
    McpToolCallCompleted {
        run_id: String,
        node_id: String,
        server: String,
        tool: String,
        call_id: String,
        ok: bool,
        output_excerpt: String,
    },
    /// Emitted before initiating a clean shutdown at run end.
    McpServerShuttingDown {
        run_id: String,
        server: String,
    },
    /// Emitted after a clean shutdown completed (exit status on stdio,
    /// connection close on http).
    McpServerExited {
        run_id: String,
        server: String,
    },
    /// Emitted when the server crashed mid-run. The owning agent
    /// terminates with a tool-call failure.
    McpServerCrashed {
        run_id: String,
        server: String,
        reason: String,
    },
    /// Emitted when a `kind: shell` node produced more bytes on
    /// `stdout` or `stderr` than the engine's
    /// `max_node_output_bytes` cap allows. The captured payload is
    /// head-truncated to the cap and the child continues running so it
    /// does not block on a full PIPE buffer; this envelope records
    /// that the captured stream is incomplete. `stream` is `"stdout"`
    /// or `"stderr"`. `captured_bytes` reports the number of bytes
    /// the child actually wrote past the cap before exiting (so an
    /// observer sees how badly the cap was exceeded). `cap_bytes`
    /// echoes the engine's cap so a single envelope tells the full
    /// story without cross-referencing the run config. Added in
    /// `schema_version: 3`.
    NodeOutputTruncated {
        run_id: String,
        node_id: String,
        /// Discriminator string mirroring `NodeKind`'s serde tag.
        /// Always `"shell"` today (only shell nodes capture
        /// subprocess output); carried for symmetry with the rest of
        /// the per-node envelopes.
        node_kind: String,
        /// `"stdout"` or `"stderr"`.
        stream: String,
        /// Total bytes the child wrote on this stream before exiting.
        /// Always `>= cap_bytes` when this event fires.
        captured_bytes: u64,
        /// The cap that fired. Echoes
        /// `EngineConfig.max_node_output_bytes` at the time of the
        /// run so a downstream tool can render the breach without
        /// re-reading the run config.
        cap_bytes: u64,
    },
}
