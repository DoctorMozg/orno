use std::sync::Arc;

use async_trait::async_trait;
use rstest::rstest;
use serde_json::json;

use super::helpers::{
    LongOutputTool, RecordingScriptedTransport, request, text_response, tool_calls_response,
};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::{AgentError, RecoverableKind, ToolError};
use crate::events::{InMemorySink, Redactor};
use crate::llm::OrnoChatMessage;
use crate::tool::{ReadHandler, ToolEffect, ToolHandler, ToolInvocation};

/// Handler whose every call fails with a freshly built error.
struct FailingTool {
    make_error: fn() -> ToolError,
}

#[async_trait]
impl ToolHandler for FailingTool {
    fn name(&self) -> &str {
        "FailingTool"
    }
    fn description(&self) -> &str {
        "Always fails."
    }
    fn schema(&self) -> serde_json::Value {
        json!({})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn invoke(
        &self,
        _inv: ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, ToolError> {
        Err((self.make_error)())
    }
}

fn agent_with(
    transport: Arc<RecordingScriptedTransport>,
    tools: Vec<Arc<dyn ToolHandler>>,
) -> LoopAgent {
    LoopAgent::new(LoopAgentConfig {
        transport,
        sink: Arc::new(InMemorySink::new()),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools,
    })
}

fn recoverable(kind: RecoverableKind) -> ToolError {
    ToolError::Recoverable {
        name: "FailingTool".into(),
        kind,
        message: "it went wrong".into(),
    }
}

fn last_tool_result(transport: &RecordingScriptedTransport, request_index: usize) -> String {
    transport.messages_seen()[request_index]
        .iter()
        .rev()
        .find_map(|m| match m {
            OrnoChatMessage::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("request carries a tool result")
}

#[rstest]
#[case::not_found(|| recoverable(RecoverableKind::NotFound))]
#[case::invalid_data(|| recoverable(RecoverableKind::InvalidData))]
#[case::timeout(|| recoverable(RecoverableKind::Timeout))]
#[case::no_match(|| recoverable(RecoverableKind::NoMatch))]
#[case::mcp_tool_reported(|| recoverable(RecoverableKind::ToolReported))]
#[tokio::test]
async fn recoverable_tool_error_is_fed_back_and_the_loop_continues(
    #[case] make_error: fn() -> ToolError,
) {
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_calls_response(&[("c1", "FailingTool", json!({}))]),
        text_response("recovered"),
    ]));
    let agent = agent_with(
        transport.clone(),
        vec![Arc::new(FailingTool { make_error })],
    );
    let mut req = request();
    req.policy.max_iterations = 3;
    req.allowed_tools = vec!["FailingTool".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("recoverable error must not terminate");

    assert_eq!(out.content, "recovered");
    assert_eq!(last_tool_result(&transport, 1), "error: it went wrong");
}

#[rstest]
#[case::denied(|| ToolError::Denied { reason: "outside root".into() })]
#[case::state_too_large(|| ToolError::StateTooLarge { name: "FailingTool".into(), bytes: 9, cap: 1 })]
#[case::internal_invocation(|| ToolError::Invocation {
    name: "FailingTool".into(),
    source: Box::new(std::io::Error::other("bug")),
})]
#[tokio::test]
async fn terminal_tool_error_ends_the_loop_with_tool_variant(
    #[case] make_error: fn() -> ToolError,
) {
    let transport = Arc::new(RecordingScriptedTransport::new(vec![tool_calls_response(
        &[("c1", "FailingTool", json!({}))],
    )]));
    let agent = agent_with(transport, vec![Arc::new(FailingTool { make_error })]);
    let mut req = request();
    req.policy.max_iterations = 3;
    req.allowed_tools = vec!["FailingTool".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("terminal tool error must end the loop");

    assert!(
        matches!(&err, AgentError::Tool { name, .. } if name == "FailingTool"),
        "got {err:?}",
    );
}

#[tokio::test]
async fn read_of_missing_file_returns_error_result_and_loop_reaches_final_answer() {
    let tmp = tempfile::TempDir::new().unwrap();
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_calls_response(&[("c1", "Read", json!({ "path": "nope.txt" }))]),
        text_response("no such file, moving on"),
    ]));
    let agent = agent_with(transport.clone(), vec![Arc::new(ReadHandler)]);
    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.roots = vec![tmp.path().to_path_buf()];
    req.allowed_tools = vec!["Read".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("a missing file is recoverable");

    assert_eq!(out.content, "no such file, moving on");
    let fed_back = last_tool_result(&transport, 1);
    assert!(
        fed_back.starts_with("error: ") && fed_back.contains("nope.txt"),
        "{fed_back}",
    );
}

#[tokio::test]
async fn read_outside_the_jail_still_terminates() {
    let tmp = tempfile::TempDir::new().unwrap();
    let transport = Arc::new(RecordingScriptedTransport::new(vec![tool_calls_response(
        &[("c1", "Read", json!({ "path": "../secret.txt" }))],
    )]));
    let agent = agent_with(transport, vec![Arc::new(ReadHandler)]);
    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.roots = vec![tmp.path().to_path_buf()];
    req.allowed_tools = vec!["Read".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("jail violation must terminate");

    assert!(
        matches!(
            &err,
            AgentError::Tool {
                source: ToolError::Denied { .. },
                ..
            }
        ),
        "got {err:?}",
    );
}

#[tokio::test]
async fn multi_call_turn_under_tight_history_cap_keeps_calls_paired_with_results() {
    let turn = |prefix: &str| {
        tool_calls_response(&[
            (&format!("{prefix}1"), "LongOutputTool", json!({})),
            (&format!("{prefix}2"), "LongOutputTool", json!({})),
            (&format!("{prefix}3"), "LongOutputTool", json!({})),
        ])
    };
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        turn("a"),
        turn("b"),
        turn("c"),
        text_response("done"),
    ]));
    let agent = agent_with(transport.clone(), vec![Arc::new(LongOutputTool::new(200))]);
    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.max_message_history_bytes = Some(300);
    req.allowed_tools = vec!["LongOutputTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must finish under a tight cap");

    for (i, msgs) in transport.messages_seen().iter().enumerate().skip(1) {
        assert!(
            matches!(msgs.first(), Some(OrnoChatMessage::ToolCalls { .. })),
            "request {i} must not start with a ToolResult: {msgs:?}",
        );
        let OrnoChatMessage::ToolCalls { calls } = &msgs[0] else {
            unreachable!("checked above");
        };
        let result_ids: Vec<&str> = msgs[1..]
            .iter()
            .map(|m| match m {
                OrnoChatMessage::ToolResult { call_id, .. } => call_id.as_str(),
                other => panic!("request {i}: expected only results after the call, got {other:?}"),
            })
            .collect();
        let call_ids: Vec<&str> = calls.iter().map(|c| c.call_id.as_str()).collect();
        assert_eq!(result_ids, call_ids, "request {i}: pairs must stay intact");
    }
}
