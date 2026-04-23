//! Typed failure payloads attached to `Event::NodeFinished` and
//! `Event::LlmRequestFailed`, plus the `SkipReason` / `BudgetKind`
//! discriminators used by related events.
//!
//! Split out of `mod.rs` so the `Event` enum and its failure detail
//! types can evolve independently. The `from_llm_error` classifier
//! lives next to `LlmFailure` so the variant set and its mapping from
//! `LlmError` stay wired together — originally sited here per ADR 0023
//! and kept co-located through the module split.

use serde::{Deserialize, Serialize};

use super::truncate_excerpt;

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

/// Which budget dimension the agent exhausted. Carried on
/// `NodeFailure::BudgetExceeded` so downstream alerting can distinguish
/// a token-count breach from a tool-call-count breach.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BudgetKind {
    /// `max_total_tokens` was exceeded across the iteration history.
    Tokens,
    /// `max_tool_calls` was exceeded within the run.
    ToolCalls,
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
    /// The agent exhausted `max_iterations` without reaching a `stop`
    /// finish reason. The final LLM call returned a tool-call turn and
    /// the loop could not continue.
    IterationLimitExceeded { max_iterations: u32 },
    /// A running budget (`max_total_tokens` or `max_tool_calls`) was
    /// exceeded. `budget_kind` discriminates which dimension breached.
    /// The field is `budget_kind` (not `kind`) because `kind` is the
    /// serde tag discriminator on this enum, mirroring the
    /// `NoExecutorRegistered { node_kind }` convention above.
    BudgetExceeded { budget_kind: BudgetKind },
    /// A tool call was denied by the policy gate (`allow_mutations`,
    /// `allow_network`, domain lists). Per ADR 0005 §3 the denial is
    /// fed back to the model as a tool-result string; this variant is
    /// available for future strict-mode use.
    ToolDenied { tool_name: String, reason: String },
    /// The node did not return before `Node.timeout` elapsed (ADR
    /// 0017). `limit_secs` echoes the declared budget. The preceding
    /// `Event::NodeTimedOut` envelope carries the measured
    /// `elapsed_ms`; this variant stays minimal so `NodeFailure`
    /// keeps its one-cause-per-variant shape.
    TimedOut { limit_secs: u64 },
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
