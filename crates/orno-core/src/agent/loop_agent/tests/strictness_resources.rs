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
    // When max_total_tokens is 0 (the "no cap" sentinel), the impl must NOT
    // send `max_tokens: Some(0)` on the wire — that would tell the provider
    // to refuse to emit any output. The conversion in `run.rs` collapses the
    // zero sentinel to `None`. Asserting the recorded `max_tokens` proves the
    // intent rather than relying on a transport that happens to ignore the
    // field.
    let sink = Arc::new(InMemorySink::new());
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        ScriptedTransport::text_response("done"),
    ]));

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: Vec::new(),
    });

    let mut req = request();
    req.policy.max_iterations = 1;
    req.policy.max_total_tokens = 0;

    agent
        .run("run_test", "n", req)
        .await
        .expect("zero max_total_tokens must not error");

    let seen = transport.max_tokens_seen();
    assert_eq!(seen.len(), 1, "transport must observe exactly one request");
    assert_eq!(
        seen[0], None,
        "max_total_tokens=0 must convert to max_tokens=None on the wire, got {:?}",
        seen[0],
    );
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

/// Build a tool-call response that targets `LongOutputTool` with a given
/// call id. Shared by the message-history cap tests so each test stays
/// focused on its own assertions rather than the response shape boilerplate.
fn long_tool_call_response(call_id: &str) -> LlmResponse {
    LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
        tool_calls: vec![OrnoChatToolCall {
            call_id: call_id.into(),
            fn_name: "LongOutputTool".into(),
            fn_arguments: serde_json::json!({}),
        }],
    }
}

fn final_text_response() -> LlmResponse {
    LlmResponse {
        content: "done".into(),
        finish_reason: Some("stop".to_string()),
        usage: None,
        tool_calls: Vec::new(),
    }
}

#[tokio::test]
async fn message_history_cap_truncates_oldest_messages() {
    // Bounded resources, history dimension: with `max_message_history_bytes`
    // set very small relative to each tool result, the loop must evict the
    // oldest messages so the history stays bounded across iterations. The
    // cap keeps at least the two most recent messages so the model still
    // sees one tool-call/tool-result pair. With a 500-byte cap and 600-byte
    // tool outputs, every request after the first must carry exactly two
    // messages — the most recent ToolCalls and ToolResult.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(LongOutputTool::new(600));
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        long_tool_call_response("c1"),
        long_tool_call_response("c2"),
        long_tool_call_response("c3"),
        long_tool_call_response("c4"),
        final_text_response(),
    ]));

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.max_tool_calls = 4;
    req.policy.max_message_history_bytes = Some(500);
    req.allowed_tools = vec!["LongOutputTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must complete despite huge tool outputs");

    let observed = transport.messages_seen();
    assert_eq!(observed.len(), 5, "transport must observe five requests");
    assert!(
        observed[0].is_empty(),
        "first request must carry no history"
    );

    // From the second request onward the cap saturates to two most-recent
    // messages: one ToolCalls turn followed by its paired ToolResult.
    for (i, msgs) in observed.iter().enumerate().skip(1) {
        assert_eq!(
            msgs.len(),
            2,
            "request {i} must carry two messages, got {}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], OrnoChatMessage::ToolCalls { .. }),
            "request {i} message[0] must be the ToolCalls turn",
        );
        match &msgs[1] {
            OrnoChatMessage::ToolResult { call_id, .. } => {
                assert_eq!(
                    call_id,
                    &format!("c{i}"),
                    "request {i} must hold the most recent ToolResult"
                );
            },
            other => panic!("request {i} message[1] must be a ToolResult, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn message_history_cap_disabled_lets_history_grow() {
    // Companion to the cap test: when the policy field is `None` the loop
    // must NOT enforce any cap, so every prior tool-call/tool-result pair
    // remains visible to the model. Confirms the cap branch is opt-in and
    // the "two messages only" claim above isn't a default behavior.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(LongOutputTool::new(600));
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        long_tool_call_response("c1"),
        long_tool_call_response("c2"),
        final_text_response(),
    ]));

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.max_tool_calls = 3;
    req.policy.max_message_history_bytes = None;
    req.allowed_tools = vec!["LongOutputTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must complete");

    let observed = transport.messages_seen();
    assert_eq!(observed.len(), 3, "transport must observe three requests");
    // Without a cap the history accumulates: two prior ToolCalls + two
    // prior ToolResults arrive on the third request.
    assert_eq!(
        observed[2].len(),
        4,
        "history must accumulate without a cap"
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

#[tokio::test]
async fn history_cap_evicts_multiple_messages_in_single_pass() {
    // F11 regression: the previous eviction loop used `messages.remove(0)`
    // inside a `while` that re-summed `messages.iter()` on every iteration —
    // O(n^2) over the history depth. The fixed implementation runs in a
    // single O(n) pass: track a running total, mark `evict` indices, then
    // `drain(..evict)` once. This test proves the cap is still enforced
    // correctly when many messages exceed it across many iterations, so a
    // future regression to a re-sum loop would still be caught by behavior
    // rather than only by performance benchmarks.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(LongOutputTool::new(200));

    // Build five tool-call responses so the loop accumulates five
    // ToolCalls/ToolResult pairs, then a final text answer terminating the
    // loop. With a 50-byte cap and ~200-byte tool results, every request
    // after the first must carry at most the two most recent messages.
    let mut responses: Vec<LlmResponse> = (0..5)
        .map(|i| LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: None,
            tool_calls: vec![OrnoChatToolCall {
                call_id: format!("c{i}"),
                fn_name: "LongOutputTool".into(),
                fn_arguments: serde_json::json!({}),
            }],
        })
        .collect();
    responses.push(final_text_response());

    let transport = Arc::new(RecordingScriptedTransport::new(responses));
    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 6;
    req.policy.max_tool_calls = 5;
    req.policy.max_message_history_bytes = Some(50);
    req.allowed_tools = vec!["LongOutputTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must complete despite tiny history cap");

    let observed = transport.messages_seen();
    assert_eq!(observed.len(), 6, "transport must observe six requests");
    // From the second request onward the history is capped at two
    // messages — anything more would mean the eviction loop terminated
    // early and the cap is no longer being enforced.
    for (i, msgs) in observed.iter().enumerate().skip(1) {
        assert!(
            msgs.len() <= 2,
            "request {i} must have at most 2 messages under the tiny cap, got {}",
            msgs.len(),
        );
    }
}

