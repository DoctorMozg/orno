use super::helpers::{EchoTool, request};
use crate::agent::{
    Agent,
    loop_agent::{LoopAgent, LoopAgentConfig},
};
use crate::error::AgentError;
use crate::events::{InMemorySink, Redactor};
use crate::llm::{DummyTransport, dummy::ScriptedTransport};
use crate::tool::ToolEffect;
use std::sync::Arc;

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
async fn single_iteration_with_text_response_succeeds() {
    // `DummyTransport` returns a plain-text response with no tool calls, so
    // the loop exits on the first iteration with the model's answer — no
    // iteration-limit breach even at `max_iterations = 1`.
    let sink = Arc::new(InMemorySink::new());
    let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink);
    let mut req = request();
    req.policy.max_iterations = 1;

    agent
        .run("run_test", "n", req)
        .await
        .expect("single iteration with text response should succeed");
}

#[tokio::test]
async fn iteration_limit_exceeded_when_model_keeps_calling_tools() {
    // Bounded iteration. A transport that never stops emitting tool-call turns
    // must terminate the loop at `max_iterations`, not spin forever.
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
