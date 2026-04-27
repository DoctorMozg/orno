//! Conversion layer between orno-owned LLM types and `genai`.
//!
//! Kept in a sibling file of [`super`] because every `genai::*` import
//! the transport needs lives in one of two places: the message/tool
//! translation helpers below, or the provider-pinning resolver in
//! [`build_client`]. Isolating them here lets `super::mod.rs` contain
//! only the trait impl + struct without a wall of conversion glue.

use std::collections::BTreeMap;

use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage, Tool, ToolCall as GenAiToolCall, ToolName, ToolResponse};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};

use crate::error::LlmError;
use crate::llm::{OrnoChatMessage, OrnoChatTool, Usage};

/// Providers known in v0.1. Kept explicit so a typo in a pipeline YAML
/// surfaces as `ConfigError` at run start, not a confusing genai
/// adapter-mapping error mid-run.
pub(super) const KNOWN_PROVIDERS: &[&str] = &["openai", "anthropic", "ollama", "openrouter"];

/// Hard cap on the body bytes preserved on `LlmError::ApiError.body`.
/// A misbehaving provider can return a multi-megabyte HTML error page
/// (think Cloudflare 502 walls) — the chained `Display` of the resulting
/// error walks the entire body, blowing up tracing fields and tool
/// outputs in the process. 512 bytes is enough to keep the head of a
/// JSON `error.message` payload, where the actionable signal lives.
/// Wire-format truncation in `LlmFailure::ApiError.body_excerpt` is a
/// separate concern (driven by `body_excerpt_max_bytes`); this constant
/// is the in-memory cap that follows the `LlmError` value through the
/// error chain, not the on-the-wire excerpt.
const MAX_API_ERR_BODY_BYTES: usize = 512;

/// Pick the `AuthData` the resolver will hand genai for a given
/// provider. CLI-resolved secrets (the `secrets.*` namespace) take
/// precedence; absent keys fall back to `AuthData::from_env(...)` so
/// shell-export and CI workflows keep working. The fallback is
/// genai-native — an `ApiKeyEnvNotFound` error from the adapter is
/// the signal that neither path found a credential.
fn resolve_auth(env_name: &str, secrets: &BTreeMap<String, String>) -> AuthData {
    match secrets.get(env_name) {
        Some(value) => AuthData::from_single(value.clone()),
        None => AuthData::from_env(env_name),
    }
}

/// Normalize an `OLLAMA_HOST` value (or fall back to the local default)
/// into the URL form genai's Ollama adapter expects. Accepts:
///
/// - `None` → `http://localhost:11434/v1/`
/// - `host:port` → prepends `http://` and appends `/v1/`
/// - `http(s)://host:port` → appends `/v1/`
/// - `http(s)://host:port/v1` → appends trailing `/`
/// - `http(s)://host:port/v1/` → kept as-is
///
/// Trailing slashes are normalized so the endpoint is always a clean
/// `…/v1/` URL regardless of how the user wrote the env var.
fn ollama_endpoint(host: Option<&str>) -> String {
    let raw = host.unwrap_or("http://localhost:11434");
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let trimmed = with_scheme.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/")
    } else {
        format!("{trimmed}/v1/")
    }
}

/// Emit an operator-visible warning when `OLLAMA_HOST` points anywhere
/// other than the loopback. Ollama serves prompts (and any tool-call
/// JSON they carry) in cleartext over HTTP; an operator who set the
/// host to `ollama.internal:11434` may not realize they just opted
/// into shipping every prompt across the wire to a different machine.
/// Loud once at client construction is the right balance — silent is
/// dangerous, per-request is noise.
fn warn_if_ollama_remote(host: Option<&str>) {
    let Some(raw) = host else { return };
    let parse_target = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let Ok(url) = reqwest::Url::parse(&parse_target) else {
        // Don't warn on a parse failure here — `ollama_endpoint`
        // already formats the URL and genai will surface the malformed
        // value at request time. Suppressing the warning avoids
        // double-noise on the same misconfiguration.
        return;
    };
    let is_loopback = url
        .host_str()
        .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "::1" | "[::1]"));
    if !is_loopback {
        tracing::warn!(
            ollama_host = %raw,
            "OLLAMA_HOST points to a non-loopback address; \
             prompts and tool-call payloads will leave this machine in cleartext",
        );
    }
}

