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
    Llm(#[from] LlmError),
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
