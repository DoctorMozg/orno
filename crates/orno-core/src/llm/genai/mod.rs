//! Production `LlmTransport` implementation wrapping the `genai` crate.
//!
//! ADR 0002 keeps `genai` types behind this trait — every `genai::*` import
//! lives in this submodule tree, `Client::default()` / `Client::builder()`
//! stay internal, and the public surface returns only `LlmResponse` /
//! `LlmError`. Adding a provider means adding a match arm to
//! [`convert::build_client`]; the caller sees no churn.
//!
//! Provider routing honors `AgentConfig.provider` (ADR 0002 amendment,
//! Phase 4 plan). Each provider key is pinned to a specific
//! `AdapterKind` via a `ServiceTargetResolver`, so `model: "gpt-5"` with
//! `provider: anthropic` fails fast at the API rather than silently
//! routing to `OpenAI` because the model prefix matched.
//!
//! The file split keeps `mod.rs` focused on the transport struct and
//! trait impl. Provider client construction, message/tool conversion,
//! and error mapping live in [`convert`] next to the only `genai::*`
//! imports they need.

mod convert;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use genai::Client;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, StopReason};
use tracing::instrument;

use crate::error::LlmError;
use crate::pipeline::AgentConfig;

use super::{LlmRequest, LlmResponse, LlmTransport, OrnoChatToolCall};
use convert::{
    KNOWN_PROVIDERS, build_client, convert_usage, map_genai_error, orno_msg_to_genai,
    orno_tool_to_genai,
};

#[derive(Debug)]
pub struct GenAiTransport {
    clients: HashMap<String, Arc<Client>>,
}

impl GenAiTransport {
    /// Build a transport from the set of providers referenced by
    /// `agents`. One `genai::Client` per distinct provider. Returns a
    /// `ConfigError` for any provider name not in `KNOWN_PROVIDERS`.
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
    #[must_use = "transport must be stored and threaded into the engine; dropping it discards per-provider client setup"]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> crate::pipeline::AgentPolicy {
        crate::pipeline::AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 0,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: crate::pipeline::OnParseError::Fail,
        }
    }

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
}
