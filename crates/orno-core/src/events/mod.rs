//! Versioned, append-only event envelope.
//!
//! The `schema_version` on the envelope and `#[non_exhaustive]` on `Event`
//! together let us grow the enum without breaking existing replay files.
//! Every envelope carries an RFC 3339 `timestamp` so log pipelines can
//! correlate events with wall clock without reconstructing from `seq`.

pub mod in_memory_sink;
pub mod redactor;
pub mod sink;
pub mod streaming_sink;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use in_memory_sink::InMemorySink;
pub use redactor::Redactor;
pub use sink::EventSink;
pub use streaming_sink::StreamingSink;

/// Re-export of `llm::Usage` so `LlmResponseReceived` can carry it
/// without cross-module coupling at the event-consumer layer.
pub use crate::llm::Usage;

/// Wire envelope for every event persisted or broadcast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub seq: u64,
    /// UTC wall-clock instant the event was emitted, serialized as RFC
    /// 3339 (`"2026-04-21T15:30:00.123456789Z"`). Distinct from `seq`:
    /// `seq` is a strictly-monotonic emission order, `timestamp` is a
    /// human-readable correlator that makes event streams legible in
    /// logs without a tool to decode them.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub event: Event,
}

impl EventEnvelope {
    /// Build an envelope with the current schema version and a freshly
    /// captured UTC timestamp. The caller owns `seq` because the
    /// scheduler is the only component authoritative for emission
    /// order.
    #[must_use]
    pub fn new(seq: u64, event: Event) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            seq,
            timestamp: OffsetDateTime::now_utc(),
            event,
        }
    }
}

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
    },
    NodeFinished {
        run_id: String,
        node_id: String,
        ok: bool,
        /// Populated only when `ok: false`; encodes *why* the node
        /// failed so downstream tools (UIs, CI annotations, log
        /// pipelines) can surface a cause without parsing stderr.
        /// On `ok: true` this is `None` and serialized as `null`.
        /// `#[non_exhaustive]` on `NodeFailure` lets new variants
        /// land non-breakingly (ADR 0022).
        failure: Option<NodeFailure>,
    },
    NodeSkipped {
        run_id: String,
        node_id: String,
        reason: SkipReason,
    },
    BudgetExceeded {
        run_id: String,
        reason: String,
    },
    /// Emitted immediately before the transport is called. Carries
    /// provider + model identifiers plus redacted head excerpts of the
    /// rendered prompt and optional system prompt (ADR 0024). The
    /// excerpts are passed through the per-run `Redactor` so rendered
    /// `secrets.*` values never reach the wire (ADR 0020), and bounded
    /// by the engine's `max_output_bytes` so a megabyte-long prompt
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
    /// truncation follow the same rules as `LlmRequestStarted`
    /// (ADR 0024).
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
    /// not on the generic `ExecutorError` blob (ADR 0023).
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
    /// (ADR 0023). Both vectors are empty on a fully-green run.
    RunFinished {
        run_id: String,
        ok: bool,
        failed_nodes: Vec<String>,
        skipped_nodes: Vec<String>,
    },
}

/// Why a node never ran. Expanded as new skip cases appear
/// (branch failure, explicit `when:` gate, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkipReason {
    /// An upstream node this node transitively depended on
    /// finished with `ok: false`.
    DependencyFailed { upstream: String },
}

/// Why a node finished with `ok: false`. Carried on
/// `Event::NodeFinished` so downstream consumers see the cause without
/// reconstructing it from stderr or `tracing` JSON. Strict-loop
/// dimensions (`BudgetExceeded`, `IterationLimitExceeded`,
/// `ToolDenied`, …) land here as those subsystems come online
/// (ADR 0022).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeFailure {
    /// No `NodeExecutor` was registered for the node's kind. A
    /// configuration mismatch between the YAML and the embedder's
    /// registry — never a child-process or transport problem.
    /// The field is `node_kind` (not `kind`) because `kind` is the
    /// serde tag discriminator on this enum.
    NoExecutorRegistered { node_kind: String },
    /// `MiniJinja` rendering of the node's request failed (unknown
    /// variable, malformed expression, type mismatch). The full
    /// `anyhow`-style chain is rendered so root cause is visible.
    TemplateRenderFailed { error: String },
    /// The executor returned `Err`. Covers process-spawn failures,
    /// transport errors, and any other pre-output failure path the
    /// executor surfaces. The error chain is rendered with `{:#}`.
    ExecutorError { error: String },
    /// The executor returned `Ok`, but its payload signaled failure
    /// (today, only shell with non-zero `exit_code`). `stderr_tail`
    /// preserves the trailing window of captured stderr bounded by
    /// `EngineConfig.max_output_bytes`; the full payload is also
    /// recorded into the per-run `Context` under `node.<id>.*` so
    /// downstream templates can read the unbounded form.
    NodePayloadFailure {
        exit_code: Option<i64>,
        stderr_tail: Option<String>,
    },
}