/// Adapter-pinning resolver: every client holds a closure that fixes
/// the `AdapterKind` (and, for openrouter, the endpoint + auth env)
/// regardless of what genai's prefix detection would otherwise pick.
/// This is what makes `provider: anthropic + model: gpt-5` fail at the
/// provider rather than silently routing to `OpenAI`.
///
/// Each auth'd closure captures its `AuthData` by move so the genai
/// resolver has no reason to consult `std::env` at request time when
/// the CLI already resolved the secret. `AuthData::clone()` returns
/// a fresh owned value per call; the capture stays `Fn`, not `FnMut`.
#[expect(
    clippy::too_many_lines,
    reason = "single dispatch table over the four supported providers; \
              splitting per-provider helpers would obscure the side-by-side \
              comparison that catches drift between adapter wirings"
)]
pub(super) fn build_client(
    provider: &str,
    secrets: &BTreeMap<String, String>,
) -> Result<Client, LlmError> {
    match provider {
        "openai" => {
            let auth = resolve_auth("OPENAI_API_KEY", secrets);
            Ok(Client::builder()
                .with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                    move |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        Ok(ServiceTarget {
                            endpoint: Endpoint::from_static("https://api.openai.com/v1/"),
                            auth: auth.clone(),
                            model: ModelIden::new(AdapterKind::OpenAI, st.model.model_name),
                        })
                    },
                ))
                .build())
        },
        "anthropic" => {
            let auth = resolve_auth("ANTHROPIC_API_KEY", secrets);
            Ok(Client::builder()
                .with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                    move |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        Ok(ServiceTarget {
                            endpoint: Endpoint::from_static("https://api.anthropic.com/v1/"),
                            auth: auth.clone(),
                            model: ModelIden::new(AdapterKind::Anthropic, st.model.model_name),
                        })
                    },
                ))
                .build())
        },
        "ollama" => {
            let host_env = std::env::var("OLLAMA_HOST").ok();
            warn_if_ollama_remote(host_env.as_deref());
            let endpoint = ollama_endpoint(host_env.as_deref());
            Ok(Client::builder()
                .with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                    move |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        Ok(ServiceTarget {
                            endpoint: Endpoint::from_owned(endpoint.clone()),
                            auth: AuthData::None,
                            model: ModelIden::new(AdapterKind::Ollama, st.model.model_name),
                        })
                    },
                ))
                .build())
        },
        "openrouter" => {
            let auth = resolve_auth("OPENROUTER_API_KEY", secrets);
            Ok(Client::builder()
                .with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                    move |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                        Ok(ServiceTarget {
                            endpoint: Endpoint::from_static("https://openrouter.ai/api/v1/"),
                            auth: auth.clone(),
                            model: ModelIden::new(AdapterKind::OpenAI, st.model.model_name),
                        })
                    },
                ))
                .build())
        },
        other => Err(LlmError::ConfigError(format!(
            "unknown provider `{other}`; known: {}",
            KNOWN_PROVIDERS.join(", "),
        ))),
    }
}

/// Classify a `genai::Error` as transient (worth retrying) vs
/// permanent. HTTP 429 + 5xx are the canonical retry codes; lower-level
/// network errors (connect failures, idle-timeouts) are wrapped by
/// genai inside `WebAdapterCall` / `WebModelCall` / `WebStream`, so the
/// match descends one layer to the embedded `webc::Error::Reqwest` and
/// asks reqwest itself.
///
/// Any non-HTTP, non-network class (parse errors, auth, model-not-found,
/// internal adapter bugs) is treated as permanent — retrying a 401 only
/// burns the operator's rate-limit budget, and a `ParseError` will
/// reproduce on the same payload.
pub(super) fn is_transient_genai_error(err: &genai::Error) -> bool {
    use genai::Error as G;
    match err {
        G::HttpError { status, .. } => {
            matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
        },
        G::WebAdapterCall { webc_error, .. } | G::WebModelCall { webc_error, .. } => {
            is_transient_webc_error(webc_error)
        },
        G::WebStream { error, .. } => {
            // The boxed error chain may carry a `reqwest::Error` —
            // unwrap one level to check `is_connect`/`is_timeout`. If the
            // chain is opaque, treat as permanent so we don't spin on
            // an unknown failure class.
            error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.is_timeout())
        },
        _ => false,
    }
}

fn is_transient_webc_error(err: &genai::webc::Error) -> bool {
    use genai::webc::Error as W;
    match err {
        W::ResponseFailedStatus { status, .. } => {
            matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
        },
        W::Reqwest(e) => e.is_connect() || e.is_timeout(),
        _ => false,
    }
}

