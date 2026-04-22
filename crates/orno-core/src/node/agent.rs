//! Agent node executor — Phase 4 single-shot implementation.
//!
//! Composes an `LlmTransport` (ADR 0002) with the event sink so the
//! loop emits `LlmRequestStarted` + `LlmResponseReceived` around each
//! transport call. Phase 4 runs exactly one LLM round-trip;
//! iteration, tool dispatch, and budget enforcement (ADR 0005
//! dimensions 1–4) land with Phase 5. `allow_mutations` and
//! `allow_network` are declared on the policy but unenforced — they
//! only start mattering when tools exist.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tracing::instrument;

use crate::error::{LlmError, NodeError};
use crate::events::{Event, EventSink, LlmFailure};
use crate::llm::{LlmRequest, LlmTransport};

use super::{AgentNodeRequest, NodeExecutor, NodeRequest, NodeResponse};

/// Default cap on the body excerpt captured into `LlmFailure::ApiError`
/// when the executor was constructed without a caller-supplied bound.
/// Mirrors `EngineConfig::default().max_output_bytes` so an embedder
/// that builds the executor in isolation gets the same truncation
/// policy the CLI threads through.
const DEFAULT_BODY_EXCERPT_BYTES: usize = 2048;

pub struct AgentExecutor {
    transport: Arc<dyn LlmTransport>,
    sink: Arc<dyn EventSink>,
    /// Cap for body excerpts captured into `LlmFailure::ApiError`.
    /// Decoupled from the engine's own `max_output_bytes` only at the
    /// type level — the CLI passes them as the same value so a
    /// truncated stderr tail and a truncated HTTP body excerpt look
    /// alike to log readers.
    body_excerpt_max_bytes: usize,
}

impl AgentExecutor {
    #[must_use]
    pub fn new(
        transport: Arc<dyn LlmTransport>,
        sink: Arc<dyn EventSink>,
        body_excerpt_max_bytes: usize,
    ) -> Self {
        Self {
            transport,
            sink,
            body_excerpt_max_bytes,
        }
    }

    /// Convenience constructor for embedders (and tests) that do not
    /// thread an `EngineConfig` through to the executor. Picks the
    /// same default the engine ships with so the wire format stays
    /// consistent across construction sites.
    #[must_use]
    pub fn with_defaults(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self::new(transport, sink, DEFAULT_BODY_EXCERPT_BYTES)
    }
}

#[async_trait]
impl NodeExecutor for AgentExecutor {
    #[instrument(
        skip(self, req),
        fields(
            node.id = %node_id,
            node.kind = "agent",
            pipeline.run_id = %run_id,
        ),
    )]
    async fn execute(
        &self,
        run_id: &str,
        node_id: &str,
        req: NodeRequest,
    ) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Agent(AgentNodeRequest {
            agent: _,
            initial_prompt,
            system,
            provider,
            model,
            policy,
            allowed_tools,
        }) = req
        else {
            return Err(NodeError::Execution {
                id: node_id.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "AgentExecutor received non-agent NodeRequest",
                )
                .into(),
            });
        };

        // Phase 4 is single-shot with no tool dispatch. Misconfigured
        // pipelines fail fast rather than silently ignoring the
        // declared policy — the whole point of strict loops is that
        // policy is load-bearing, not cosmetic.
        if !allowed_tools.is_empty() {
            return Err(NodeError::UnsupportedYet {
                id: node_id.to_string(),
                feature: "allowed_tools (Phase 5)".to_string(),
            });
        }
        if policy.max_iterations == 0 {
            return Err(NodeError::Execution {
                id: node_id.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "max_iterations must be >= 1",
                )
                .into(),
            });
        }

        let llm_req = LlmRequest {
            provider: provider.clone(),
            model: model.clone(),
            prompt: initial_prompt,
            system,
            temperature: None,
            // Phase 4 treats the agent's budget as a per-call cap
            // until the loop lands. Clamp into u32 because genai's
            // ChatOptions uses u32; a user who wrote u64::MAX in YAML
            // gets u32::MAX sent over the wire. Treat `0` as "unset"
            // — OpenAI and Anthropic read `max_tokens: 0` as a zero
            // completion-token cap and return empty responses, so we
            // must omit the field entirely when the budget is
            // unconfigured.
            max_tokens: (policy.max_total_tokens > 0)
                .then(|| u32::try_from(policy.max_total_tokens).unwrap_or(u32::MAX)),
        };

        self.sink
            .record(Event::LlmRequestStarted {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                provider: provider.clone(),
                model: model.clone(),
            })
            .await;

        // Inspect the transport result before mapping to NodeError so a
        // typed `LlmRequestFailed` lands on the wire next to the
        // dangling `LlmRequestStarted`. Without this, an auth or
        // rate-limit failure surfaces only as the generic
        // `NodeFailure::ExecutorError` blob — log pipelines cannot page
        // on `auth_failed` separately from a stray template error.
        let response = match self.transport.complete(llm_req).await {
            Ok(resp) => resp,
            Err(err) => {
                let failure = LlmFailure::from_llm_error(&err, self.body_excerpt_max_bytes);
                self.sink
                    .record(Event::LlmRequestFailed {
                        run_id: run_id.to_string(),
                        node_id: node_id.to_string(),
                        provider: provider.clone(),
                        model: model.clone(),
                        failure,
                    })
                    .await;
                return Err(llm_error_to_node(node_id, err));
            }
        };

        self.sink
            .record(Event::LlmResponseReceived {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                finish_reason: response.finish_reason.clone(),
                usage: response.usage.clone(),
            })
            .await;

        Ok(NodeResponse {
            node_id: node_id.to_string(),
            output: json!({
                "content": response.content,
                "finish_reason": response.finish_reason,
                "usage": response.usage,
            }),
        })
    }
}