/// Why an LLM transport call failed. Carried on
/// `Event::LlmRequestFailed` so downstream alerting can branch on
/// failure class (auth, rate-limit, model-not-found, …) without
/// regex-matching the human-readable error chain on
/// `NodeFailure::ExecutorError`. Mirrors the typed variants of
/// `crate::error::LlmError`; new error kinds get matching variants
/// here, with `Other` as the catch-all so non-matching inputs degrade
/// to a string rather than disappearing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LlmFailure {
    /// HTTP 401/403 or pre-flight `RequiresApiKey` / `NoAuthData`.
    /// Provider name is not duplicated here — the parent event
    /// already carries it.
    AuthFailed,
    /// HTTP 429. Callers may want to retry; v0.1 does not retry.
    RateLimited,
    /// HTTP 404 on the chat endpoint. Usually a model-name typo.
    ModelNotFound,
    /// Any other HTTP failure. `body_excerpt` is bounded by the
    /// engine's `max_output_bytes` (the same cap that bounds shell
    /// stderr tails) so a verbose provider error does not flood the
    /// event log.
    ApiError { status: u16, body_excerpt: String },
    /// Network/timeout/transport problem from the underlying client.
    Transport { error: String },
    /// Pre-flight misconfiguration caught before any network call.
    ConfigError { message: String },
    /// Provider returned a payload the adapter could not parse.
    ParseError { message: String },
    /// Replay tape miss — the caller is running against a tape from a
    /// different pipeline, or the tape is incomplete.
    ReplayMiss { key: String },
    /// Catch-all for legacy `LlmError` variants (`Rejected`,
    /// `NotImplemented`) and any future `#[non_exhaustive]` additions
    /// that have not yet earned a typed wire variant. Carries the
    /// rendered error chain so the cause is not lost.
    Other { message: String },
}

impl LlmFailure {
    /// Classify an `LlmError` into the wire-format `LlmFailure` for
    /// `Event::LlmRequestFailed`. Lives here next to the type so the
    /// classifier evolves with the variant set rather than drifting in
    /// a sibling module. `body_excerpt_max_bytes` bounds the body
    /// captured into `ApiError` — pass the engine's
    /// `EngineConfig.max_output_bytes` so the truncation policy
    /// matches shell stderr tails.
    #[must_use]
    pub fn from_llm_error(err: &crate::error::LlmError, body_excerpt_max_bytes: usize) -> Self {
        use crate::error::LlmError;
        match err {
            LlmError::AuthFailed { .. } => Self::AuthFailed,
            LlmError::RateLimited { .. } => Self::RateLimited,
            LlmError::ModelNotFound { .. } => Self::ModelNotFound,
            LlmError::ApiError { status, body, .. } => Self::ApiError {
                status: *status,
                body_excerpt: truncate_excerpt(body, body_excerpt_max_bytes),
            },
            LlmError::Transport(source) => Self::Transport {
                error: format!("{source:#}"),
            },
            LlmError::ConfigError(message) => Self::ConfigError {
                message: message.clone(),
            },
            LlmError::ParseError(message) => Self::ParseError {
                message: message.clone(),
            },
            LlmError::ReplayMiss { key } => Self::ReplayMiss { key: key.clone() },
            // `Rejected`, `NotImplemented`, and any future non-exhaustive
            // additions land here. Render the full chain so the cause is
            // preserved even without a typed variant yet.
            other => Self::Other {
                message: format!("{other:#}"),
            },
        }
    }
}

/// Keep the leading `max_bytes` of a string on a UTF-8 boundary,
/// suffixed with `"…"` when truncation occurred. HTTP error bodies put
/// the actionable signal at the *front* (status text, JSON
/// `error.message`); truncating from the head would drop exactly that
/// — opposite of stderr tails where the cause sits at the end.
///
/// The same head-retention semantics apply to prompt and response
/// excerpts on `LlmRequestStarted` / `LlmResponseReceived` (ADR 0024):
/// a rendered prompt starts with the operator instruction, and a model
/// response starts with the direct answer — truncating either from
/// the back keeps the part a human would actually read first.
pub(crate) fn truncate_excerpt(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmError;

    #[test]
    fn classifies_auth_rate_limit_and_model_not_found_without_carrying_provider() {
        // Provider is on the parent event already; classification must
        // not duplicate it on the wire.
        assert!(matches!(
            LlmFailure::from_llm_error(
                &LlmError::AuthFailed {
                    provider: "openai".into()
                },
                512,
            ),
            LlmFailure::AuthFailed,
        ));
        assert!(matches!(
            LlmFailure::from_llm_error(
                &LlmError::RateLimited {
                    provider: "openai".into()
                },
                512,
            ),
            LlmFailure::RateLimited,
        ));
        assert!(matches!(
            LlmFailure::from_llm_error(
                &LlmError::ModelNotFound {
                    provider: "openai".into(),
                    model: "gpt-9".into(),
                },
                512,
            ),
            LlmFailure::ModelNotFound,
        ));
    }

    #[test]
    fn api_error_truncates_body_to_configured_cap() {
        // Body excerpts must respect the engine's max_output_bytes so a
        // provider that returns megabytes of debug HTML does not flood
        // the event log.
        let big = "x".repeat(10_000);
        let f = LlmFailure::from_llm_error(
            &LlmError::ApiError {
                provider: "anthropic".into(),
                status: 502,
                body: big,
            },
            64,
        );
        match f {
            LlmFailure::ApiError {
                status,
                body_excerpt,
            } => {
                assert_eq!(status, 502);
                assert!(
                    body_excerpt.ends_with('…'),
                    "marker missing: {body_excerpt}"
                );
                assert!(
                    body_excerpt.len() <= 64 + '…'.len_utf8(),
                    "excerpt larger than cap+marker: {} bytes",
                    body_excerpt.len(),
                );
            }
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn legacy_variants_fall_through_to_other_with_chain_preserved() {
        // `Rejected`, `NotImplemented`, and future non-exhaustive
        // additions degrade to `Other { message }` rather than getting
        // dropped. The Display chain must round-trip so a downstream
        // operator can still see the cause.
        let f = LlmFailure::from_llm_error(&LlmError::Rejected("payload too large".into()), 512);
        match f {
            LlmFailure::Other { message } => {
                assert!(
                    message.contains("payload too large"),
                    "lost cause: {message}"
                );
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }
}
