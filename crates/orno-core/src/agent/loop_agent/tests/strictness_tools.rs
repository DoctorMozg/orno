use super::helpers::{DottedEchoTool, EchoTool, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::AgentError;
use crate::events::{Event, InMemorySink, Redactor};
use crate::llm::{DummyTransport, dummy::ScriptedTransport};
use crate::tool::ToolEffect;
use std::sync::Arc;

#[tokio::test]
async fn unknown_tool_in_allowed_list_is_rejected_before_any_call() {
    // Phase 5 cross-checks `allowed_tools` against registered handlers at the
    // top of `run`. A name absent from the handler set terminates with
    // `UnknownToolCalled` before the LLM is even contacted.
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
async fn model_calling_unknown_tool_terminates_with_unknown_tool_called() {
    // Bounded tool surface. A tool-call turn naming a handler the agent was
    // never given must terminate with `UnknownToolCalled` — not silently drop,
    // not retry, not ask the model to pick again.
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
async fn tool_dispatch_success_feeds_result_to_next_llm_turn() {
    // Happy-path companion to the strictness tests: model calls a tool, the
    // result reaches the next LLM turn, the model emits a text response, and
    // the loop exits with `finish_reason: stop`.
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
async fn dotted_tool_name_translates_to_underscore_on_wire_and_back() {
    // The LLM sees `subagent_child` (underscore) because providers reject dots
    // in function names; when the model's tool call comes back with that wire
    // form, the loop must reverse the translation and dispatch to the
    // `subagent.child` handler.
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

    // The dotted handler's canned output ("dotted ok") must reach the next LLM
    // turn as the tool result — only possible if the wire-form tool call
    // resolved to the dotted handler.
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
async fn set_state_call_surfaces_in_agent_output_state() {
    // End-to-end check: a `ContextSelf` tool that writes through the per-call
    // `state_handle` must appear in the returned `AgentOutput.state`. The flag
    // is on, the handler is the real `SetStateHandler`, and the transport
    // scripts one SetState call followed by a text turn so the loop exits on
    // iteration two with the buffer populated.
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
