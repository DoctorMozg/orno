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
use crate::events::{Event, EventSink};
use crate::llm::{LlmRequest, LlmTransport};

use super::{AgentNodeRequest, NodeExecutor, NodeRequest, NodeResponse};

pub struct AgentExecutor {
    transport: Arc<dyn LlmTransport>,
    sink: Arc<dyn EventSink>,
}

impl AgentExecutor {
    #[must_use]
    pub fn new(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self { transport, sink }
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
            // gets u32::MAX sent over the wire.
            max_tokens: Some(u32::try_from(policy.max_total_tokens).unwrap_or(u32::MAX)),
        };

        self.sink
            .record(Event::LlmRequestStarted {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                provider: provider.clone(),
                model: model.clone(),
            })
            .await;

        let response = self
            .transport
            .complete(llm_req)
            .await
            .map_err(|e| llm_error_to_node(node_id, e))?;

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
    use crate::llm::DummyTransport;
    use crate::pipeline::{AgentPolicy, OnParseError};

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
        let exec = AgentExecutor::new(Arc::new(DummyTransport), sink.clone());

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
        let exec = AgentExecutor::new(Arc::new(DummyTransport), sink);
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
    async fn non_agent_request_rejected() {
        use crate::node::ShellNodeRequest;
        let sink = Arc::new(InMemorySink::new());
        let exec = AgentExecutor::new(Arc::new(DummyTransport), sink);
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
}
