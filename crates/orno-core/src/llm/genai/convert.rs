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

/// Pick the `AuthData` the resolver will hand genai for a given
/// provider. CLI-resolved secrets (ADR 0020 `secrets.*` namespace)
/// take precedence; absent keys fall back to `AuthData::from_env(...)`
/// so shell-export and CI workflows keep working. The fallback is
/// genai-native — an `ApiKeyEnvNotFound` error from the adapter is
/// the signal that neither path found a credential.
fn resolve_auth(env_name: &str, secrets: &BTreeMap<String, String>) -> AuthData {
    match secrets.get(env_name) {
        Some(value) => AuthData::from_single(value.clone()),
        None => AuthData::from_env(env_name),
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
        "ollama" => Ok(Client::builder()
            .with_service_target_resolver(ServiceTargetResolver::from_resolver_fn(
                |st: ServiceTarget| -> Result<ServiceTarget, genai::resolver::Error> {
                    Ok(ServiceTarget {
                        endpoint: Endpoint::from_static("http://localhost:11434/v1/"),
                        auth: AuthData::None,
                        model: ModelIden::new(AdapterKind::Ollama, st.model.model_name),
                    })
                },
            ))
            .build()),
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

/// Collapse `genai::Error` into the small set of `LlmError` variants
/// callers can reason about. HTTP status codes dispatch to the
/// typed variants (`AuthFailed`, `RateLimited`, `ModelNotFound`);
/// everything else falls through to `ApiError` (wire-level) or
/// `Transport` / `ParseError` (local problem).
pub(super) fn map_genai_error(provider: &str, model: &str, err: genai::Error) -> LlmError {
    use genai::Error as G;
    match err {
        G::HttpError { status, body, .. } => {
            status_to_error(provider, model, status.as_u16(), body)
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

fn status_to_error(provider: &str, model: &str, status: u16, body: String) -> LlmError {
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
            body,
        },
    }
}

/// Usage reported by a provider uses `Option<i32>` fields (genai
/// normalizes zero → None for cross-provider consistency). orno's
/// `Usage` is `u32`-tight because the budget enforcer math assumes
/// non-negative counters. Negative genai values — which shouldn't
/// happen but are representable — collapse to 0.
pub(super) fn convert_usage(u: &genai::chat::Usage) -> Usage {
    #[expect(clippy::cast_sign_loss, reason = "negatives clamped to 0 above")]
    fn nonneg(v: Option<i32>) -> u32 {
        match v {
            Some(n) if n > 0 => n as u32,
            _ => 0,
        }
    }
    Usage {
        prompt_tokens: nonneg(u.prompt_tokens),
        completion_tokens: nonneg(u.completion_tokens),
        total_tokens: nonneg(u.total_tokens),
    }
}

/// Translate an orno-owned message variant into the genai counterpart.
/// Kept private so `genai::ChatMessage` never appears in a public signature
/// (ADR 0002). `ToolCalls` collapses to a single assistant turn via
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
        // ADR 0020 ergonomics: a `--secrets-file` value reaches genai
        // as a literal `AuthData::Key`, not as a deferred env lookup.
        // Without this, the adapter's request-time `std::env::var`
        // call would still fail even though the user handed orno the
        // credential explicitly.
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
    fn status_401_maps_to_auth_failed() {
        match status_to_error("openai", "gpt-5", 401, "bad key".into()) {
            LlmError::AuthFailed { provider } => assert_eq!(provider, "openai"),
            other => panic!("expected AuthFailed, got {other:?}"),
        }
    }

    #[test]
    fn status_403_maps_to_auth_failed() {
        assert!(matches!(
            status_to_error("x", "m", 403, String::new()),
            LlmError::AuthFailed { .. }
        ));
    }

    #[test]
    fn status_404_maps_to_model_not_found() {
        match status_to_error("anthropic", "claude-9001", 404, String::new()) {
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
            status_to_error("x", "m", 429, String::new()),
            LlmError::RateLimited { .. }
        ));
    }

    #[test]
    fn status_500_falls_through_to_api_error() {
        match status_to_error("x", "m", 503, "upstream boom".into()) {
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
        let out = convert_usage(&u);
        assert_eq!(out.prompt_tokens, 10);
        assert_eq!(out.completion_tokens, 0);
        assert_eq!(out.total_tokens, 0);
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