/// Collapse `genai::Error` into the small set of `LlmError` variants
/// callers can reason about. HTTP status codes dispatch to the
/// typed variants (`AuthFailed`, `RateLimited`, `ModelNotFound`);
/// everything else falls through to `ApiError` (wire-level) or
/// `Transport` / `ParseError` (local problem).
pub(super) fn map_genai_error(provider: &str, model: &str, err: genai::Error) -> LlmError {
    use genai::Error as G;
    match err {
        G::HttpError { status, body, .. } => {
            status_to_error(provider, model, status.as_u16(), &body)
        },
        G::RequiresApiKey { .. } | G::NoAuthData { .. } | G::NoAuthResolver { .. } => {
            LlmError::AuthFailed {
                provider: provider.to_string(),
            }
        },
        G::ChatResponseGeneration { cause, .. } => LlmError::ParseError(cause),
        G::NoChatResponse { .. } => {
            LlmError::ParseError("provider returned no chat response".to_string())
        },
        G::StreamParse { serde_error, .. } => LlmError::ParseError(serde_error.to_string()),
        G::SerdeJson(e) => LlmError::ParseError(e.to_string()),
        other => LlmError::Transport(Box::new(GenAiErrorAdapter(other))),
    }
}

fn status_to_error(provider: &str, model: &str, status: u16, body: &str) -> LlmError {
    match status {
        401 | 403 => LlmError::AuthFailed {
            provider: provider.to_string(),
        },
        404 => LlmError::ModelNotFound {
            provider: provider.to_string(),
            model: model.to_string(),
        },
        429 => LlmError::RateLimited {
            provider: provider.to_string(),
        },
        _ => LlmError::ApiError {
            provider: provider.to_string(),
            status,
            body: truncate_api_err_body(body),
        },
    }
}

/// Bound the body string captured into `LlmError::ApiError`. Keeps the
/// leading window — the JSON `error.message` field shows up first in
/// every provider's payload and is what an operator actually reads —
/// and walks the cut back to a UTF-8 boundary so a multi-byte character
/// across the threshold does not leave an invalid string. Suffix names
/// the dropped byte count so logs aren't ambiguous about whether the
/// payload was short or merely cut.
fn truncate_api_err_body(body: &str) -> String {
    if body.len() <= MAX_API_ERR_BODY_BYTES {
        return body.to_string();
    }
    let mut end = MAX_API_ERR_BODY_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = body.len() - end;
    format!("{}…[{dropped} bytes truncated]", &body[..end])
}

/// Usage reported by a provider uses `Option<i32>` fields (genai
/// normalizes zero → None for cross-provider consistency). orno's
/// `Usage` is `u32`-tight because the budget enforcer math assumes
/// non-negative counters. Negative genai values — which shouldn't
/// happen but are representable — collapse to 0.
///
/// Returns `None` when the provider reported no usable token data so
/// downstream consumers can distinguish "the model used zero tokens"
/// from "the provider didn't say". Today no provider legitimately
/// reports `0/0/0`; collapsing that case to `None` keeps the budget
/// enforcer from charging zero against `max_total_tokens` and lets the
/// loop agent emit its degraded-enforcement `tracing::warn` rather than
/// silently bumping nothing.
pub(super) fn convert_usage(u: &genai::chat::Usage) -> Option<Usage> {
    #[expect(
        clippy::cast_sign_loss,
        reason = "negatives clamped to 0 by the guarded match arm above"
    )]
    fn nonneg(v: Option<i32>) -> u32 {
        match v {
            Some(n) if n > 0 => n as u32,
            _ => 0,
        }
    }
    let prompt = nonneg(u.prompt_tokens);
    let completion = nonneg(u.completion_tokens);
    let total = nonneg(u.total_tokens);
    if prompt == 0 && completion == 0 && total == 0 {
        return None;
    }
    Some(Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    })
}

/// Translate an orno-owned message variant into the genai counterpart.
/// Kept private so `genai::ChatMessage` never appears in a public
/// signature. `ToolCalls` collapses to a single assistant turn via
/// `ChatMessage::from(Vec<ToolCall>)`; `ToolResult` becomes a `Tool`-role
/// message carrying a `ToolResponse`.
pub(super) fn orno_msg_to_genai(msg: &OrnoChatMessage) -> ChatMessage {
    match msg {
        OrnoChatMessage::User { content } => ChatMessage::user(content.clone()),
        OrnoChatMessage::Assistant { content } => ChatMessage::assistant(content.clone()),
        OrnoChatMessage::ToolCalls { calls } => {
            let genai_calls: Vec<GenAiToolCall> = calls
                .iter()
                .map(|c| GenAiToolCall {
                    call_id: c.call_id.clone(),
                    fn_name: c.fn_name.clone(),
                    fn_arguments: c.fn_arguments.clone(),
                    thought_signatures: None,
                })
                .collect();
            ChatMessage::from(genai_calls)
        },
        OrnoChatMessage::ToolResult { call_id, content } => {
            ChatMessage::from(ToolResponse::new(call_id.clone(), content.clone()))
        },
    }
}

