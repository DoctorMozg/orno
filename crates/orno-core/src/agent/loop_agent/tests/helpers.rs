use crate::agent::AgentRequest;
use crate::error::LlmError;
use crate::error::ToolError;
use crate::llm::LlmTransport;
use crate::llm::{LlmRequest, LlmResponse, OrnoChatMessage};
use crate::pipeline::{AgentPolicy, OnHistoryOverflow, OnParseError};
use crate::tool::ToolHandler;
use crate::tool::net_guard::HostResolver;
use crate::tool::{ToolEffect, ToolInvocation};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Transport stub that returns a caller-chosen `LlmError`. Lives in the test
/// module because production code never wants a transport that always fails —
/// its only purpose is to exercise the `LlmRequestFailed` emission path.
pub(super) struct FailingTransport(pub(super) LlmError);

impl FailingTransport {
    pub(super) fn auth() -> Self {
        Self(LlmError::AuthFailed {
            provider: "openai".into(),
        })
    }
}

#[async_trait]
impl LlmTransport for FailingTransport {
    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
        // Cloning by reconstruction since LlmError isn't Clone — the
        // stub holds a single error and the test calls it once.
        Err(match &self.0 {
            LlmError::AuthFailed { provider } => LlmError::AuthFailed {
                provider: provider.clone(),
            },
            other => panic!("FailingTransport got an unsupported variant: {other:?}"),
        })
    }
}

pub(super) fn policy() -> AgentPolicy {
    AgentPolicy {
        max_iterations: 1,
        max_total_tokens: 1000,
        max_tool_calls: 0,
        max_subagent_depth: 0,
        max_tool_output_bytes: None,
        allow_mutations: false,
        allow_network: false,
        allow_context_writes: false,
        allowed_domains: Vec::new(),
        blocked_domains: Vec::new(),
        on_parse_error: OnParseError::Fail,
        roots: Vec::new(),
        max_message_history_bytes: None,
        on_history_overflow: OnHistoryOverflow::Truncate,
        unconfined_shell: false,
    }
}

pub(super) fn request() -> AgentRequest {
    AgentRequest {
        agent_name: "greeter".into(),
        initial_prompt: std::sync::Arc::from("say hi"),
        system: None,
        provider: std::sync::Arc::from("openai"),
        model: std::sync::Arc::from("gpt-5"),
        policy: policy(),
        allowed_tools: Vec::new(),
        depth: 0,
        parent_budget: None,
        render_scope: None,
    }
}

/// Minimal `ToolHandler` that returns a canned output for any call. Used to
/// exercise the tool-dispatch path without real I/O.
pub(super) struct EchoTool {
    pub(super) effect: ToolEffect,
    pub(super) output: &'static str,
    pub(super) name: &'static str,
}

impl EchoTool {
    pub(super) fn new(effect: ToolEffect, output: &'static str) -> Self {
        Self {
            effect,
            output,
            name: "EchoTool",
        }
    }
}

#[async_trait]
impl ToolHandler for EchoTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Returns a fixed string."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn effect(&self) -> ToolEffect {
        self.effect
    }
    async fn invoke(
        &self,
        _inv: ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok(self.output.to_string())
    }
}

/// Dotted-name handler used to exercise the wire-name translation path without
/// a full `SubagentHandler` dispatch. Returns a fixed string — the assertion
/// is that the LLM's wire-form tool call (`subagent_child`) routes back to
/// this handler whose YAML name contains a dot.
pub(super) struct DottedEchoTool;

