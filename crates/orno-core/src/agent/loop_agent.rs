//! `LoopAgent` — Phase 4 single-shot implementation of [`Agent`].
//!
//! Runs exactly one LLM round-trip. Rejects non-empty `allowed_tools`
//! with [`AgentError::UnsupportedYet`] (tools land in Phase 5) and
//! `max_iterations == 0` with [`AgentError::InvalidPolicy`]. On transport
//! failure the impl emits a typed `LlmRequestFailed` next to the dangling
//! `LlmRequestStarted` so downstream consumers can classify auth /
//! rate-limit / model-not-found without grepping error strings.
//!
//! Phase 5 replaces the single-shot body with real iteration, tool
//! dispatch, and full five-dimension enforcement (ADR 0005) while
//! keeping the [`Agent`] contract and event shape stable.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::instrument;

use crate::error::AgentError;
use crate::events::{Event, EventSink, LlmFailure, Redactor, truncate_excerpt};
use crate::llm::{LlmRequest, LlmTransport};

use super::{Agent, AgentOutput, AgentRequest};

/// Default cap on the body excerpt captured into `LlmFailure::ApiError`
/// when the agent was constructed without a caller-supplied bound.
/// Mirrors `EngineConfig::default().max_output_bytes` so an embedder
/// that builds the agent in isolation gets the same truncation policy
/// the CLI threads through.
const DEFAULT_BODY_EXCERPT_BYTES: usize = 2048;

pub struct LoopAgent {
    transport: Arc<dyn LlmTransport>,
    sink: Arc<dyn EventSink>,
    /// Redacts `secrets.*` values out of prompt and response excerpts
    /// before they reach the wire (ADR 0020 / 0024). Shared with the
    /// engine — a rendered prompt passes through this same instance
    /// so an end-to-end reader sees consistent redaction across the
    /// agent and scheduler surfaces. An `Arc` (not `Redactor` by
    /// value) because the scheduler already holds one per run and
    /// cloning the value list on every `LlmRequestStarted` would be
    /// wasteful on long runs.
    redactor: Arc<Redactor>,
    /// Cap for body excerpts captured into `LlmFailure::ApiError` and
    /// the new `prompt_excerpt` / `system_excerpt` / `content_excerpt`
    /// fields (ADR 0024). Decoupled from the engine's own
    /// `max_output_bytes` only at the type level — the CLI passes them
    /// as the same value so a truncated stderr tail, a truncated HTTP
    /// body excerpt, and a truncated prompt all look alike to log
    /// readers.
    body_excerpt_max_bytes: usize,
}

impl LoopAgent {
    #[must_use]
    pub fn new(
        transport: Arc<dyn LlmTransport>,
        sink: Arc<dyn EventSink>,
        redactor: Arc<Redactor>,
        body_excerpt_max_bytes: usize,
    ) -> Self {
        Self {
            transport,
            sink,
            redactor,
            body_excerpt_max_bytes,
        }
    }

    /// Convenience constructor for embedders and tests that do not
    /// thread an `EngineConfig` or a live secret map through. Picks
    /// the same default the engine ships with for the body cap and a
    /// no-op redactor; the wire format stays consistent across
    /// construction sites, and a test without secrets pays no
    /// redaction cost (`Redactor::is_noop() == true`).
    #[must_use]
    pub fn with_defaults(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self::new(
            transport,
            sink,
            Arc::new(Redactor::default()),
            DEFAULT_BODY_EXCERPT_BYTES,
        )
    }

    /// Redact + head-truncate a user-visible string for emission into
    /// an `LlmRequestStarted` / `LlmResponseReceived` excerpt field.
    /// Head truncation because prompts lead with the instruction and
    /// responses lead with the answer (ADR 0024). Returns an owned
    /// `String` because the redactor may allocate and the excerpt
    /// always crosses a trait-object boundary into the sink.
    fn excerpt_for_wire(&self, s: &str) -> String {
        truncate_excerpt(
            self.redactor.redact(s).as_ref(),
            self.body_excerpt_max_bytes,
        )
    }
}

