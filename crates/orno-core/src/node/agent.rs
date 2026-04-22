//! Adapter between the `NodeExecutor` seam and the [`Agent`] seam.
//!
//! `AgentExecutor` translates `NodeRequest::Agent` into an `AgentRequest`,
//! delegates to an `Arc<dyn Agent>`, and packages the `AgentOutput` back
//! into `NodeResponse` JSON. All loop behavior — LLM dispatch, event
//! emission, policy checks — lives inside the injected `Agent` impl
//! (`LoopAgent` today; Phase 5 grows it).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use crate::agent::{Agent, AgentOutput, AgentRequest, LoopAgent, LoopAgentConfig};
use crate::error::{AgentError, NodeError};
use crate::events::EventSink;
use crate::llm::LlmTransport;

use super::{AgentNodeRequest, NodeExecutor, NodeRequest, NodeResponse};

pub struct AgentExecutor {
    agent: Arc<dyn Agent>,
}

impl AgentExecutor {
    /// Construct an adapter backed by a caller-supplied `Agent`. Primary
    /// construction point for embedders and tests that want to swap in
    /// a fake agent implementation.
    #[must_use]
    pub fn from_agent(agent: Arc<dyn Agent>) -> Self {
        Self { agent }
    }

    /// Construct an adapter backed by a fresh [`LoopAgent`] built from
    /// the supplied [`LoopAgentConfig`]. The CLI hands transport, sink,
    /// redactor, body-excerpt cap, and the registered tool-handler
    /// set; the executor wires them into the default agent. The
    /// redactor is shared with the engine so `secrets.*` values redact
    /// consistently across agent-emitted `LlmRequestStarted` excerpts
    /// and scheduler-emitted `NodeFailure` tails (ADR 0020 / 0024).
    #[must_use]
    pub fn new(config: LoopAgentConfig) -> Self {
        Self::from_agent(Arc::new(LoopAgent::new(config)))
    }

    /// Construct an adapter backed by a [`LoopAgent`] with the default
    /// body-excerpt cap. Mirrors `LoopAgent::with_defaults` so embedders
    /// that don't thread an `EngineConfig` get the same truncation
    /// policy the CLI threads through.
    #[must_use]
    pub fn with_defaults(transport: Arc<dyn LlmTransport>, sink: Arc<dyn EventSink>) -> Self {
        Self::from_agent(Arc::new(LoopAgent::with_defaults(transport, sink)))
    }
}

#[async_trait]
impl NodeExecutor for AgentExecutor {
    async fn execute(
        &self,
        run_id: &str,
        node_id: &str,
        req: NodeRequest,
    ) -> Result<NodeResponse, NodeError> {
        let NodeRequest::Agent(AgentNodeRequest {
            agent,
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

        let agent_req = AgentRequest {
            agent_name: agent,
            initial_prompt,
            system,
            provider,
            model,
            policy,
            allowed_tools,
            // Root `kind: agent` nodes always enter at depth 0. A
            // subagent tool call from depth N dispatches the child at
            // `N + 1`; see `LoopAgent::run` for the recursion gate
            // (ADR 0006).
            depth: 0,
        };

        let output = self
            .agent
            .run(run_id, node_id, agent_req)
            .await
            .map_err(|err| agent_error_to_node(node_id, err))?;

        Ok(NodeResponse {
            node_id: node_id.to_string(),
            output: agent_output_to_json(&output),
        })
    }
}

/// Translate an [`AgentError`] into a [`NodeError`] preserving the
/// two user-facing distinctions that the dispatch layer already
/// surfaces: "feature not ready yet" stays `UnsupportedYet` so strict
/// pipelines get the same classification they got pre-adapter;
/// everything else collapses into `Execution` with the agent error as
/// the `source()` chain.
fn agent_error_to_node(id: &str, err: AgentError) -> NodeError {
    match err {
        AgentError::UnsupportedYet(feature) => NodeError::UnsupportedYet {
            id: id.to_string(),
            feature,
        },
        other => NodeError::Execution {
            id: id.to_string(),
            source: Box::new(other),
        },
    }
}

fn agent_output_to_json(out: &AgentOutput) -> serde_json::Value {
    json!({
        "content": out.content,
        "finish_reason": out.finish_reason,
        "usage": out.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentOutput, AgentRequest};
    use crate::error::AgentError;
    use crate::events::InMemorySink;
    use crate::llm::{DummyTransport, Usage};
    use crate::pipeline::{AgentPolicy, OnParseError};

    /// Hand-rolled fake `Agent` for adapter tests. Returns a preset
    /// output or a preset error — enough to exercise the two adapter
    /// branches without pulling in a transport.
    struct FakeAgent {
        result: std::sync::Mutex<Option<Result<AgentOutput, AgentError>>>,
    }

    impl FakeAgent {
        fn ok(out: AgentOutput) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(out))),
            })
        }

        fn err(err: AgentError) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(err))),
            })
        }
    }

    #[async_trait]
    impl Agent for FakeAgent {
        async fn run(
            &self,
            _run_id: &str,
            _node_id: &str,
            _req: AgentRequest,
        ) -> Result<AgentOutput, AgentError> {
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("FakeAgent called more than once")
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

    fn agent_node_request() -> NodeRequest {
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
    async fn wraps_agent_output_into_node_response_json() {
        let fake = FakeAgent::ok(AgentOutput {
            content: "hello".into(),
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 4,
                total_tokens: 7,
            }),
            iterations: 0,
            total_tokens: 7,
        });
        let exec = AgentExecutor::from_agent(fake);

        let resp = exec
            .execute("run_test", "n", agent_node_request())
            .await
            .expect("adapter must forward successful agent output");

        assert_eq!(resp.node_id, "n");
        assert_eq!(resp.output["content"], "hello");
        assert_eq!(resp.output["finish_reason"], "stop");
        assert_eq!(resp.output["usage"]["total_tokens"], 7);
    }

    #[tokio::test]
    async fn unsupported_yet_error_preserves_node_classification() {
        // The adapter must keep the "feature not ready" distinction that
        // strict pipelines rely on — collapsing it into Execution would
        // make unrelated failures indistinguishable from policy drift.
        let fake = FakeAgent::err(AgentError::UnsupportedYet("allowed_tools (Phase 5)".into()));
        let exec = AgentExecutor::from_agent(fake);

        let err = exec
            .execute("run_test", "n", agent_node_request())
            .await
            .expect_err("UnsupportedYet must propagate");
        match err {
            NodeError::UnsupportedYet { id, feature } => {
                assert_eq!(id, "n");
                assert!(feature.contains("allowed_tools"));
            }
            other => panic!("expected UnsupportedYet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_policy_error_surfaces_as_execution_with_source() {
        let fake = FakeAgent::err(AgentError::InvalidPolicy(
            "max_iterations must be >= 1".into(),
        ));
        let exec = AgentExecutor::from_agent(fake);

        let err = exec
            .execute("run_test", "n", agent_node_request())
            .await
            .expect_err("InvalidPolicy must propagate as Execution");
        match err {
            NodeError::Execution { id, source } => {
                assert_eq!(id, "n");
                assert!(
                    source.to_string().contains("invalid agent policy"),
                    "expected AgentError display in source chain, got {source}",
                );
            }
            other => panic!("expected Execution, got {other:?}"),
        }
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
}
