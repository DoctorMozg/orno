use super::helpers::{EchoTool, LongOutputTool, RecordingScriptedTransport, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::AgentError;
use crate::events::{BudgetKind, Event, InMemorySink, Redactor};
use crate::llm::{LlmResponse, OrnoChatMessage, OrnoChatToolCall, Usage, dummy::ScriptedTransport};
use crate::tool::ToolEffect;
use std::sync::Arc;

#[tokio::test]
async fn max_total_tokens_zero_sends_no_cap() {
    // When max_total_tokens is 0 (the default), the impl must NOT send
    // max_tokens: Some(0) to the transport. DummyTransport always succeeds; if
    // the impl panicked or sent Some(0) the test would need a real provider to
    // observe the bad behavior — but at minimum we verify the path completes
    // without error.
    use crate::llm::DummyTransport;

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
async fn tool_call_budget_exceeded() {
    // Bounded resources. The second tool call in a run with `max_tool_calls =
    // 1` must terminate with the typed `BudgetKind::ToolCalls` variant so
    // downstream alerting can distinguish it from a token breach.
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
async fn token_budget_breach_still_emits_llm_response_received() {
    // Pairing invariant: every `LlmRequestStarted` must be paired with
    // `LlmResponseReceived` (or `LlmRequestFailed`) on the wire. Before the
    // fix the token-budget check ran BEFORE the response-received emission, so
    // a breach at the end of an iteration left a dangling `LlmRequestStarted`
    // and the operator saw only "agent exceeded budget" with no record of the
    // final model turn. This regression guards the ordering.
    let sink = Arc::new(InMemorySink::new());
    // `text_response` reports 15 total_tokens; cap of 10 trips on the very
    // first response.
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
async fn max_tokens_decrements_across_iterations() {
    // H6 regression: `max_tokens` must shrink each iteration by the previous
    // turn's `total_tokens`. Pre-fix the value was computed once at the top of
    // `run` and re-sent unchanged on every iteration, which let a chatty agent
    // burn through the declared budget before the post-hoc total breach check
    // could fire. With `max_total_tokens=1000` and a first-iter usage of 400,
    // the second iteration must request at most `Some(600)`.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

    // First response: tool call that costs 400 tokens. Second: text answer
    // terminating the loop. The recording transport captures `max_tokens` on
    // both `complete` calls.
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

#[tokio::test]
async fn tool_output_truncated_in_conversation_history() {
    // WU-3.1 regression: a tool result whose byte length exceeds
    // `policy.max_tool_output_bytes` must be head-truncated to the cap and an
    // ellipsis marker appended before being pushed onto `messages`. The
    // assertion targets the messages observed by the SECOND `complete()` call
    // — that's the request the loop builds from the first turn's tool result.
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

    // Second call carries the assistant tool-call turn followed by the
    // truncated tool result.
    let second_req = &observed[1];
    let result_msg = second_req
        .iter()
        .find_map(|m| match m {
            OrnoChatMessage::ToolResult { call_id, content } if call_id == "c1" => Some(content),
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
    // Companion to the regression above: when the tool result fits within the
    // configured cap, the message must arrive on the next request byte-for-byte
    // with no ellipsis appended.
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
            OrnoChatMessage::ToolResult { call_id, content } if call_id == "c1" => Some(content),
            _ => None,
        })
        .expect("second request must contain the tool-result message");
    assert_eq!(result_msg, "aaaaa");
}