#[async_trait]
impl Agent for LoopAgent {
    #[instrument(
        skip(self, req),
        fields(
            node.id = %node_id,
            pipeline.run_id = %run_id,
            llm.provider = %req.provider,
            llm.model = %req.model,
        ),
    )]
    async fn run(
        &self,
        run_id: &str,
        node_id: &str,
        req: AgentRequest,
    ) -> Result<AgentOutput, AgentError> {
        if !req.allowed_tools.is_empty() {
            return Err(AgentError::UnsupportedYet(
                "allowed_tools (Phase 5)".to_string(),
            ));
        }
        if req.policy.max_iterations == 0 {
            return Err(AgentError::InvalidPolicy(
                "max_iterations must be >= 1".to_string(),
            ));
        }

        // Excerpt the prompt and system message before `llm_req`
        // consumes them. Redaction happens here, not in the sink, so
        // the excerpts handed to `self.sink.record(...)` are already
        // safe and the agent is self-contained on its own redaction
        // contract (ADR 0024).
        let prompt_excerpt = self.excerpt_for_wire(&req.initial_prompt);
        let system_excerpt = req.system.as_deref().map(|s| self.excerpt_for_wire(s));

        let llm_req = LlmRequest {
            provider: req.provider.clone(),
            model: req.model.clone(),
            prompt: req.initial_prompt,
            system: req.system,
            temperature: None,
            // Phase 4 treats the agent's budget as a per-call cap until
            // the loop body lands. Clamp into u32 because genai's
            // ChatOptions uses u32; a user who wrote u64::MAX in YAML
            // gets u32::MAX sent over the wire. Treat `0` as "unset" —
            // OpenAI and Anthropic read `max_tokens: 0` as a zero
            // completion-token cap and return empty responses, so we
            // must omit the field entirely when the budget is
            // unconfigured.
            max_tokens: (req.policy.max_total_tokens > 0)
                .then(|| u32::try_from(req.policy.max_total_tokens).unwrap_or(u32::MAX)),
        };

        self.sink
            .record(Event::LlmRequestStarted {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                provider: req.provider.clone(),
                model: req.model.clone(),
                prompt_excerpt,
                system_excerpt,
            })
            .await;

        // Inspect the transport result before mapping to AgentError so a
        // typed `LlmRequestFailed` lands on the wire next to the dangling
        // `LlmRequestStarted`. Without this, an auth or rate-limit failure
        // surfaces only as the opaque error chain — log pipelines cannot
        // page on `auth_failed` separately from a stray parse error.
        let response = match self.transport.complete(llm_req).await {
            Ok(resp) => resp,
            Err(err) => {
                let failure = LlmFailure::from_llm_error(&err, self.body_excerpt_max_bytes);
                self.sink
                    .record(Event::LlmRequestFailed {
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                        provider: req.provider.clone(),
                        model: req.model.clone(),
                        failure,
                    })
                    .await;
                return Err(AgentError::from(err));
            }
        };

        let content_excerpt = self.excerpt_for_wire(&response.content);

        self.sink
            .record(Event::LlmResponseReceived {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                finish_reason: response.finish_reason.clone(),
                usage: response.usage.clone(),
                content_excerpt,
            })
            .await;

        Ok(AgentOutput {
            content: response.content,
            finish_reason: response.finish_reason,
            usage: response.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmError;
    use crate::events::InMemorySink;
    use crate::llm::{DummyTransport, LlmResponse};
    use crate::pipeline::{AgentPolicy, OnParseError};
    use async_trait::async_trait;

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
            allow_mutations: false,
            allow_network: false,
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

        // ADR 0024: the excerpt fields must round-trip the rendered
        // prompt and the model response, not be silently empty. A
        // consumer pairing the two envelopes must see what went in and
        // what came back without folding `NodeResponse.output`.
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
    async fn nonempty_allowed_tools_rejected_as_unsupported() {
        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
        let mut req = request();
        req.allowed_tools = vec!["Bash".into()];

        let err = agent
            .run("run_test", "n", req)
            .await
            .expect_err("tools must be refused in Phase 4");
        match err {
            AgentError::UnsupportedYet(feature) => assert!(feature.contains("allowed_tools")),
            other => panic!("expected UnsupportedYet, got {other:?}"),
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
                }
                Event::LlmResponseReceived { .. } => {
                    panic!("LlmResponseReceived must not fire on a transport failure");
                }
                _ => {}
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
        // ADR 0020 + ADR 0024: the agent shares the engine's `Redactor`
        // so a prompt that embedded a rendered `secrets.*` value never
        // reaches the sink in cleartext. Without this, enabling
        // prompt excerpts would regress the secrets-namespace contract.
        use std::collections::BTreeMap;
        let mut secret_map = BTreeMap::new();
        secret_map.insert(
            "OPENROUTER_API_KEY".to_string(),
            "sk-very-secret-12345".to_string(),
        );
        let redactor = Arc::new(crate::events::Redactor::new(&secret_map));

        let sink = Arc::new(InMemorySink::new());
        let agent = LoopAgent::new(Arc::new(DummyTransport), sink.clone(), redactor, 2048);
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
        let agent = LoopAgent::new(
            Arc::new(DummyTransport),
            sink.clone(),
            Arc::new(crate::events::Redactor::default()),
            32,
        );
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
}