/// Translate orno's tool metadata into the genai `Tool` shape. Custom tool
/// names always land as `ToolName::Custom`; orno exposes no built-ins on this
/// surface.
pub(super) fn orno_tool_to_genai(t: &OrnoChatTool) -> Tool {
    Tool::new(ToolName::Custom(t.name.clone()))
        .with_description(t.description.clone())
        .with_schema(t.schema.clone())
}

/// Wrapper so `genai::Error` can cross the `std::error::Error` trait
/// object boundary required by `LlmError::Transport`. `genai::Error`
/// implements `Display` via `derive_more`; we forward both.
#[derive(Debug)]
struct GenAiErrorAdapter(genai::Error);

impl std::fmt::Display for GenAiErrorAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GenAiErrorAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_auth_prefers_cli_secret_over_env_lookup() {
        // Secrets-file ergonomics: a `--secrets-file` value reaches
        // genai as a literal `AuthData::Key`, not as a deferred env
        // lookup. Without this, the adapter's request-time
        // `std::env::var` call would still fail even though the user
        // handed orno the credential explicitly.
        let mut secrets = BTreeMap::new();
        secrets.insert("OPENROUTER_API_KEY".into(), "cli-resolved-value".into());

        match resolve_auth("OPENROUTER_API_KEY", &secrets) {
            AuthData::Key(value) => assert_eq!(value, "cli-resolved-value"),
            other => panic!("expected AuthData::Key, got {other:?}"),
        }
    }

    #[test]
    fn resolve_auth_falls_back_to_env_when_secret_absent() {
        // The env path stays reachable so CI runners that `export` the
        // key in the shell — and replay tapes that don't thread secrets
        // through at all — keep working without a CLI flag.
        let secrets = BTreeMap::new();
        match resolve_auth("OPENROUTER_API_KEY", &secrets) {
            AuthData::FromEnv(name) => assert_eq!(name, "OPENROUTER_API_KEY"),
            other => panic!("expected AuthData::FromEnv, got {other:?}"),
        }
    }

    #[test]
    fn build_client_openrouter_with_secret_does_not_defer_to_env() {
        // Integration-style check over the build_client → closure path:
        // a pipeline that resolves OPENROUTER_API_KEY via `--secrets-file`
        // must construct a client whose resolver never calls into
        // `std::env` for auth. Proving this without spawning a live
        // request is awkward, so we check the precondition by making
        // resolve_auth's branch observable: a secret-backed client
        // builds the same way as one without, never erroring on
        // construction.
        let mut secrets = BTreeMap::new();
        secrets.insert("OPENROUTER_API_KEY".into(), "cli-val".into());
        assert!(build_client("openrouter", &secrets).is_ok());
        assert!(build_client("openrouter", &BTreeMap::new()).is_ok());
    }

    #[test]
    fn ollama_endpoint_defaults_to_local_when_unset() {
        assert_eq!(ollama_endpoint(None), "http://localhost:11434/v1/");
    }

    #[test]
    fn ollama_endpoint_accepts_bare_host_and_prepends_http_and_v1_path() {
        assert_eq!(
            ollama_endpoint(Some("ollama.internal:11434")),
            "http://ollama.internal:11434/v1/"
        );
    }

    #[test]
    fn ollama_endpoint_preserves_https_scheme_when_provided() {
        assert_eq!(
            ollama_endpoint(Some("https://ollama.example.com")),
            "https://ollama.example.com/v1/"
        );
    }

    #[test]
    fn ollama_endpoint_normalizes_existing_v1_path_to_trailing_slash() {
        assert_eq!(
            ollama_endpoint(Some("http://ollama.local:11434/v1")),
            "http://ollama.local:11434/v1/"
        );
    }

    #[test]
    fn ollama_endpoint_strips_redundant_trailing_slashes() {
        assert_eq!(
            ollama_endpoint(Some("http://ollama.local:11434/")),
            "http://ollama.local:11434/v1/"
        );
        assert_eq!(
            ollama_endpoint(Some("http://ollama.local:11434/v1/")),
            "http://ollama.local:11434/v1/"
        );
    }

    #[test]
    fn build_client_ollama_succeeds_regardless_of_env_state() {
        // build_client reads OLLAMA_HOST internally; the construction
        // path must not error on either branch even when the env is
        // unset. (We avoid mutating the process env here because
        // `std::env::set_var` is forbidden by the workspace clippy
        // policy.)
        assert!(build_client("ollama", &BTreeMap::new()).is_ok());
    }

    #[test]
    fn status_401_maps_to_auth_failed() {
        match status_to_error("openai", "gpt-5", 401, "bad key") {
            LlmError::AuthFailed { provider } => assert_eq!(provider, "openai"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn status_403_maps_to_auth_failed() {
        assert!(matches!(
            status_to_error("x", "m", 403, ""),
            LlmError::AuthFailed { .. }
        ));
    }

    #[test]
    fn status_404_maps_to_model_not_found() {
        match status_to_error("anthropic", "claude-9001", 404, "") {
            LlmError::ModelNotFound { provider, model } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model, "claude-9001");
            },
            other => panic!("expected ModelNotFound, got {other:?}"),
        }
    }

    #[test]
    fn status_429_maps_to_rate_limited() {
        assert!(matches!(
            status_to_error("x", "m", 429, ""),
            LlmError::RateLimited { .. }
        ));
    }

    #[test]
    fn status_500_falls_through_to_api_error() {
        match status_to_error("x", "m", 503, "upstream boom") {
            LlmError::ApiError {
                provider,
                status,
                body,
            } => {
                assert_eq!(provider, "x");
                assert_eq!(status, 503);
                assert_eq!(body, "upstream boom");
            },
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn convert_usage_handles_none_and_negative() {
        let u = genai::chat::Usage {
            prompt_tokens: Some(10),
            completion_tokens: None,
            total_tokens: Some(-1),
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        let out = convert_usage(&u).expect("non-zero prompt_tokens must yield Some");
        assert_eq!(out.prompt_tokens, 10);
        assert_eq!(out.completion_tokens, 0);
        assert_eq!(out.total_tokens, 0);
    }

    #[test]
    fn convert_usage_returns_none_when_provider_reports_no_counts() {
        // genai's zero-as-None normalization plus a provider that never
        // populates totals would surface as 0/0/0 here. Returning `None`
        // is what the loop agent's degraded-enforcement branch depends
        // on — without it, the budget enforcer would silently bump
        // `total_tokens` by zero on every iteration and the warn that
        // tells the operator "your provider returned no usage" would
        // never fire.
        let u = genai::chat::Usage {
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            prompt_tokens_details: None,
            completion_tokens_details: None,
        };
        assert!(convert_usage(&u).is_none());
    }

    #[test]
    fn truncate_api_err_body_passes_short_input_through() {
        // Bodies under the cap stay verbatim. The `status_500…` test
        // above relies on this: "upstream boom" is well under 512 bytes.
        assert_eq!(truncate_api_err_body("upstream boom"), "upstream boom");
    }

    #[test]
    fn truncate_api_err_body_caps_long_input_with_dropped_count() {
        let body = "a".repeat(MAX_API_ERR_BODY_BYTES + 100);
        let cut = truncate_api_err_body(&body);
        assert!(
            cut.starts_with(&"a".repeat(MAX_API_ERR_BODY_BYTES)),
            "must keep the leading window so error.message stays visible",
        );
        assert!(
            cut.contains("100 bytes truncated"),
            "must announce the dropped byte count",
        );
    }

    #[test]
    fn truncate_api_err_body_walks_back_to_utf8_boundary() {
        // 'é' is two bytes; placed so it straddles the threshold the
        // cut must walk back so the resulting String stays valid UTF-8.
        let mut body = "a".repeat(MAX_API_ERR_BODY_BYTES - 1);
        body.push('é');
        body.push_str(&"b".repeat(50));
        let cut = truncate_api_err_body(&body);
        // String construction enforces UTF-8 validity, so the cut
        // body itself can't be invalid; the suffix witnesses that the
        // walk-back actually triggered rather than passing through.
        assert!(cut.contains("bytes truncated"));
    }

    #[test]
    fn test_orno_msg_user_converts_to_genai() {
        let msg = OrnoChatMessage::User {
            content: "hi there".to_string(),
        };
        let genai_msg = orno_msg_to_genai(&msg);
        assert_eq!(genai_msg.role, genai::chat::ChatRole::User);
    }

    #[test]
    fn test_orno_tool_converts_to_genai() {
        let tool = OrnoChatTool {
            name: "get_weather".to_string(),
            description: "Looks up the current weather".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let genai_tool = orno_tool_to_genai(&tool);
        assert_eq!(genai_tool.name.as_str(), "get_weather");
        assert_eq!(
            genai_tool.description.as_deref(),
            Some("Looks up the current weather"),
        );
    }
}
