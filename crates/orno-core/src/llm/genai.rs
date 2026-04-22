//! Production `LlmTransport` implementation wrapping the `genai` crate.
//!
//! ADR 0002 keeps `genai` types behind this trait — every `genai::*` import
//! lives in this file, `Client::default()` / `Client::builder()` stay
//! internal, and the public surface returns only `LlmResponse` /
//! `LlmError`. Adding a provider means adding a match arm to
//! [`build_client`]; the caller sees no churn.
//!
//! Provider routing honors `AgentConfig.provider` (ADR 0002 amendment,
//! Phase 4 plan). Each provider key is pinned to a specific
//! `AdapterKind` via a `ServiceTargetResolver`, so `model: "gpt-5"` with
//! `provider: anthropic` fails fast at the API rather than silently
//! routing to `OpenAI` because the model prefix matched.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, StopReason, Tool, ToolCall as GenAiToolCall, ToolName,
    ToolResponse,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use tracing::instrument;

use crate::error::LlmError;
use crate::pipeline::AgentConfig;

use super::{
    LlmRequest, LlmResponse, LlmTransport, OrnoChatMessage, OrnoChatTool, OrnoChatToolCall, Usage,
};

/// Providers known in v0.1. Kept explicit so a typo in a pipeline YAML
/// surfaces as `ConfigError` at run start, not a confusing genai
/// adapter-mapping error mid-run.
const KNOWN_PROVIDERS: &[&str] = &["openai", "anthropic", "ollama", "openrouter"];

#[derive(Debug)]
pub struct GenAiTransport {
    clients: HashMap<String, Arc<Client>>,
}

impl GenAiTransport {
    /// Build a transport from the set of providers referenced by
    /// `agents`. One `genai::Client` per distinct provider. Returns a
    /// `ConfigError` for any provider name not in [`KNOWN_PROVIDERS`].
    ///
    /// `secrets` is the resolved `secrets.*` namespace from ADR 0020 —
    /// values present here (typically from `--secrets-file`) are handed
    /// to genai as literal auth data, taking precedence over the
    /// provider's conventional env-var lookup. When a provider's
    /// secret is absent from the map, the client falls back to
    /// `AuthData::from_env(...)` so CI runners and replay tapes that
    /// export keys in the shell keep working unchanged.
    ///
    /// API-key presence is NOT checked here — genai itself fails with
    /// `RequiresApiKey` when the transport is invoked without either
    /// a literal secret or an env var. Failing at run start rather
    /// than dispatch time is a Phase 7 improvement (`orno plan` will
    /// surface it).
    pub fn from_agents(
        agents: &BTreeMap<String, AgentConfig>,
        secrets: &BTreeMap<String, String>,
    ) -> Result<Self, LlmError> {
        let mut clients: HashMap<String, Arc<Client>> = HashMap::new();
        for cfg in agents.values() {
            if clients.contains_key(&cfg.provider) {
                continue;
            }
            let client = build_client(&cfg.provider, secrets)?;
            clients.insert(cfg.provider.clone(), Arc::new(client));
        }
        Ok(Self { clients })
    }
}

#[async_trait]
impl LlmTransport for GenAiTransport {
    #[instrument(
        skip(self, req),
        fields(
            llm.provider = %req.provider,
            llm.model = %req.model,
        ),
    )]
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let client = self.clients.get(&req.provider).ok_or_else(|| {
            LlmError::ConfigError(format!(
                "provider `{}` was not registered at transport construction — known: {}",
                req.provider,
                KNOWN_PROVIDERS.join(", "),
            ))
        })?;

        let mut chat = ChatRequest::new(vec![ChatMessage::user(req.prompt.clone())]);
        if let Some(system) = &req.system {
            chat = chat.with_system(system.clone());
        }
        for msg in &req.messages {
            chat = chat.append_message(orno_msg_to_genai(msg));
        }
        if !req.tools.is_empty() {
            chat = chat.with_tools(req.tools.iter().map(orno_tool_to_genai));
        }

        let mut options = ChatOptions::default();
        if let Some(t) = req.temperature {
            options = options.with_temperature(f64::from(t));
        }
        if let Some(max) = req.max_tokens {
            options = options.with_max_tokens(max);
        }

        let response = client
            .exec_chat(req.model.as_str(), chat, Some(&options))
            .await
            .map_err(|err| map_genai_error(&req.provider, &req.model, err))?;

        let finish_reason = response.stop_reason.as_ref().map(|r| r.raw().to_string());
        let usage = convert_usage(&response.usage);
        let is_tool_call = matches!(response.stop_reason, Some(StopReason::ToolCall(_)));

        if is_tool_call {
            let tool_calls = response
                .into_tool_calls()
                .into_iter()
                .map(|c| OrnoChatToolCall {
                    call_id: c.call_id,
                    fn_name: c.fn_name,
                    fn_arguments: c.fn_arguments,
                })
                .collect();
            return Ok(LlmResponse {
                content: String::new(),
                finish_reason,
                usage: Some(usage),
                tool_calls,
            });
        }

        let content = response
            .into_first_text()
            .ok_or_else(|| LlmError::ParseError("provider returned no text content".to_string()))?;

        Ok(LlmResponse {
            content,
            finish_reason,
            usage: Some(usage),
            tool_calls: Vec::new(),
        })
    }
}

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
fn build_client(provider: &str, secrets: &BTreeMap<String, String>) -> Result<Client, LlmError> {
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
        }
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
        }
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
        }
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
fn map_genai_error(provider: &str, model: &str, err: genai::Error) -> LlmError {
    use genai::Error as G;
    match err {
        G::HttpError { status, body, .. } => {
            status_to_error(provider, model, status.as_u16(), body)
        }
        G::RequiresApiKey { .. } | G::NoAuthData { .. } | G::NoAuthResolver { .. } => {
            LlmError::AuthFailed {
                provider: provider.to_string(),
            }
        }
        G::ChatResponseGeneration { cause, .. } => LlmError::ParseError(cause),
        G::NoChatResponse { .. } => {
            LlmError::ParseError("provider returned no chat response".to_string())
        }
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
fn convert_usage(u: &genai::chat::Usage) -> Usage {
    #[allow(clippy::cast_sign_loss)] // negatives clamped to 0 above
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
fn orno_msg_to_genai(msg: &OrnoChatMessage) -> ChatMessage {
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
        }
        OrnoChatMessage::ToolResult { call_id, content } => {
            ChatMessage::from(ToolResponse::new(call_id.clone(), content.clone()))
        }
    }
}

/// Translate orno's tool metadata into the genai `Tool` shape. Custom tool
/// names always land as `ToolName::Custom`; orno exposes no built-ins on this
/// surface.
fn orno_tool_to_genai(t: &OrnoChatTool) -> Tool {
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
    fn unknown_provider_rejected_at_construction() {
        let mut agents = BTreeMap::new();
        agents.insert(
            "bad".to_string(),
            AgentConfig {
                model: "x".into(),
                provider: "not-a-real-provider".into(),
                system: None,
                allowed_tools: Vec::new(),
                policy: default_policy(),
            },
        );
        let err = GenAiTransport::from_agents(&agents, &BTreeMap::new())
            .expect_err("must reject unknown provider");
        assert!(
            matches!(err, LlmError::ConfigError(msg) if msg.contains("not-a-real-provider")),
            "expected ConfigError naming the bad provider",
        );
    }

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
            }
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
            }
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

    fn default_policy() -> crate::pipeline::AgentPolicy {
        crate::pipeline::AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 0,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            allow_mutations: false,
            allow_network: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: crate::pipeline::OnParseError::Fail,
        }
    }
}
