//! `on_history_overflow`: what the loop does with turns it removes to keep
//! the history under `max_message_history_bytes`.
//!
//! Byte math used below: a `LongOutputTool` turn costs 618 estimated bytes
//! (16 for the call, 2 + 600 for the result), so a 1000-byte cap overflows
//! on the second turn.

use std::sync::Arc;

use super::helpers::{LongOutputTool, RecordingScriptedTransport, request};
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::agent::{Agent, AgentOutput, AgentRequest};
use crate::error::{AgentError, LlmError};
use crate::events::{BudgetKind, Event, InMemorySink, Redactor};
use crate::llm::{
    LlmResponse, LlmTransport, OrnoChatMessage, RecordingTransport, ReplayTransport, Usage,
    dummy::ScriptedTransport,
};
use crate::pipeline::OnHistoryOverflow;

const CAP: usize = 1000;
const TURN_BYTES: u64 = 618;

fn tool_turn(call_id: &str) -> LlmResponse {
    ScriptedTransport::tool_call_response(call_id, "LongOutputTool", serde_json::json!({}))
}

fn summary(text: &str) -> LlmResponse {
    ScriptedTransport::text_response(text)
}

fn agent(transport: Arc<dyn LlmTransport>, sink: Arc<InMemorySink>) -> LoopAgent {
    LoopAgent::new(LoopAgentConfig {
        transport,
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![Arc::new(LongOutputTool::new(600))],
    })
}

fn capped_request(strategy: OnHistoryOverflow, turns: u32) -> AgentRequest {
    let mut req = request();
    req.policy.max_iterations = turns + 1;
    req.policy.max_tool_calls = turns;
    req.policy.max_message_history_bytes = Some(CAP);
    req.policy.on_history_overflow = strategy;
    req.allowed_tools = vec!["LongOutputTool".into()];
    req
}

/// `(strategy, evicted_messages, bytes_before, bytes_after)` per event.
fn compactions(sink: &InMemorySink) -> Vec<(String, u64, u64, u64)> {
    sink.snapshot()
        .into_iter()
        .filter_map(|env| match env.event {
            Event::HistoryCompacted {
                strategy,
                evicted_messages,
                bytes_before,
                bytes_after,
                cap_bytes,
                ..
            } => {
                assert_eq!(cap_bytes, CAP as u64);
                Some((strategy, evicted_messages, bytes_before, bytes_after))
            },
            _ => None,
        })
        .collect()
}

fn summary_text(msg: &OrnoChatMessage) -> &str {
    match msg {
        OrnoChatMessage::User { content } => content,
        other => panic!("history must open with the summary message, got {other:?}"),
    }
}

#[tokio::test]
async fn truncate_drops_the_oldest_turn_and_reports_it() {
    let sink = Arc::new(InMemorySink::new());
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_turn("c1"),
        tool_turn("c2"),
        summary("done"),
    ]));

    agent(transport.clone(), sink.clone())
        .run(
            "run_test",
            "n",
            capped_request(OnHistoryOverflow::Truncate, 2),
        )
        .await
        .expect("loop must complete");

    let seen = transport.messages_seen();
    assert_eq!(seen.len(), 3, "truncate must not add an LLM call");
    assert!(
        matches!(&seen[2][..], [OrnoChatMessage::ToolCalls { .. }, OrnoChatMessage::ToolResult { call_id, .. }] if call_id == "c2")
    );
    assert_eq!(
        compactions(&sink),
        vec![("truncate".to_string(), 2, 2 * TURN_BYTES, TURN_BYTES)],
    );
}

#[tokio::test]
async fn summarize_replaces_the_oldest_turn_with_a_summary() {
    let sink = Arc::new(InMemorySink::new());
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_turn("c1"),
        tool_turn("c2"),
        summary("read c1, nothing found"),
        summary("done"),
    ]));

    let out = agent(transport.clone(), sink.clone())
        .run(
            "run_test",
            "n",
            capped_request(OnHistoryOverflow::Summarize, 2),
        )
        .await
        .expect("loop must complete");
    assert_eq!(out.content, "done");

    // The third call is the summary: no tools, no history, its own system
    // prompt, the task and the evicted turn flattened into the prompt.
    let prompt = &transport.prompts_seen()[2];
    assert!(transport.messages_seen()[2].is_empty());
    assert!(
        transport.systems_seen()[2]
            .as_deref()
            .is_some_and(|s| s.contains("compress"))
    );
    assert!(prompt.contains("say hi"), "task missing: {prompt}");
    assert!(prompt.contains("TOOL CALL c1 LongOutputTool({})"));
    assert!(prompt.contains("TOOL RESULT c1:"));
    assert!(
        !prompt.contains("c2"),
        "the newest turn must stay out of the summary"
    );
    // Capped at an eighth of the cap, about 4 bytes per token.
    assert_eq!(transport.max_tokens_seen()[2], Some(31));

    let last = &transport.messages_seen()[3];
    assert_eq!(last.len(), 3, "summary + newest turn: {last:?}");
    let text = summary_text(&last[0]);
    assert!(text.starts_with("[orno: summary of earlier turns"));
    assert!(text.ends_with("read c1, nothing found"));

    let summary_bytes = text.len() as u64;
    assert_eq!(
        compactions(&sink),
        vec![(
            "summarize".to_string(),
            2,
            2 * TURN_BYTES,
            TURN_BYTES + summary_bytes
        )],
    );
}

