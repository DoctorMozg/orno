//! Production `LlmTransport` implementation wrapping the `genai` crate.
//!
//! `genai` types are kept behind this trait — every `genai::*` import
//! lives in this submodule tree, `Client::default()` / `Client::builder()`
//! stay internal, and the public surface returns only `LlmResponse` /
//! `LlmError`. Adding a provider means adding a match arm to
//! [`convert::build_client`]; the caller sees no churn.
//!
//! Provider routing honors `AgentConfig.provider`. Each provider key is
//! pinned to a specific `AdapterKind` via a `ServiceTargetResolver`, so
//! `model: "gpt-5"` with `provider: anthropic` fails fast at the API
//! rather than silently routing to `OpenAI` because the model prefix
//! matched.
//!
//! The file split keeps `mod.rs` focused on the transport struct and
//! trait impl. Provider client construction, message/tool conversion,
//! and error mapping live in [`convert`] next to the only `genai::*`
//! imports they need.

mod convert;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use genai::Client;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatResponse, StopReason};
use tracing::instrument;

use crate::error::LlmError;
use crate::pipeline::AgentConfig;

use super::{LlmRequest, LlmResponse, LlmTransport, OrnoChatToolCall};
use convert::{
    KNOWN_PROVIDERS, build_client, convert_usage, is_transient_genai_error, map_genai_error,
    orno_msg_to_genai, orno_tool_to_genai,
};

/// How many *additional* attempts the transport will make after the
/// initial call fails on a transient error. Each retry doubles the
/// delay, starting at [`RETRY_BASE_DELAY_MS`]. The total budget across
/// all attempts is bounded so a 503-storm cannot block a node for
/// longer than its declared `timeout:` would otherwise allow.
const LLM_RETRY_MAX_ATTEMPTS: u32 = 3;
/// Initial backoff before the first retry. Doubled per attempt:
/// 500 ms, 1 s, 2 s — totals 3.5 s of wall-clock retry budget on the
/// worst case, leaving most of a per-node `timeout:` for productive
/// work.
const RETRY_BASE_DELAY_MS: u64 = 500;

#[derive(Debug)]
pub struct GenAiTransport {
    clients: HashMap<String, Arc<Client>>,
}

impl GenAiTransport {
    /// Build a transport from the set of providers referenced by
    /// `agents`. One `genai::Client` per distinct provider. Returns a
    /// `ConfigError` for any provider name not in `KNOWN_PROVIDERS`.
    ///
    /// `secrets` is the resolved `secrets.*` namespace — values present
    /// here (typically from `--secrets-file`) are handed to genai as
    /// literal auth data, taking precedence over the provider's
    /// conventional env-var lookup. When a provider's secret is absent
    /// from the map, the client falls back to `AuthData::from_env(...)`
    /// so CI runners and replay tapes that export keys in the shell
    /// keep working unchanged.
    ///
    /// API-key presence is NOT checked here — genai itself fails with
    /// `RequiresApiKey` when the transport is invoked without either
    /// a literal secret or an env var.
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
        for msg in req.messages.iter() {
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

        let response =
            exec_chat_with_retry(client, req.model.as_str(), &chat, &options, &req.provider)
                .await?;

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
                usage,
                tool_calls,
            });
        }

        let content = response
            .into_first_text()
            .ok_or_else(|| LlmError::ParseError("provider returned no text content".to_string()))?;

        Ok(LlmResponse {
            content,
            finish_reason,
            usage,
            tool_calls: Vec::new(),
        })
    }
}

/// Drive `exec_chat` with bounded exponential backoff on transient
/// failures (HTTP 429/5xx, connect/timeout transport errors). Permanent
/// errors (auth, model-not-found, payload problems) short-circuit on
/// the first attempt — retrying a 401 only delays the inevitable and
/// burns the operator's rate-limit budget. Each retry clones the
/// request because `exec_chat` consumes its `ChatRequest`; cloning is
/// cheap relative to the network round trip we are already paying.
async fn exec_chat_with_retry(
    client: &Client,
    model: &str,
    chat: &ChatRequest,
    options: &ChatOptions,
    provider: &str,
) -> Result<ChatResponse, LlmError> {
    let mut attempt: u32 = 0;
    loop {
        match client.exec_chat(model, chat.clone(), Some(options)).await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                let transient = is_transient_genai_error(&err);
                if attempt < LLM_RETRY_MAX_ATTEMPTS && transient {
                    let delay_ms = RETRY_BASE_DELAY_MS.saturating_mul(1u64 << attempt);
                    tracing::warn!(
                        llm.provider = %provider,
                        llm.model = %model,
                        attempt = attempt + 1,
                        max_attempts = LLM_RETRY_MAX_ATTEMPTS,
                        delay_ms,
                        error = %err,
                        "LLM transient error; retrying after backoff",
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    attempt += 1;
                    continue;
                }
                return Err(map_genai_error(provider, model, err));
            },
        }
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
            max_tool_output_bytes: None,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            on_parse_error: crate::pipeline::OnParseError::Fail,
            roots: Vec::new(),
            max_message_history_bytes: None,
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
