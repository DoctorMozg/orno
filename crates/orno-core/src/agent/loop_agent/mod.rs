//! `LoopAgent` — iteration-loop implementation of [`Agent`].
//!
//! Enforces the five strictness dimensions in one loop: bounded
//! iteration (`max_iterations`), bounded tool surface (`allowed_tools`
//! plus registered handlers), bounded effects (`allow_mutations` and
//! `allow_network` — denials feed back to the model as tool-result
//! strings, the loop continues), bounded resources (`max_total_tokens`,
//! `max_tool_calls`), and bounded non-determinism (delegated to the
//! transport / recording layer).
//!
//! On transport failure the impl emits a typed `LlmRequestFailed` next
//! to the dangling `LlmRequestStarted` so downstream consumers can
//! classify auth / rate-limit / model-not-found without grepping error
//! strings.
//!
//! **Subagent dispatch.** Entries in `allowed_tools` named
//! `subagent.<child>` correspond to [`SubagentHandler`] instances that
//! hold a `Weak<LoopAgent>` back-pointer into this same loop. Depth is
//! enforced here (not in the handler) so the policy gate runs before
//! any child loop entry, and the denial feeds back as a
//! tool-result string. Wire names are sanitized at the
//! `OrnoChatTool` boundary — the YAML uses dotted names but some
//! providers reject dots in `function.name`, so we translate
//! `subagent.<child>` → `subagent_<child>` before the schema reaches
//! the LLM, and reverse-translate when routing the model's tool call
//! back to a handler.
//!
//! Module layout: `mod.rs` owns the struct and its trivial helpers plus
//! all unit tests; `policy.rs` owns the effect-gate and parse-retry
//! helpers; `run.rs` owns the `impl Agent for LoopAgent` body.

mod policy;
mod run;

use std::sync::Arc;

use crate::events::{EventSink, Redactor, truncate_excerpt};
use crate::llm::LlmTransport;
use crate::tool::ToolHandler;

/// Default cap on the body excerpt captured into `LlmFailure::ApiError`
/// when the agent was constructed without a caller-supplied bound.
/// Mirrors `EngineConfig::default().max_output_bytes` so an embedder
/// that builds the agent in isolation gets the same truncation policy
/// the CLI threads through.
const DEFAULT_BODY_EXCERPT_BYTES: usize = 2048;

/// YAML-facing subagent prefix. A tool name in `allowed_tools` that
/// starts with this string is routed through the recursion-depth gate
/// before dispatch. Kept as a constant so a typo in one place does
/// not silently bypass the gate.
const SUBAGENT_PREFIX: &str = "subagent.";

/// Configuration bundle for [`LoopAgent`]. Keeps the constructor below
/// the four-parameter threshold per the project's config-struct
/// convention (CLAUDE.md). Fields are `pub` so embedders can construct
/// the struct with standard field-init syntax.
pub struct LoopAgentConfig {
    pub transport: Arc<dyn LlmTransport>,
    pub sink: Arc<dyn EventSink>,
    /// Redacts `secrets.*` values out of prompt, response, and tool
    /// excerpts before they reach the wire.
    pub redactor: Arc<Redactor>,
    /// Cap for body excerpts captured into `LlmFailure::ApiError` and
    /// the `prompt_excerpt` / `system_excerpt` / `content_excerpt` /
    /// tool-call excerpt fields. Shared with the engine's
    /// `max_output_bytes` so every truncated field looks alike to log
    /// readers.
    pub body_excerpt_max_bytes: usize,
    /// Handlers for tools the agent is allowed to invoke. An empty
    /// vector means the agent can only converse — it will receive no
    /// tool definitions and any tool-call turn from the model will
    /// route through [`AgentError::UnknownToolCalled`] since every
    /// name validates against this set.
    pub tools: Vec<Arc<dyn ToolHandler>>,
}

pub struct LoopAgent {
    config: LoopAgentConfig,
}

