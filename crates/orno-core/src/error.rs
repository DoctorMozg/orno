//! Error hierarchy. One enum per subsystem, `#[source]` for chaining,
//! `#[from]` only where the conversion is unambiguous.

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    #[error(transparent)]
    Pipeline(#[from] PipelineError),

    #[error(transparent)]
    Node(#[from] NodeError),

    #[error(transparent)]
    Agent(#[from] AgentError),

    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error(transparent)]
    Tool(#[from] ToolError),

    #[error(transparent)]
    Mcp(#[from] McpError),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PipelineError {
    #[error("failed to read pipeline file {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse pipeline YAML")]
    Parse(#[source] serde_yaml_ng::Error),

    #[error("pipeline validation failed: {0}")]
    Validation(String),

    #[error("template render failed for `{name}`")]
    Template {
        name: String,
        #[source]
        source: minijinja::Error,
    },

    #[error("pipeline graph is invalid: {reason}")]
    InvalidGraph { reason: String },

    /// An agent node referenced a name missing from `Pipeline.agents`.
    /// Raised during dispatch, not at load time — `validate()` does not
    /// yet cross-check node → agent references (that is Phase 7 work on
    /// the full `orno validate` policy surface per `docs/roadmap.md`).
    #[error("node references unknown agent `{name}`")]
    UnknownAgent { name: String },
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum NodeError {
    #[error("node `{id}` kind `{kind}` is not registered")]
    UnknownKind { id: String, kind: String },

    #[error("node `{id}` is not implemented yet (skeleton stub)")]
    NotImplemented { id: String },

    /// Raised when a `kind: agent` node carries a configuration that
    /// Phase 4 cannot honor yet — e.g. a non-empty `allowed_tools`
    /// (tools land in Phase 5) or `max_iterations > 1` (the loop body
    /// also lands in Phase 5). Fails fast rather than silently
    /// ignoring the declared policy.
    #[error("node `{id}` uses feature `{feature}` not yet supported")]
    UnsupportedYet { id: String, feature: String },

    #[error("node `{id}` failed")]
    Execution {
        id: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Agent-loop errors surfaced from [`crate::agent::Agent::run`]. Carries
/// the variants enforced by the strictness contract: `PolicyInvalid`,
/// `IterationLimitExceeded`, `UnknownToolCalled`, `BudgetExceeded`, and
/// `ParseFailed`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AgentError {
    /// The agent policy is internally inconsistent (e.g. `max_iterations`
    /// is zero). Raised before any LLM call so the operator sees a
    /// configuration error rather than a mid-run transport failure.
    #[error("invalid agent policy: {0}")]
    InvalidPolicy(String),

    /// A policy feature is declared but not yet honored in this phase
    /// (e.g. non-empty `allowed_tools` before Phase 5 wires tool
    /// handlers). Fails fast so strict pipelines never silently lose
    /// policy.
    #[error("agent uses feature `{0}` not yet supported")]
    UnsupportedYet(String),

    /// Agent exhausted `max_iterations` without reaching a `stop`
    /// finish reason.
    #[error("agent exceeded max_iterations ({max})")]
    IterationLimitExceeded { max: u32 },

    /// The LLM called a tool not listed in `allowed_tools` or not
    /// registered in the executor's handler map.
    #[error("agent called unknown tool `{name}`")]
    UnknownToolCalled { name: String },

    /// A running budget dimension was exceeded. `kind` is from
    /// `crate::events::BudgetKind` so the wire format shares the
    /// classification type between the error surface and the event
    /// surface.
    #[error("agent exceeded budget: {kind:?}")]
    BudgetExceeded { kind: crate::events::BudgetKind },

    /// Subagent recursion exceeded `max_subagent_depth`.
    #[error("subagent depth exceeded ({max})")]
    SubagentDepthExceeded { max: u32 },

    /// A tool's arguments failed to parse and `on_parse_error: fail`
    /// was set.
    #[error("tool `{tool}` arguments failed to parse: {error}")]
    ParseFailed { tool: String, error: String },

    /// A tool handler returned an error. `name` is the tool name for
    /// context; `source` is the underlying `ToolError`.
    #[error("tool `{name}` failed")]
    Tool {
        name: String,
        #[source]
        source: ToolError,
    },

    /// LLM transport returned a typed error. Use `source()` to reach the
    /// underlying `LlmError` for classification.
    #[error(transparent)]
    Llm(#[from] LlmError),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LlmError {
    #[error("LLM transport is not wired yet (skeleton stub)")]
    NotImplemented,

    #[error("LLM request rejected: {0}")]
    Rejected(String),

    /// Provider authentication failed (HTTP 401 or 403). The API key
    /// is either missing, invalid, or unauthorized for the requested
    /// model. Maps from `genai::Error::HttpError` on 4xx auth codes
    /// and from `RequiresApiKey` / `NoAuthData` pre-flight failures.
    #[error("authentication failed for provider `{provider}`")]
    AuthFailed { provider: String },

    /// Provider rate-limited the request (HTTP 429). Callers may want
    /// to retry; v0.1 does not retry automatically.
    #[error("provider `{provider}` rate-limited the request")]
    RateLimited { provider: String },

    /// The requested model is not available on the provider (HTTP 404
    /// on the chat endpoint). Usually a typo in the pipeline YAML.
    #[error("model `{model}` is not available on provider `{provider}`")]
    ModelNotFound { provider: String, model: String },

    /// Generic API error — any non-auth, non-rate-limit, non-404 HTTP
    /// failure. Carries status code and body so the operator can
    /// diagnose without re-running with debug tracing.
    #[error("provider `{provider}` returned HTTP {status}: {body}")]
    ApiError {
        provider: String,
        status: u16,
        body: String,
    },

    /// Network-level failure, timeout, or any non-HTTP transport
    /// problem surfaced by the underlying client.
    #[error("transport error calling LLM")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// Misconfiguration caught before any network call — unknown
    /// provider key, missing API-key env var, malformed endpoint URL.
    #[error("LLM configuration error: {0}")]
    ConfigError(String),

    /// The provider returned a response that could not be parsed.
    /// Usually a genai adapter problem rather than an orno bug.
    #[error("failed to parse LLM response: {0}")]
    ParseError(String),

    /// Replay tape has no entry for the requested key. Indicates the
    /// caller is running against a tape recorded from a different
    /// pipeline, or that the tape is incomplete. `ReplayTransport`
    /// never falls through to a live call.
    #[error("replay tape miss for key `{key}`")]
    ReplayMiss { key: String },
}

/// Errors surfaced from [`crate::mcp::McpClient`] implementations.
/// One variant per discernible cause so downstream consumers can branch
/// without regex-matching the chain. No `rmcp` types appear here —
/// `RmcpClient` maps them at the trait boundary.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("failed to spawn MCP server `{server}`")]
    SpawnFailed {
        server: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("MCP handshake failed with server `{server}`")]
    HandshakeFailed {
        server: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("MCP server `{server}` tool call `{tool}` failed")]
    CallFailed {
        server: String,
        tool: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// MCP-protocol-level tool error (server returned `isError: true`).
    /// This is data, not a transport failure — the tool ran but reported
    /// an error payload. Distinct from `CallFailed` which is a wire/transport
    /// failure.
    #[error("MCP server `{server}` tool `{tool}` reported error: {message}")]
    ToolError {
        server: String,
        tool: String,
        message: String,
    },

    #[error("MCP server `{server}` crashed mid-run")]
    ServerCrashed { server: String },

    #[error("MCP transport `{transport}` is not supported in this build")]
    UnsupportedTransport { transport: String },

    #[error("MCP server `{server}` shutdown timed out after {secs}s")]
    ShutdownTimeout { server: String, secs: u64 },

    #[error("MCP server `{server}` does not advertise tool `{tool}`")]
    UnknownTool { server: String, tool: String },
}

/// Errors surfaced from [`crate::tool::ToolHandler::invoke`].
/// Handlers return these; [`AgentError::Tool`] wraps them with
/// the tool name for caller context.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// The underlying I/O or API call failed. `source` carries the
    /// original error chain.
    #[error("tool `{name}` invocation failed")]
    Invocation {
        name: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The arguments JSON could not be deserialized into the expected
    /// shape. Returned before any side effects.
    #[error("tool `{name}` arguments invalid: {message}")]
    InvalidArgs { name: String, message: String },

    /// A `SetState` call would push the serialized size of
    /// `nodes.<self>.state` past `EngineConfig.max_output_bytes`. The
    /// write is rejected and the previous state is left intact. ADR
    /// 0025 §5 — shared cap with stderr tails, API body excerpts, and
    /// prompt/response excerpts so one knob governs every
    /// wire-emitted value.
    #[error("tool `{name}` write rejected: state size {bytes} exceeds cap {cap}")]
    StateTooLarge {
        name: String,
        bytes: usize,
        cap: usize,
    },

    /// This handler is not yet wired (stub). Indicates a Phase 5+
    /// feature that the caller declared but has not been implemented.
    #[error("tool `{name}` not implemented yet: {feature}")]
    NotImplemented { name: String, feature: String },
}