#[tokio::test]
async fn a_later_summary_folds_in_the_earlier_one() {
    let sink = Arc::new(InMemorySink::new());
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_turn("c1"),
        tool_turn("c2"),
        summary("first summary"),
        tool_turn("c3"),
        summary("second summary"),
        summary("done"),
    ]));

    agent(transport.clone(), sink.clone())
        .run(
            "run_test",
            "n",
            capped_request(OnHistoryOverflow::Summarize, 3),
        )
        .await
        .expect("loop must complete");

    let second_prompt = &transport.prompts_seen()[4];
    assert!(second_prompt.contains("USER:\n[orno: summary of earlier turns"));
    assert!(second_prompt.contains("first summary"));
    assert!(second_prompt.contains("TOOL RESULT c2:"));

    let last = &transport.messages_seen()[5];
    assert_eq!(last.len(), 3, "one summary + newest turn: {last:?}");
    assert!(summary_text(&last[0]).ends_with("second summary"));
    assert_eq!(compactions(&sink).len(), 2);
}

#[tokio::test]
async fn summary_tokens_count_against_the_budget() {
    let sink = Arc::new(InMemorySink::new());
    let mut costly = summary("summary");
    costly.usage = Some(Usage {
        prompt_tokens: 900,
        completion_tokens: 200,
        total_tokens: 1100,
    });
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_turn("c1"),
        tool_turn("c2"),
        costly,
    ]));

    let err = agent(transport, sink)
        .run(
            "run_test",
            "n",
            capped_request(OnHistoryOverflow::Summarize, 2),
        )
        .await
        .expect_err("a summary over max_total_tokens must end the node");
    assert!(
        matches!(
            err,
            AgentError::BudgetExceeded {
                kind: BudgetKind::Tokens
            }
        ),
        "got {err:?}",
    );
}

#[tokio::test]
async fn a_failed_summary_call_ends_the_node() {
    let sink = Arc::new(InMemorySink::new());
    // Nothing scripted for the summary call, so the transport rejects it.
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_turn("c1"),
        tool_turn("c2"),
    ]));

    let err = agent(transport, sink.clone())
        .run(
            "run_test",
            "n",
            capped_request(OnHistoryOverflow::Summarize, 2),
        )
        .await
        .expect_err("a failed summary call must end the node");
    assert!(
        matches!(err, AgentError::Llm(LlmError::Rejected(_))),
        "got {err:?}"
    );
    let events = sink.snapshot();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::LlmRequestFailed { .. }))
    );
    assert!(compactions(&sink).is_empty());
}

#[tokio::test]
async fn a_summarized_run_replays_from_its_tape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tape = dir.path().join("tape.ndjson");
    let script = vec![
        tool_turn("c1"),
        tool_turn("c2"),
        summary("summary"),
        summary("done"),
    ];

    let recorder = Arc::new(
        RecordingTransport::create(
            Arc::new(ScriptedTransport::new(script)),
            &tape,
            Arc::new(Redactor::default()),
        )
        .expect("create tape"),
    );
    let run = |transport: Arc<dyn LlmTransport>| async move {
        let sink = Arc::new(InMemorySink::new());
        let out: AgentOutput = agent(transport, sink.clone())
            .run(
                "run_test",
                "n",
                capped_request(OnHistoryOverflow::Summarize, 2),
            )
            .await
            .expect("loop must complete");
        (out.content, compactions(&sink))
    };

    let live = run(recorder.clone()).await;
    recorder.flush().expect("flush tape");
    let replayed = run(Arc::new(ReplayTransport::load(&tape).expect("load tape"))).await;

    assert_eq!(replayed, live);
    assert_eq!(replayed.1.len(), 1, "the replay must summarize too");
}