impl LoopAgent {
    #[must_use]
    pub fn new(config: LoopAgentConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor for embedders and tests that do not
    /// thread an `EngineConfig` or a live secret map through. Picks
    /// the same default the engine ships with for the body cap and a
    /// no-op redactor; the wire format stays consistent across
    /// construction sites, and a test without secrets pays no
    /// redaction cost (`Redactor::is_noop() == true`).
    #[must_use]
    pub fn with_defaults(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self::new(LoopAgentConfig {
            transport,
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: DEFAULT_BODY_EXCERPT_BYTES,
            tools: Vec::new(),
        })
    }

    /// Redact + head-truncate a user-visible string for emission into
    /// an excerpt field on an event envelope. Head truncation because
    /// prompts lead with the instruction and responses lead with the
    /// answer.
    fn excerpt_for_wire(&self, s: &str) -> String {
        truncate_excerpt(
            self.config.redactor.redact(s).as_ref(),
            self.config.body_excerpt_max_bytes,
        )
    }

    /// Locate the handler for a tool by its YAML-facing name. Returns
    /// `None` only when the name slipped past the `allowed_tools`
    /// cross-check — treated as `AgentError::UnknownToolCalled` at the
    /// call site.
    fn find_handler(&self, yaml_name: &str) -> Option<&Arc<dyn ToolHandler>> {
        self.config.tools.iter().find(|h| h.name() == yaml_name)
    }

    /// Translate a YAML-facing tool name (possibly containing dots, as
    /// in `subagent.contributor_vibes`) into the wire-safe form the LLM
    /// schema presents. Dotless names are returned unchanged; a new
    /// allocation only happens for the subagent case.
    fn to_wire_name(yaml_name: &str) -> String {
        if yaml_name.contains('.') {
            yaml_name.replace('.', "_")
        } else {
            yaml_name.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentRequest};
    use crate::error::{AgentError, LlmError, ToolError};
    use crate::events::{BudgetKind, Event, InMemorySink, LlmFailure};
    use crate::llm::{
        DummyTransport, LlmRequest, LlmResponse, OrnoChatMessage, OrnoChatToolCall, Usage,
        dummy::ScriptedTransport,
    };
    use crate::pipeline::{AgentConfig, AgentPolicy, OnParseError};
    use crate::tool::{ToolEffect, ToolInvocation};
    use async_trait::async_trait;
    use std::sync::{Mutex, Weak};

    /// Transport stub that returns a caller-chosen `LlmError`. Lives in
    /// the test module because production code never wants a transport
    /// that always fails — its only purpose is to exercise the
    /// `LlmRequestFailed` emission path.
    struct FailingTransport(LlmError);

    impl FailingTransport {
        fn auth() -> Self {
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

    fn policy() -> AgentPolicy {
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
        }
    }

    fn request() -> AgentRequest {
        AgentRequest {
            agent_name: "greeter".into(),
            initial_prompt: "say hi".into(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: policy(),
            allowed_tools: Vec::new(),
            depth: 0,
            parent_token_counter: None,
        }
    }

    #[tokio::test]
    async fn emits_request_and_response_events_in_order() {
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());

        let out = agent
            .run("run_test", "n", request())
            .await
            .expect("dummy transport always succeeds");

        assert!(out.content.contains("[dummy]"));

        let events = sink.snapshot();
        let starts = events
            .iter()
            .enumerate()
            .find_map(|(i, e)| matches!(e.event, Event::LlmRequestStarted { .. }).then_some(i))
            .expect("LlmRequestStarted emitted");
        let recvs = events
            .iter()
            .enumerate()
            .find_map(|(i, e)| matches!(e.event, Event::LlmResponseReceived { .. }).then_some(i))
            .expect("LlmResponseReceived emitted");
        assert!(
            starts < recvs,
            "LlmRequestStarted must precede LlmResponseReceived",
        );

        // The excerpt fields must round-trip the rendered prompt and
        // the model response, not be silently empty. A consumer pairing
        // the two envelopes must see what went in and what came back
        // without folding `NodeResponse.output`.
        if let Event::LlmRequestStarted {
            provider,
            model,
            prompt_excerpt,
            system_excerpt,
            ..
        } = &events[starts].event
        {
            assert_eq!(provider, "openai");
            assert_eq!(model, "gpt-5");
            assert!(
                prompt_excerpt.contains("say hi"),
                "prompt_excerpt must carry the rendered prompt: {prompt_excerpt:?}",
            );
            assert!(
                system_excerpt.is_none(),
                "baseline request has no system prompt — excerpt must be None, got {system_excerpt:?}",
            );
        } else {
            panic!("LlmRequestStarted event not destructurable");
        }

        if let Event::LlmResponseReceived {
            content_excerpt, ..
        } = &events[recvs].event
        {
            assert!(
                content_excerpt.contains("[dummy]"),
                "content_excerpt must carry the transport response: {content_excerpt:?}",
            );
        } else {
            panic!("LlmResponseReceived event not destructurable");
        }
    }

    #[tokio::test]
    async fn unknown_tool_in_allowed_list_is_rejected_before_any_call() {
        // Phase 5 cross-checks `allowed_tools` against registered
        // handlers at the top of `run`. A name absent from the handler
        // set terminates with `UnknownToolCalled` before the LLM is
        // even contacted.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.allowed_tools = vec!["UnregisteredTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("unknown tool must be rejected");
        match err {
            AgentError::UnknownToolCalled { name } => assert_eq!(name, "UnregisteredTool"),
            other => panic!("expected UnknownToolCalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_max_iterations_rejected_as_invalid_policy() {
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_iterations = 0;

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("zero max_iterations must be rejected");
        match err {
            AgentError::InvalidPolicy(msg) => assert!(msg.contains("max_iterations")),
            other => panic!("expected InvalidPolicy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_emits_llm_request_failed_before_propagating() {
        // The Phase 3 invariant: a transport failure leaves a typed
        // `LlmRequestFailed` on the wire next to the dangling
        // `LlmRequestStarted`. Without this event, a downstream consumer
        // can only see the opaque error chain and cannot tell
        // `auth_failed` from a stray parse error.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(FailingTransport::auth()), sink.clone());

        let err = agent
            .run("run_test", "n", request())
            .await
            .expect_err("transport failure must propagate as AgentError");
        match err {
            AgentError::Llm(LlmError::AuthFailed { provider }) => assert_eq!(provider, "openai"),
            other => panic!("expected AgentError::Llm(AuthFailed), got {other:?}"),
        }

        let events = sink.snapshot();
        let mut started_idx = None;
        let mut failed_idx = None;
        for (i, env) in events.iter().enumerate() {
            match &env.event {
                Event::LlmRequestStarted { .. } => started_idx = Some(i),
                Event::LlmRequestFailed { failure, .. } => {
                    assert!(
                        matches!(failure, LlmFailure::AuthFailed),
                        "expected AuthFailed classification, got {failure:?}",
                    );
                    failed_idx = Some(i);
                },
                Event::LlmResponseReceived { .. } => {
                    panic!("LlmResponseReceived must not fire on a transport failure");
                },
                _ => {},
            }
        }
        let started = started_idx.expect("LlmRequestStarted must still fire");
        let failed = failed_idx.expect("LlmRequestFailed must fire on transport error");
        assert!(
            started < failed,
            "LlmRequestFailed must follow LlmRequestStarted in stream order",
        );
    }

    #[tokio::test]
    async fn max_total_tokens_zero_sends_no_cap() {
        // When max_total_tokens is 0 (the default), the impl must NOT
        // send max_tokens: Some(0) to the transport. DummyTransport
        // always succeeds; if the impl panicked or sent Some(0) the
        // test would need a real provider to observe the bad behavior —
        // but at minimum we verify the path completes without error.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_total_tokens = 0;

        agent
            .run("run_test", "n", req)
            .await
            .expect("zero max_total_tokens must not error");
    }

    #[tokio::test]
    async fn system_excerpt_present_when_agent_config_declared_a_system_prompt() {
        // The sibling of the baseline test: when the agent config
        // carries a `system:` block, its redacted excerpt must reach
        // the wire so a consumer pairs request intent with the
        // behavioral contract the operator set.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());
        let mut req = request();
        req.system = Some("You are a terse assistant.".to_string());

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let got = events.iter().find_map(|e| match &e.event {
            Event::LlmRequestStarted { system_excerpt, .. } => Some(system_excerpt.clone()),
            _ => None,
        });
        assert_eq!(
            got,
            Some(Some("You are a terse assistant.".to_string())),
            "system_excerpt must round-trip the configured system prompt",
        );
    }

    #[tokio::test]
    async fn prompt_excerpt_redacts_known_secret_values() {
        // The agent shares the engine's `Redactor` so a prompt that
        // embedded a rendered `secrets.*` value never reaches the sink
        // in cleartext. Without this, enabling prompt excerpts would
        // regress the secrets-namespace contract.
        use std::collections::BTreeMap;
        let mut secret_map = BTreeMap::new();
        secret_map.insert(
            "OPENROUTER_API_KEY".to_string(),
            "sk-very-secret-12345".to_string(),
        );
        let redactor = Arc::new(Redactor::new(&secret_map));

        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(DummyTransport),
            sink: sink.clone(),
            redactor,
            body_excerpt_max_bytes: 2048,
            tools: Vec::new(),
        });
        let mut req = request();
        req.initial_prompt = "Use key sk-very-secret-12345 to authorize this request.".to_string();

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let prompt = events
            .iter()
            .find_map(|e| match &e.event {
                Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
                _ => None,
            })
            .expect("LlmRequestStarted must be present");
        assert!(
            !prompt.contains("sk-very-secret-12345"),
            "raw secret leaked into prompt_excerpt: {prompt:?}",
        );
        assert!(
            prompt.contains("***"),
            "redactor must substitute `***` for the secret value: {prompt:?}",
        );
    }

    #[tokio::test]
    async fn prompt_excerpt_truncates_at_configured_cap() {
        // A multi-KB rendered prompt must not flood the event stream.
        // Same truncation policy as LlmFailure::ApiError.body_excerpt —
        // head bytes win (the operator instruction sits at the front),
        // ellipsis marker appended when truncation happened.
        let sink = Arc::new(InMemorySink::new());
        // Explicit 32-byte cap makes truncation observable without
        // needing a megabyte prompt.
        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(DummyTransport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 32,
            tools: Vec::new(),
        });
        let mut req = request();
        req.initial_prompt = "A".repeat(1000);

        agent
            .run("run_test", "n", req)
            .await
            .expect("dummy transport always succeeds");

        let events = sink.snapshot();
        let prompt = events
            .iter()
            .find_map(|e| match &e.event {
                Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
                _ => None,
            })
            .expect("LlmRequestStarted must be present");
        assert!(
            prompt.ends_with('…'),
            "truncation marker missing from long prompt: {prompt:?}",
        );
        assert!(
            prompt.len() <= 32 + '…'.len_utf8(),
            "excerpt exceeds cap+marker ({} bytes): {prompt:?}",
            prompt.len(),
        );
        assert!(
            prompt.starts_with("AAAA"),
            "excerpt must keep the head of the prompt: {prompt:?}",
        );
    }

    #[tokio::test]
    async fn single_iteration_with_text_response_succeeds() {
        // `DummyTransport` returns a plain-text response with no tool
        // calls, so the loop exits on the first iteration with the
        // model's answer — no iteration-limit breach even at
        // `max_iterations = 1`.
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.policy.max_iterations = 1;

        agent
            .run("run_test", "n", req)
            .await
            .expect("single iteration with text response should succeed");
    }

    /// Minimal `ToolHandler` that returns a canned output for any call.
    /// Used to exercise the tool-dispatch path without real I/O.
    struct EchoTool {
        effect: ToolEffect,
        output: &'static str,
        name: &'static str,
    }

    impl EchoTool {
        fn new(effect: ToolEffect, output: &'static str) -> Self {
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

    #[tokio::test]
    async fn iteration_limit_exceeded_when_model_keeps_calling_tools() {
        // Bounded iteration. A transport that never stops emitting
        // tool-call turns must terminate the loop at `max_iterations`,
        // not spin forever.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "done"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c2", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c3", "EchoTool", serde_json::json!({})),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 2;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must exceed iteration limit");
        match err {
            AgentError::IterationLimitExceeded { max } => assert_eq!(max, 2),
            other => panic!("expected IterationLimitExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_call_budget_exceeded() {
        // Bounded resources. The second tool call in a run with
        // `max_tool_calls = 1` must terminate with the typed
        // `BudgetKind::ToolCalls` variant so downstream alerting can
        // distinguish it from a token breach.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c2", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("done"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.max_tool_calls = 1;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must exceed tool call budget");
        match err {
            AgentError::BudgetExceeded { kind } => assert!(matches!(kind, BudgetKind::ToolCalls)),
            other => panic!("expected BudgetExceeded(ToolCalls), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_calling_unknown_tool_terminates_with_unknown_tool_called() {
        // Bounded tool surface. A tool-call turn naming a handler the
        // agent was never given must terminate with `UnknownToolCalled`
        // — not silently drop, not retry, not ask the model to pick
        // again.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

        let transport = ScriptedTransport::new(vec![ScriptedTransport::tool_call_response(
            "c1",
            "HackerTool",
            serde_json::json!({}),
        )]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.allowed_tools = vec!["EchoTool".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("calling unknown tool must terminate");
        match err {
            AgentError::UnknownToolCalled { name } => assert_eq!(name, "HackerTool"),
            other => panic!("expected UnknownToolCalled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_writes_denied_feeds_back_as_tool_result_and_loop_continues() {
        // `ContextSelf` tools are gated by `allow_context_writes`. Like
        // `Mutations` / `Network` denials, a refusal is non-terminal —
        // the denial string reaches the model as a `ToolResult` and the
        // loop continues. A SetState call with the flag off must not
        // mutate the node-state buffer and must not terminate the loop.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ContextSelf, "should not run"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("noted — state writes disabled"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_context_writes = false;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("context-writes denial must feed back, not terminate");

        assert!(
            out.content.contains("state writes disabled"),
            "final output should carry the model's acknowledgment: {:?}",
            out.content,
        );
        // The denial string must be routed as the tool's `ToolResult`
        // so a downstream pair of `ToolCallRecorded` + next
        // `LlmRequestStarted` shows the gate fired.
        let events = sink.snapshot();
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must still fire for a gated ContextSelf call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.contains("allow_context_writes=false"),
                "denial excerpt must name the gate: {output_excerpt:?}",
            );
        }
        // Because no SetState write landed, the buffer stays empty and
        // `AgentOutput.state` collapses to `None`.
        assert!(
            out.state.is_none(),
            "no SetState call landed, state must be None: {:?}",
            out.state,
        );
    }

    #[tokio::test]
    async fn mutations_denied_feeds_back_as_tool_result_and_loop_continues() {
        // Bounded effects via the feed-back mechanism. A denied
        // mutation is *not* a terminal error — the denial string
        // reaches the next LLM turn as a `ToolResult` and the model
        // adapts. The loop only terminates on iteration limit, budget,
        // unknown tool, or a final text turn.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Mutations, "mutation done"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("understood, I cannot mutate"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_mutations = false;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("policy denial must not terminate the loop — it feeds back to the model");

        assert!(
            out.content.contains("cannot mutate"),
            "final output should carry the model's acknowledgment: {:?}",
            out.content,
        );

        let events = sink.snapshot();
        let denied_event = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolDenied { .. }))
            .expect("ToolDenied must fire for a mutations denial");
        if let Event::ToolDenied {
            tool_name, reason, ..
        } = &denied_event.event
        {
            assert_eq!(tool_name, "EchoTool");
            assert!(
                reason.contains("allow_mutations=false"),
                "reason was: {reason}",
            );
        } else {
            panic!("unexpected event type");
        }
    }

    #[tokio::test]
    async fn tool_dispatch_success_feeds_result_to_next_llm_turn() {
        // Happy-path companion to the strictness tests: model calls a
        // tool, the result reaches the next LLM turn, the model emits a
        // text response, and the loop exits with `finish_reason: stop`.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "file contents here"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("I read the file successfully"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("tool dispatch succeeds");
        assert!(
            out.content.contains("successfully"),
            "model's final response should be the text turn: {:?}",
            out.content,
        );
        assert_eq!(
            out.finish_reason.as_deref(),
            Some("stop"),
            "finish_reason should be stop for a completed text turn",
        );
    }

    #[tokio::test]
    async fn set_state_call_surfaces_in_agent_output_state() {
        // End-to-end check: a `ContextSelf` tool that writes through
        // the per-call `state_handle` must appear in the returned
        // `AgentOutput.state`. The flag is on, the handler is the real
        // `SetStateHandler`, and the transport scripts one SetState
        // call followed by a text turn so the loop exits on iteration
        // two with the buffer populated.
        use crate::tool::SetStateHandler;

        let sink = Arc::new(InMemorySink::new());
        let redactor = Arc::new(Redactor::default());
        let state_tool = Arc::new(SetStateHandler::new(redactor.clone(), 2048));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "SetState",
                serde_json::json!({ "key": "plan", "value": { "status": "ready" } }),
            ),
            ScriptedTransport::text_response("wrote plan"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor,
            body_excerpt_max_bytes: 2048,
            tools: vec![state_tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_context_writes = true;
        req.allowed_tools = vec!["SetState".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("SetState dispatch succeeds");

        let state = out
            .state
            .expect("state must be present after a SetState call");
        assert_eq!(state["plan"]["status"], "ready");
    }

    /// Dotted-name handler used to exercise the wire-name translation
    /// path without a full `SubagentHandler` dispatch. Returns a fixed
    /// string — the assertion is that the LLM's wire-form tool call
    /// (`subagent_child`) routes back to this handler whose YAML name
    /// contains a dot.
    struct DottedEchoTool;

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

    /// Dotted-name handler with `Mutations` effect — used to exercise
    /// the policy-gate denial path on a dotted YAML name. The `deny()`
    /// helper must surface the YAML form (`subagent.child`) on both the
    /// `ToolDenied` event and the feed-back string, not the wire form
    /// (`subagent_child`) the LLM saw.
    struct DottedMutationTool;

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

    #[tokio::test]
    async fn subagent_depth_gate_denies_when_child_depth_exceeds_max_and_emits_event() {
        // At depth N with `max_subagent_depth = 0`, any subagent call
        // would run at depth 1 which is > 0, so the gate must fire.
        // The child is never invoked; the parent receives a denial
        // string and an observability event appears on the wire.
        let sink = Arc::new(InMemorySink::new());
        let dotted = Arc::new(DottedEchoTool);

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
            ScriptedTransport::text_response("acknowledging denial"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![dotted],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_subagent_depth = 0;
        req.allowed_tools = vec!["subagent.child".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("depth-gate denial is non-terminal");
        assert!(
            out.content.contains("acknowledging denial"),
            "parent should continue after denial: {:?}",
            out.content,
        );

        let events = sink.snapshot();
        let depth_exceeded = events
            .iter()
            .find(|e| matches!(e.event, Event::SubagentDepthExceeded { .. }))
            .expect("SubagentDepthExceeded must fire on depth overflow");
        if let Event::SubagentDepthExceeded {
            attempted_child_agent,
            depth_attempted,
            max_depth,
            ..
        } = &depth_exceeded.event
        {
            assert_eq!(attempted_child_agent, "child");
            assert_eq!(*depth_attempted, 1);
            assert_eq!(*max_depth, 0);
        }

        // Denial reaches the model as a tool-result string, so the
        // ToolCallRecorded envelope carries it verbatim — that's the
        // bounded-effects feed-back contract applied to the depth case.
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must still fire for a denied subagent call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.contains("exceeding max_subagent_depth"),
                "denial excerpt should name the gate: {output_excerpt:?}",
            );
        }
    }

    #[tokio::test]
    async fn token_budget_breach_still_emits_llm_response_received() {
        // Pairing invariant: every `LlmRequestStarted` must be paired
        // with `LlmResponseReceived` (or `LlmRequestFailed`) on the
        // wire. Before the fix the token-budget check ran BEFORE the
        // response-received emission, so a breach at the end of an
        // iteration left a dangling `LlmRequestStarted` and the
        // operator saw only "agent exceeded budget" with no record of
        // the final model turn. This regression guards the ordering.
        let sink = Arc::new(InMemorySink::new());
        // `text_response` reports 15 total_tokens; cap of 10 trips on
        // the very first response.
        let transport = ScriptedTransport::new(vec![ScriptedTransport::text_response("over cap")]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: Vec::new(),
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_total_tokens = 10;

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("must trip the token budget");
        match err {
            AgentError::BudgetExceeded { kind } => assert!(matches!(kind, BudgetKind::Tokens)),
            other => panic!("expected BudgetExceeded(Tokens), got {other:?}"),
        }

        let events = sink.snapshot();
        let started_idx = events
            .iter()
            .position(|e| matches!(e.event, Event::LlmRequestStarted { .. }))
            .expect("LlmRequestStarted must fire");
        let received_idx = events
            .iter()
            .position(|e| matches!(e.event, Event::LlmResponseReceived { .. }))
            .expect("LlmResponseReceived must fire even on budget breach — pairing invariant");
        assert!(
            started_idx < received_idx,
            "LlmRequestStarted must precede LlmResponseReceived on the wire",
        );
    }

    #[tokio::test]
    async fn dotted_tool_name_translates_to_underscore_on_wire_and_back() {
        // The LLM sees `subagent_child` (underscore) because providers
        // reject dots in function names; when the model's tool call
        // comes back with that wire form, the loop must reverse the
        // translation and dispatch to the `subagent.child` handler.
        let sink = Arc::new(InMemorySink::new());
        let dotted = Arc::new(DottedEchoTool);

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
            ScriptedTransport::text_response("done"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![dotted],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        // Raise the subagent-depth budget so the gate does NOT fire —
        // this test is about name translation, not about the gate.
        req.policy.max_subagent_depth = 1;
        req.allowed_tools = vec!["subagent.child".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("dotted-name dispatch should succeed after wire translation");
        assert!(out.content.contains("done"));

        // The dotted handler's canned output ("dotted ok") must reach
        // the next LLM turn as the tool result — only possible if the
        // wire-form tool call resolved to the dotted handler.
        let events = sink.snapshot();
        let tool_call = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must fire");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &tool_call.event {
            assert!(
                output_excerpt.contains("dotted ok"),
                "handler output must be the dotted echo's fixed string: {output_excerpt:?}",
            );
        }
    }

    #[tokio::test]
    async fn tool_denied_emits_yaml_name_for_dotted_subagent() {
        // M3 regression: when a dotted YAML name (`subagent.child`) is
        // sanitized to wire form (`subagent_child`) at the schema
        // boundary, a policy denial must still surface the YAML form
        // operators wrote in their pipeline. Otherwise an event reader
        // grepping for the tool name in their YAML cannot find the
        // matching `ToolDenied` and the feed-back string back to the
        // model uses a name that does not appear anywhere in the
        // operator-authored config.
        let sink = Arc::new(InMemorySink::new());
        let dotted = Arc::new(DottedMutationTool);

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
            ScriptedTransport::text_response("acknowledging mutation denial"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![dotted],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_subagent_depth = 1;
        req.policy.allow_mutations = false;
        req.allowed_tools = vec!["subagent.child".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("mutations denial must feed back, not terminate");

        let events = sink.snapshot();
        let denied = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolDenied { .. }))
            .expect("ToolDenied must fire for a dotted-name mutations denial");
        if let Event::ToolDenied {
            tool_name, reason, ..
        } = &denied.event
        {
            assert_eq!(
                tool_name, "subagent.child",
                "ToolDenied.tool_name must be the YAML form, not the wire form `subagent_child`",
            );
            assert!(
                reason.contains("allow_mutations=false"),
                "denial reason must name the gate: {reason:?}",
            );
        } else {
            panic!("unexpected event type");
        }

        // The feed-back string back to the model must also use the YAML
        // form so the model sees consistent naming with the pipeline
        // YAML the operator wrote.
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must fire for a denied dotted-name call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.starts_with("denied: tool `subagent.child`"),
                "feed-back string must use the YAML form: {output_excerpt:?}",
            );
        }
    }

    #[tokio::test]
    async fn network_denied_feeds_back_as_tool_result_and_loop_continues() {
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "net done"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
            ScriptedTransport::text_response("understood, I cannot reach network"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = false;
        req.allowed_tools = vec!["EchoTool".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("network denial must feed back, not terminate");

        assert!(
            out.content.contains("cannot reach network"),
            "final output should carry the model's acknowledgment: {:?}",
            out.content,
        );

        let events = sink.snapshot();
        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must fire for a denied network call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.contains("allow_network=false"),
                "denial excerpt must name the gate: {output_excerpt:?}",
            );
        }
        let tool_denied = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolDenied { .. }))
            .expect("ToolDenied must fire for a network denial");
        if let Event::ToolDenied {
            tool_name, reason, ..
        } = &tool_denied.event
        {
            assert_eq!(tool_name, "EchoTool");
            assert!(
                reason.contains("allow_network=false"),
                "ToolDenied.reason must name the gate: {reason:?}",
            );
        }
    }

    #[tokio::test]
    async fn domain_blocked_feeds_back_and_emits_tool_denied_event() {
        use crate::tool::WebFetchHandler;

        let sink = Arc::new(InMemorySink::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(WebFetchHandler::default());

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "WebFetch",
                serde_json::json!({ "url": "https://evil.com/data" }),
            ),
            ScriptedTransport::text_response("acknowledging domain denial"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![handler],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.blocked_domains = vec!["evil.com".into()];
        req.allowed_tools = vec!["WebFetch".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("domain denial must feed back, not terminate");
        assert!(
            out.content.contains("acknowledging domain denial"),
            "parent should continue after denial: {:?}",
            out.content,
        );

        let events = sink.snapshot();
        let tool_denied = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolDenied { .. }))
            .expect("ToolDenied must fire for a blocked domain");
        if let Event::ToolDenied { reason, .. } = &tool_denied.event {
            assert!(
                reason.contains("evil.com"),
                "reason should name the domain: {reason:?}",
            );
            assert!(
                reason.contains("blocked_domains"),
                "reason should name the gate: {reason:?}",
            );
        }

        let recorded = events
            .iter()
            .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
            .expect("ToolCallRecorded must still fire for a domain-blocked call");
        if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
            assert!(
                output_excerpt.starts_with("denied: tool `WebFetch`"),
                "denial string must route through the tool result: {output_excerpt:?}",
            );
        }
    }

    #[tokio::test]
    async fn allowed_domains_allowlist_denies_host_not_in_list() {
        use crate::tool::WebFetchHandler;

        let sink = Arc::new(InMemorySink::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(WebFetchHandler::default());

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "WebFetch",
                serde_json::json!({ "url": "https://random.example/data" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![handler],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.allowed_domains = vec!["api.trusted.com".into()];
        req.allowed_tools = vec!["WebFetch".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("allowlist denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must fire when host is not on allowlist");
        assert!(
            reason.contains("allowed_domains"),
            "reason should name the gate: {reason:?}",
        );
        assert!(
            reason.contains("random.example"),
            "reason should name the rejected host: {reason:?}",
        );
    }

    #[tokio::test]
    async fn blocked_domains_matches_subdomain_by_suffix() {
        // Suffix matching invariant: `blocked_domains: ["evil.com"]`
        // must deny `sub.evil.com`, closing the footgun where naive
        // equality let subdomains through.
        use crate::tool::WebFetchHandler;

        let sink = Arc::new(InMemorySink::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(WebFetchHandler::default());

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "WebFetch",
                serde_json::json!({ "url": "https://sub.evil.com/data" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![handler],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.blocked_domains = vec!["evil.com".into()];
        req.allowed_tools = vec!["WebFetch".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("subdomain denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must fire for a subdomain of a blocked domain");
        assert!(
            reason.contains("sub.evil.com"),
            "reason should name the rejected host: {reason:?}",
        );
        assert!(
            reason.contains("blocked_domains"),
            "reason should name the gate: {reason:?}",
        );
    }

    #[tokio::test]
    async fn blocked_domains_does_not_match_unrelated_host_with_shared_suffix() {
        // Suffix matching must not misfire on `notevil.com` when
        // `blocked_domains: ["evil.com"]`: the match requires either
        // exact equality or a `.`-separated suffix, not a raw string
        // suffix. Without this guard an attacker could register a
        // sibling name that passes the filter.
        //
        // Uses `EchoTool` with `ToolEffect::Network` so the policy gate
        // runs (examining the `url` arg) without the handler actually
        // hitting the network.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "ok"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "EchoTool",
                serde_json::json!({ "url": "https://notevil.com/data" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.blocked_domains = vec!["evil.com".into()];
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("unrelated host must not terminate");

        let events = sink.snapshot();
        let denied = events.iter().find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        assert!(
            denied.is_none(),
            "ToolDenied must NOT fire for notevil.com when only evil.com is blocked: {denied:?}",
        );
    }

    #[tokio::test]
    async fn blocked_domains_still_matches_exact_host() {
        // Suffix matching must preserve the exact-equality case: the
        // existing `evil.com` deny-all behavior must not regress.
        use crate::tool::WebFetchHandler;

        let sink = Arc::new(InMemorySink::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(WebFetchHandler::default());

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "WebFetch",
                serde_json::json!({ "url": "https://evil.com/data" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![handler],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.blocked_domains = vec!["evil.com".into()];
        req.allowed_tools = vec!["WebFetch".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("exact-host denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must still fire on the exact blocked domain");
        assert!(
            reason.contains("evil.com"),
            "reason should name the rejected host: {reason:?}",
        );
        assert!(
            reason.contains("blocked_domains"),
            "reason should name the gate: {reason:?}",
        );
    }

    #[tokio::test]
    async fn allowed_domains_matches_subdomain_by_suffix() {
        // Mirror of the blocked-domain test: `allowed_domains:
        // ["api.trusted.com"]` must ALLOW `sub.api.trusted.com` under
        // suffix matching. An exact-match allowlist would force
        // operators to enumerate every subdomain.
        //
        // Uses `EchoTool` with `ToolEffect::Network` so the policy gate
        // runs (examining the `url` arg) without the handler actually
        // hitting the network — the sibling `blocked_domains_*` tests
        // use `WebFetchHandler` because they rely on the denial path
        // firing before any network call, but the allowed path here
        // would otherwise attempt a real DNS lookup.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "ok"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "EchoTool",
                serde_json::json!({ "url": "https://sub.api.trusted.com/data" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.policy.allowed_domains = vec!["api.trusted.com".into()];
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("subdomain of allowlisted host must not terminate");

        let events = sink.snapshot();
        let denied = events.iter().find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        assert!(
            denied.is_none(),
            "ToolDenied must NOT fire for sub.api.trusted.com on an api.trusted.com allowlist: {denied:?}",
        );
    }

    #[tokio::test]
    async fn non_http_scheme_denied() {
        // file://, ftp://, data:// are disallowed regardless of domain lists.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "EchoTool",
                serde_json::json!({ "url": "file:///etc/passwd" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("scheme denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must fire for non-HTTP scheme");
        assert!(
            reason.contains("file"),
            "reason should name the disallowed scheme: {reason:?}",
        );
    }

    #[tokio::test]
    async fn loopback_ip_denied() {
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "EchoTool",
                serde_json::json!({ "url": "http://127.0.0.1/secret" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("IP denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must fire for loopback IP");
        assert!(
            reason.contains("127.0.0.1"),
            "reason should name the IP: {reason:?}",
        );
    }

    #[tokio::test]
    async fn private_ip_denied() {
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response(
                "c1",
                "EchoTool",
                serde_json::json!({ "url": "http://192.168.1.100/internal" }),
            ),
            ScriptedTransport::text_response("ok"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink: sink.clone(),
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.allow_network = true;
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("private IP denial must feed back, not terminate");

        let events = sink.snapshot();
        let reason = events
            .iter()
            .find_map(|e| match &e.event {
                Event::ToolDenied { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .expect("ToolDenied must fire for private IP");
        assert!(
            reason.contains("192.168.1.100"),
            "reason should name the IP: {reason:?}",
        );
    }

    #[tokio::test]
    async fn retry_once_feeds_invalid_args_back_as_tool_result() {
        struct FlakyParse {
            attempts: Mutex<u32>,
        }
        #[async_trait]
        impl ToolHandler for FlakyParse {
            fn name(&self) -> &str {
                "FlakyParse"
            }
            fn description(&self) -> &str {
                "Fails with InvalidArgs once."
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
                let mut n = self.attempts.lock().expect("mutex");
                *n += 1;
                if *n == 1 {
                    Err(ToolError::InvalidArgs {
                        name: "FlakyParse".into(),
                        message: "missing field `x`".into(),
                    })
                } else {
                    Ok("second call ok".into())
                }
            }
        }

        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(FlakyParse {
            attempts: Mutex::new(0),
        });

        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::tool_call_response("c1", "FlakyParse", serde_json::json!({})),
            ScriptedTransport::tool_call_response("c2", "FlakyParse", serde_json::json!({})),
            ScriptedTransport::text_response("done"),
        ]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.on_parse_error = OnParseError::RetryOnce;
        req.allowed_tools = vec!["FlakyParse".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("RetryOnce must feed back, loop continues");
        assert!(out.content.contains("done"));
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "the inline `AlwaysInvalid` ToolHandler stub plus the dual-call response literal push the body past 60 lines; both stay inline so the test's intent is readable end-to-end"
    )]
    async fn retry_once_second_parse_error_on_same_call_id_terminates_with_parse_failed() {
        struct AlwaysInvalid;
        #[async_trait]
        impl ToolHandler for AlwaysInvalid {
            fn name(&self) -> &str {
                "AlwaysInvalid"
            }
            fn description(&self) -> &str {
                "Always returns InvalidArgs."
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
                Err(ToolError::InvalidArgs {
                    name: "AlwaysInvalid".into(),
                    message: "bad schema".into(),
                })
            }
        }

        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(AlwaysInvalid);

        // Both tool_calls share the same call_id and ride in ONE
        // assistant turn so the parse-retry de-dup runs within a
        // single iteration's tool dispatch loop. Per-iteration scope
        // (post-M2) means a fresh `retried_parse_errors` set at the
        // start of every iteration; the second identical-call_id call
        // within the same iteration still terminates.
        let same_call = OrnoChatToolCall {
            call_id: "same".into(),
            fn_name: "AlwaysInvalid".into(),
            fn_arguments: serde_json::json!({}),
        };
        let dual_call = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            tool_calls: vec![same_call.clone(), same_call],
        };
        let transport = ScriptedTransport::new(vec![dual_call]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.on_parse_error = OnParseError::RetryOnce;
        req.allowed_tools = vec!["AlwaysInvalid".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("second InvalidArgs on same call_id must terminate");
        match err {
            AgentError::ParseFailed { tool, error } => {
                assert_eq!(tool, "AlwaysInvalid");
                assert!(error.contains("bad schema"));
            },
            other => panic!("expected ParseFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn on_parse_error_fail_terminates_on_first_invalid_args() {
        struct InvalidOnce;
        #[async_trait]
        impl ToolHandler for InvalidOnce {
            fn name(&self) -> &str {
                "InvalidOnce"
            }
            fn description(&self) -> &str {
                "InvalidArgs."
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
                Err(ToolError::InvalidArgs {
                    name: "InvalidOnce".into(),
                    message: "missing x".into(),
                })
            }
        }

        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(InvalidOnce);

        let transport = ScriptedTransport::new(vec![ScriptedTransport::tool_call_response(
            "c1",
            "InvalidOnce",
            serde_json::json!({}),
        )]);

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: Arc::new(transport),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.on_parse_error = OnParseError::Fail;
        req.allowed_tools = vec!["InvalidOnce".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("OnParseError::Fail must terminate on first InvalidArgs");
        match err {
            AgentError::ParseFailed { tool, .. } => assert_eq!(tool, "InvalidOnce"),
            other => panic!("expected ParseFailed, got {other:?}"),
        }
    }

    /// Transport stub that records each request's `max_tokens` field
    /// in declaration order before returning the next scripted
    /// response. Used by the H6 regression test below to assert the
    /// budget is decremented across iterations rather than re-sent at
    /// its original value every turn.
    struct RecordingScriptedTransport {
        responses: Mutex<std::collections::VecDeque<LlmResponse>>,
        max_tokens_seen: Mutex<Vec<Option<u32>>>,
        messages_seen: Mutex<Vec<Vec<OrnoChatMessage>>>,
    }

    impl RecordingScriptedTransport {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                max_tokens_seen: Mutex::new(Vec::new()),
                messages_seen: Mutex::new(Vec::new()),
            }
        }

        fn max_tokens_seen(&self) -> Vec<Option<u32>> {
            self.max_tokens_seen
                .lock()
                .expect("RecordingScriptedTransport mutex poisoned")
                .clone()
        }

        fn messages_seen(&self) -> Vec<Vec<OrnoChatMessage>> {
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
            self.messages_seen
                .lock()
                .expect("RecordingScriptedTransport mutex poisoned")
                .push(req.messages.clone());
            self.responses
                .lock()
                .expect("RecordingScriptedTransport mutex poisoned")
                .pop_front()
                .ok_or_else(|| LlmError::Rejected("no more scripted responses".to_string()))
        }
    }

    #[tokio::test]
    async fn max_tokens_decrements_across_iterations() {
        // H6 regression: `max_tokens` must shrink each iteration by
        // the previous turn's `total_tokens`. Pre-fix the value was
        // computed once at the top of `run` and re-sent unchanged on
        // every iteration, which let a chatty agent burn through the
        // declared budget before the post-hoc total breach check
        // could fire. With `max_total_tokens=1000` and a first-iter
        // usage of 400, the second iteration must request at most
        // `Some(600)`.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

        // First response: tool call that costs 400 tokens. Second:
        // text answer terminating the loop. The recording transport
        // captures `max_tokens` on both `complete` calls.
        let first = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: Some(Usage {
                prompt_tokens: 200,
                completion_tokens: 200,
                total_tokens: 400,
            }),
            tool_calls: vec![OrnoChatToolCall {
                call_id: "c1".into(),
                fn_name: "EchoTool".into(),
                fn_arguments: serde_json::json!({}),
            }],
        };
        let second = LlmResponse {
            content: "done".into(),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
            tool_calls: Vec::new(),
        };
        let transport = Arc::new(RecordingScriptedTransport::new(vec![first, second]));

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: transport.clone(),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_total_tokens = 1000;
        req.allowed_tools = vec!["EchoTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("two iterations must complete within the token budget");

        let seen = transport.max_tokens_seen();
        assert_eq!(seen.len(), 2, "transport must observe two requests");
        assert_eq!(
            seen[0],
            Some(1000),
            "first iteration must request the full budget",
        );
        assert_eq!(
            seen[1],
            Some(600),
            "second iteration must request the remaining budget after a 400-token first turn",
        );
    }

    /// Long-output tool used by the truncation regression below. Returns
    /// a string of `count` ASCII bytes so byte-length assertions on the
    /// truncated payload remain trivial.
    struct LongOutputTool {
        len: usize,
    }

    impl LongOutputTool {
        fn new(len: usize) -> Self {
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

    #[tokio::test]
    async fn tool_output_truncated_in_conversation_history() {
        // WU-3.1 regression: a tool result whose byte length exceeds
        // `policy.max_tool_output_bytes` must be head-truncated to the
        // cap and an ellipsis marker appended before being pushed onto
        // `messages`. The assertion targets the messages observed by the
        // SECOND `complete()` call — that's the request the loop builds
        // from the first turn's tool result.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(LongOutputTool::new(1_000));

        let first = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: None,
            tool_calls: vec![OrnoChatToolCall {
                call_id: "c1".into(),
                fn_name: "LongOutputTool".into(),
                fn_arguments: serde_json::json!({}),
            }],
        };
        let second = LlmResponse {
            content: "done".into(),
            finish_reason: Some("stop".to_string()),
            usage: None,
            tool_calls: Vec::new(),
        };
        let transport = Arc::new(RecordingScriptedTransport::new(vec![first, second]));

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: transport.clone(),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_tool_calls = 1;
        req.policy.max_tool_output_bytes = Some(10);
        req.allowed_tools = vec!["LongOutputTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("loop must complete after the truncated tool result");

        let observed = transport.messages_seen();
        assert_eq!(observed.len(), 2, "transport must observe two requests");

        // Second call carries the assistant tool-call turn followed by
        // the truncated tool result.
        let second_req = &observed[1];
        let result_msg = second_req
            .iter()
            .find_map(|m| match m {
                OrnoChatMessage::ToolResult { call_id, content } if call_id == "c1" => {
                    Some(content)
                },
                _ => None,
            })
            .expect("second request must contain the tool-result message");

        // 10 bytes of 'a' + the 3-byte UTF-8 ellipsis '…'.
        assert_eq!(
            result_msg.len(),
            10 + '…'.len_utf8(),
            "truncated content must equal cap + ellipsis byte length",
        );
        assert!(
            result_msg.ends_with('…'),
            "truncated content must end with the ellipsis marker, got {result_msg:?}",
        );
        assert!(
            result_msg.starts_with("aaaaaaaaaa"),
            "truncated content must keep the first `cap` bytes",
        );
    }

    #[tokio::test]
    async fn tool_output_under_cap_passes_through_unchanged() {
        // Companion to the regression above: when the tool result fits
        // within the configured cap, the message must arrive on the next
        // request byte-for-byte with no ellipsis appended.
        let sink = Arc::new(InMemorySink::new());
        let tool = Arc::new(LongOutputTool::new(5));

        let first = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: None,
            tool_calls: vec![OrnoChatToolCall {
                call_id: "c1".into(),
                fn_name: "LongOutputTool".into(),
                fn_arguments: serde_json::json!({}),
            }],
        };
        let second = LlmResponse {
            content: "done".into(),
            finish_reason: Some("stop".to_string()),
            usage: None,
            tool_calls: Vec::new(),
        };
        let transport = Arc::new(RecordingScriptedTransport::new(vec![first, second]));

        let agent = LoopAgent::new(LoopAgentConfig {
            transport: transport.clone(),
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![tool],
        });

        let mut req = request();
        req.policy.max_iterations = 3;
        req.policy.max_tool_calls = 1;
        req.policy.max_tool_output_bytes = Some(10);
        req.allowed_tools = vec!["LongOutputTool".into()];

        agent
            .run("run_test", "n", req)
            .await
            .expect("under-cap tool result must not derail the loop");

        let observed = transport.messages_seen();
        let result_msg = observed[1]
            .iter()
            .find_map(|m| match m {
                OrnoChatMessage::ToolResult { call_id, content } if call_id == "c1" => {
                    Some(content)
                },
                _ => None,
            })
            .expect("second request must contain the tool-result message");
        assert_eq!(result_msg, "aaaaa");
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "WU-3.3 regression test that wires a parent + subagent + recording transport end-to-end; splitting it would obscure the propagation invariant being asserted"
    )]
    async fn parent_token_budget_covers_subagent_spend() {
        // WU-3.3 regression: a subagent loop's per-LLM-response usage
        // must charge against the parent loop's `max_total_tokens`. The
        // assertion runs against the recording transport's captured
        // `max_tokens` field on the parent's SECOND request — that's
        // the iteration the parent enters after the subagent returned.
        //
        // Wiring: parent budget = 100 tokens, child uses 80 tokens
        // inside a single subagent dispatch. Parent's second iteration
        // must request `Some(20)`, not `Some(100)` — the latter would
        // mean child usage was lost.
        //
        // Transport call ordering across the same recorder:
        //   [0] parent iter 0 → tool_call subagent_child (max=Some(100))
        //   [1] child  iter 0 → text answer with usage 80 (max=Some(<child policy>))
        //   [2] parent iter 1 → text answer terminating loop (max=Some(20))
        let sink = Arc::new(InMemorySink::new());

        let parent_first = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: None,
            tool_calls: vec![OrnoChatToolCall {
                call_id: "c1".into(),
                fn_name: "subagent_child".into(),
                fn_arguments: serde_json::json!({"prompt": "do thing"}),
            }],
        };
        let child_only = LlmResponse {
            content: "child done".into(),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 40,
                completion_tokens: 40,
                total_tokens: 80,
            }),
            tool_calls: Vec::new(),
        };
        let parent_second = LlmResponse {
            content: "all done".into(),
            finish_reason: Some("stop".to_string()),
            usage: None,
            tool_calls: Vec::new(),
        };
        let transport = Arc::new(RecordingScriptedTransport::new(vec![
            parent_first,
            child_only,
            parent_second,
        ]));

        // Child config: a small policy registered under the subagent
        // name. Provider/model are placeholders — the recording transport
        // does not branch on them.
        let child_config = AgentConfig {
            model: "gpt-5".into(),
            provider: "openai".into(),
            system: None,
            allowed_tools: Vec::new(),
            policy: AgentPolicy {
                max_iterations: 1,
                max_total_tokens: 1_000,
                max_tool_calls: 0,
                max_subagent_depth: 0,
                max_tool_output_bytes: None,
                allow_mutations: false,
                allow_network: false,
                allow_context_writes: false,
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
                on_parse_error: OnParseError::Fail,
            },
        };

        let event_sink: Arc<dyn EventSink> = sink.clone();
        let transport_dyn: Arc<dyn LlmTransport> = transport.clone();

        // `Arc::new_cyclic` matches the production wiring in
        // `cli/run.rs`: a `SubagentHandler` holds a `Weak<LoopAgent>`
        // back-pointer to break the Arc cycle.
        let agent: Arc<LoopAgent> = Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
            let subagent: Arc<dyn ToolHandler> = Arc::new(crate::tool::SubagentHandler::new(
                "subagent.child".into(),
                "child".into(),
                child_config,
                weak.clone(),
                event_sink.clone(),
            ));
            LoopAgent::new(LoopAgentConfig {
                transport: transport_dyn,
                sink: event_sink,
                redactor: Arc::new(Redactor::default()),
                body_excerpt_max_bytes: 256,
                tools: vec![subagent],
            })
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.max_total_tokens = 100;
        req.policy.max_tool_calls = 5;
        req.policy.max_subagent_depth = 1;
        req.allowed_tools = vec!["subagent.child".into()];

        let out = agent
            .run("run_test", "n", req)
            .await
            .expect("parent loop must terminate cleanly within budget");
        assert_eq!(
            out.total_tokens, 80,
            "parent's reported total_tokens must include the subagent's 80 tokens",
        );

        let max_tokens_seen = transport.max_tokens_seen();
        assert_eq!(
            max_tokens_seen.len(),
            3,
            "transport must observe 3 calls (parent#1, child#1, parent#2)",
        );
        assert_eq!(
            max_tokens_seen[0],
            Some(100),
            "parent's first call must request the full 100-token budget",
        );
        assert_eq!(
            max_tokens_seen[2],
            Some(20),
            "parent's second call must request 20 tokens (100 - 80 child spend)",
        );
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "WU-3.3 regression test that wires a parent + subagent + recording transport end-to-end and asserts terminal-variant behavior after the subagent overruns; splitting it would obscure the bound being proven"
    )]
    async fn parent_token_budget_breach_on_subagent_overrun() {
        // Companion: when a subagent's spend pushes the parent's running
        // total past `max_total_tokens`, the parent's NEXT iteration
        // entry detects the breach. The parent never reaches a third
        // LLM request; it terminates with `BudgetExceeded { Tokens }`
        // because the per-iteration check at the top of `run()`
        // saturates `remaining` to zero, but the post-response check
        // from the previous iteration already breached.
        //
        // Wiring: parent budget = 50; child uses 80 tokens. The loop
        // sees 80 > 50 on the post-child iteration's assistant turn
        // and terminates.
        let sink = Arc::new(InMemorySink::new());

        let parent_first = LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: None,
            tool_calls: vec![OrnoChatToolCall {
                call_id: "c1".into(),
                fn_name: "subagent_child".into(),
                fn_arguments: serde_json::json!({"prompt": "do thing"}),
            }],
        };
        let child_only = LlmResponse {
            content: "child done".into(),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 40,
                completion_tokens: 40,
                total_tokens: 80,
            }),
            tool_calls: Vec::new(),
        };
        // Parent's second response would charge another 30 tokens; the
        // post-response check totals 80 + 30 = 110 > 50 → BudgetExceeded.
        let parent_second = LlmResponse {
            content: "after child".into(),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 15,
                completion_tokens: 15,
                total_tokens: 30,
            }),
            tool_calls: Vec::new(),
        };
        let transport = Arc::new(RecordingScriptedTransport::new(vec![
            parent_first,
            child_only,
            parent_second,
        ]));

        let child_config = AgentConfig {
            model: "gpt-5".into(),
            provider: "openai".into(),
            system: None,
            allowed_tools: Vec::new(),
            policy: AgentPolicy {
                max_iterations: 1,
                max_total_tokens: 1_000,
                max_tool_calls: 0,
                max_subagent_depth: 0,
                max_tool_output_bytes: None,
                allow_mutations: false,
                allow_network: false,
                allow_context_writes: false,
                allowed_domains: Vec::new(),
                blocked_domains: Vec::new(),
                on_parse_error: OnParseError::Fail,
            },
        };

        let event_sink: Arc<dyn EventSink> = sink.clone();
        let transport_dyn: Arc<dyn LlmTransport> = transport.clone();

        let agent: Arc<LoopAgent> = Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
            let subagent: Arc<dyn ToolHandler> = Arc::new(crate::tool::SubagentHandler::new(
                "subagent.child".into(),
                "child".into(),
                child_config,
                weak.clone(),
                event_sink.clone(),
            ));
            LoopAgent::new(LoopAgentConfig {
                transport: transport_dyn,
                sink: event_sink,
                redactor: Arc::new(Redactor::default()),
                body_excerpt_max_bytes: 256,
                tools: vec![subagent],
            })
        });

        let mut req = request();
        req.policy.max_iterations = 5;
        req.policy.max_total_tokens = 50;
        req.policy.max_tool_calls = 5;
        req.policy.max_subagent_depth = 1;
        req.allowed_tools = vec!["subagent.child".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("budget breach via child spend must terminate the parent");
        assert!(
            matches!(
                err,
                AgentError::BudgetExceeded {
                    kind: BudgetKind::Tokens
                }
            ),
            "expected BudgetExceeded(Tokens), got {err:?}",
        );
    }
}