#[async_trait]
impl ToolHandler for DottedEchoTool {
    fn name(&self) -> &str {
        "subagent.child"
    }
    fn description(&self) -> &str {
        "Dotted-name echo for wire translation."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn invoke(
        &self,
        _inv: ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("dotted ok".to_string())
    }
}

/// Dotted-name handler with `Mutations` effect — used to exercise the
/// policy-gate denial path on a dotted YAML name. The `deny()` helper must
/// surface the YAML form (`subagent.child`) on both the `ToolDenied` event
/// and the feed-back string, not the wire form (`subagent_child`) the LLM saw.
pub(super) struct DottedMutationTool;

#[async_trait]
impl ToolHandler for DottedMutationTool {
    fn name(&self) -> &str {
        "subagent.child"
    }
    fn description(&self) -> &str {
        "Dotted-name mutation for denial-name translation."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutations
    }
    async fn invoke(
        &self,
        _inv: ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("should not run".to_string())
    }
}

/// Transport stub that records each request's `max_tokens` field in declaration
/// order before returning the next scripted response. Used by regression tests
/// to assert the budget is decremented across iterations rather than re-sent at
/// its original value every turn.
pub(super) struct RecordingScriptedTransport {
    pub(super) responses: Mutex<VecDeque<LlmResponse>>,
    pub(super) max_tokens_seen: Mutex<Vec<Option<u32>>>,
    pub(super) messages_seen: Mutex<Vec<Vec<OrnoChatMessage>>>,
    pub(super) systems_seen: Mutex<Vec<Option<String>>>,
    pub(super) prompts_seen: Mutex<Vec<String>>,
}

impl RecordingScriptedTransport {
    pub(super) fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            max_tokens_seen: Mutex::new(Vec::new()),
            messages_seen: Mutex::new(Vec::new()),
            systems_seen: Mutex::new(Vec::new()),
            prompts_seen: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn prompts_seen(&self) -> Vec<String> {
        self.prompts_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .clone()
    }

    pub(super) fn systems_seen(&self) -> Vec<Option<String>> {
        self.systems_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .clone()
    }

    pub(super) fn max_tokens_seen(&self) -> Vec<Option<u32>> {
        self.max_tokens_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .clone()
    }

    pub(super) fn messages_seen(&self) -> Vec<Vec<OrnoChatMessage>> {
        self.messages_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl LlmTransport for RecordingScriptedTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        self.max_tokens_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .push(req.max_tokens);
        self.systems_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .push(req.system.as_deref().map(str::to_string));
        self.prompts_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .push(req.prompt.to_string());
        self.messages_seen
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .push((*req.messages).clone());
        self.responses
            .lock()
            .expect("RecordingScriptedTransport mutex poisoned")
            .pop_front()
            .ok_or_else(|| LlmError::Rejected("no more scripted responses".to_string()))
    }
}

/// Long-output tool used by truncation regression tests. Returns a string of
/// `count` ASCII bytes so byte-length assertions on the truncated payload
/// remain trivial.
pub(super) struct LongOutputTool {
    pub(super) len: usize,
}

impl LongOutputTool {
    pub(super) fn new(len: usize) -> Self {
        Self { len }
    }
}

#[async_trait]
impl ToolHandler for LongOutputTool {
    fn name(&self) -> &str {
        "LongOutputTool"
    }
    fn description(&self) -> &str {
        "Returns a long ASCII string of the configured length."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn invoke(
        &self,
        _inv: ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok("a".repeat(self.len))
    }
}

/// Resolver that answers every hostname with one fixed public address,
/// so gate tests never touch real DNS.
#[derive(Debug)]
pub(super) struct FixedResolver(pub(super) std::net::IpAddr);

impl FixedResolver {
    pub(super) fn public() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::net::IpAddr::from([93, 184, 216, 34])))
    }
}

#[async_trait]
impl HostResolver for FixedResolver {
    async fn resolve(&self, _host: &str) -> std::io::Result<Vec<std::net::IpAddr>> {
        Ok(vec![self.0])
    }
}

/// Response asking for one or more tool calls, each given as
/// `(call_id, tool name, args)`.
pub(super) fn tool_calls_response(calls: &[(&str, &str, serde_json::Value)]) -> LlmResponse {
    LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
        tool_calls: calls
            .iter()
            .map(|(id, name, args)| crate::llm::OrnoChatToolCall {
                call_id: (*id).into(),
                fn_name: (*name).into(),
                fn_arguments: args.clone(),
            })
            .collect(),
    }
}

pub(super) fn text_response(content: &str) -> LlmResponse {
    LlmResponse {
        content: content.into(),
        finish_reason: Some("stop".to_string()),
        usage: None,
        tool_calls: Vec::new(),
    }
}