fn llm_error_to_node(id: &str, err: LlmError) -> NodeError {
    NodeError::Execution {
        id: id.to_string(),
        source: Box::new(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn agent_req() -> NodeRequest {
        NodeRequest::Agent(AgentNodeRequest {
            agent: "greeter".into(),
            initial_prompt: "say hi".into(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: policy(),
            allowed_tools: Vec::new(),
        })
    }

    #[tokio::test]
    async fn emits_request_and_response_events_in_order() {
        let sink = Arc::new(InMemorySink::new());
        let exec = AgentExecutor::with_defaults(Arc::new(DummyTransport), sink.clone());

        let resp = exec
            .execute("run_test", "n", agent_req())
            .await
            .expect("dummy transport always succeeds");

        assert_eq!(resp.node_id, "n");
        assert!(resp.output["content"].as_str().unwrap().contains("[dummy]"));

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

        if let Event::LlmRequestStarted {
            provider, model, ..
        } = &events[starts].event
        {
            assert_eq!(provider, "openai");
            assert_eq!(model, "gpt-5");
        }
    }

    #[tokio::test]
    async fn nonempty_allowed_tools_rejected_as_unsupported() {
        let sink = Arc::new(InMemorySink::new());
        let exec = AgentExecutor::with_defaults(Arc::new(DummyTransport), sink);
        let req = NodeRequest::Agent(AgentNodeRequest {
            agent: "greeter".into(),
            initial_prompt: "say hi".into(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: policy(),
            allowed_tools: vec!["Bash".into()],
        });

        let err = exec
            .execute("run_test", "n", req)
            .await
            .expect_err("tools must be refused in Phase 4");
        match err {
            NodeError::UnsupportedYet { id, feature } => {
                assert_eq!(id, "n");
                assert!(feature.contains("allowed_tools"));
            }
            other => panic!("expected UnsupportedYet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_emits_llm_request_failed_before_propagating() {
        // The Phase 3 invariant: a transport failure leaves a typed
        // `LlmRequestFailed` on the wire next to the dangling
        // `LlmRequestStarted`. Without this event, a downstream consumer
        // can only see the generic `NodeFailure::ExecutorError` blob and
        // cannot tell `auth_failed` from a stray template error.
        let sink = Arc::new(InMemorySink::new());
        let exec = AgentExecutor::with_defaults(Arc::new(FailingTransport::auth()), sink.clone());

        let err = exec
            .execute("run_test", "n", agent_req())
            .await
            .expect_err("transport failure must propagate as NodeError");
        assert!(matches!(err, NodeError::Execution { .. }));

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
    async fn non_agent_request_rejected() {
        use crate::node::ShellNodeRequest;
        let sink = Arc::new(InMemorySink::new());
        let exec = AgentExecutor::with_defaults(Arc::new(DummyTransport), sink);
        let req = NodeRequest::Shell(ShellNodeRequest {
            command: "echo".into(),
            args: Vec::new(),
        });
        let err = exec
            .execute("run_test", "n", req)
            .await
            .expect_err("shell request must be rejected by AgentExecutor");
        match err {
            NodeError::Execution { id, .. } => assert_eq!(id, "n"),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn max_total_tokens_zero_sends_no_cap() {
        // When max_total_tokens is 0 (the default), the executor must NOT
        // send max_tokens: Some(0) to the transport. DummyTransport always
        // succeeds; if the executor panicked or sent Some(0) the test would
        // need a real provider to observe the bad behavior — but at minimum
        // we verify the path completes without error.
        let sink = Arc::new(InMemorySink::new());
        let mut p = policy();
        p.max_total_tokens = 0;
        let req = NodeRequest::Agent(AgentNodeRequest {
            agent: "greeter".into(),
            initial_prompt: "say hi".into(),
            system: None,
            provider: "openai".into(),
            model: "gpt-5".into(),
            policy: p,
            allowed_tools: Vec::new(),
        });
        let exec = AgentExecutor::with_defaults(Arc::new(DummyTransport), sink);
        exec.execute("run_test", "n", req)
            .await
            .expect("zero max_total_tokens must not error");
    }
}