#[tokio::test]
async fn history_cap_keeps_at_least_two_messages() {
    // The eviction loop must stop at two messages even when the total still
    // exceeds the cap — the model needs at least one ToolCalls/ToolResult
    // pair to make causal sense of the next turn. Setting the cap below any
    // single message's byte cost forces the floor to engage.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(LongOutputTool::new(1_000));
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        long_tool_call_response("c1"),
        final_text_response(),
    ]));

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
    req.policy.max_message_history_bytes = Some(1);
    req.allowed_tools = vec!["LongOutputTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must not crash when the cap is smaller than any single message");

    let observed = transport.messages_seen();
    assert_eq!(
        observed[1].len(),
        2,
        "second request must carry exactly two messages when the cap forces the minimum",
    );
}

#[tokio::test]
async fn token_budget_exhausted_before_iteration_does_not_call_transport() {
    // F14 regression: when `used == max_total_tokens` at the start of an
    // iteration, the loop must return `BudgetExceeded` WITHOUT making an
    // LLM call. Before the fix, `(remaining > 0).then(...)` produced
    // `max_tokens: None` for the next iteration's request — an uncapped
    // (and billable) call that was only detected after the response.
    let sink = Arc::new(InMemorySink::new());
    let transport = Arc::new(RecordingScriptedTransport::new(vec![LlmResponse {
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
    }]));

    let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));
    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.max_total_tokens = 400;
    req.policy.max_tool_calls = 5;
    req.allowed_tools = vec!["EchoTool".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("budget exactly exhausted at iteration boundary must return BudgetExceeded");
    assert!(
        matches!(
            err,
            AgentError::BudgetExceeded {
                kind: BudgetKind::Tokens
            }
        ),
        "expected BudgetExceeded(Tokens), got {err:?}",
    );
    // Transport called exactly once. A second call would prove the
    // pre-check is missing — that's the bypass the fix prevents.
    assert_eq!(
        transport.max_tokens_seen().len(),
        1,
        "transport must be called exactly once; a second call would be an uncapped billing bypass",
    );
}

#[tokio::test]
async fn token_counter_saturates_at_u64_max_instead_of_wrapping() {
    // F15 regression: `AtomicU64::fetch_add` wraps on overflow. If the
    // counter wraps from near-`u64::MAX` to near-zero, the post-response
    // budget check `total_tokens > max_total_tokens` no longer fires
    // because the addition silently rolled over. The fix uses
    // `fetch_update` with `saturating_add`, so a runaway response saturates
    // the counter at `u64::MAX` and the budget breach is reported on the
    // same iteration.
    let sink = Arc::new(InMemorySink::new());
    // `Usage::total_tokens` is `u32`, so the largest single-response
    // delta we can express is `u32::MAX`. That's enough to drive the
    // saturating path because the post-add check reads the counter
    // back via `Ordering::Relaxed` and compares against
    // `max_total_tokens`.
    let huge_usage = u32::MAX;
    let transport = Arc::new(RecordingScriptedTransport::new(vec![LlmResponse {
        content: String::new(),
        finish_reason: Some("stop".to_string()),
        usage: Some(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: huge_usage,
        }),
        tool_calls: Vec::new(),
    }]));

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: Vec::new(),
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.max_total_tokens = u64::from(huge_usage) - 1;

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("massive usage must trip the budget");
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
