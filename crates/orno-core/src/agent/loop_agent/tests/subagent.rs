use super::helpers::{DottedEchoTool, RecordingScriptedTransport, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::AgentError;
use crate::events::{BudgetKind, Event, EventSink, InMemorySink, Redactor};
use crate::llm::{LlmResponse, LlmTransport, OrnoChatToolCall, Usage, dummy::ScriptedTransport};
use crate::pipeline::{AgentConfig, AgentPolicy, OnParseError};
use crate::tool::ToolHandler;
use std::sync::{Arc, Weak};

#[tokio::test]
async fn subagent_depth_gate_denies_when_child_depth_exceeds_max_and_emits_event() {
    // At depth N with `max_subagent_depth = 0`, any subagent call would run at
    // depth 1 which is > 0, so the gate must fire. The child is never invoked;
    // the parent receives a denial string and an observability event appears on
    // the wire.
    let sink = Arc::new(InMemorySink::new());
    let dotted = Arc::new(DottedEchoTool);

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
        ScriptedTransport::text_response("acknowledging denial"),
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
    req.policy.max_subagent_depth = 0;
    req.allowed_tools = vec!["subagent.child".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("depth-gate denial is non-terminal");
    assert!(
        out.content.contains("acknowledging denial"),
        "parent should continue after denial: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let depth_exceeded = events
        .iter()
        .find(|e| matches!(e.event, Event::SubagentDepthExceeded { .. }))
        .expect("SubagentDepthExceeded must fire on depth overflow");
    if let Event::SubagentDepthExceeded {
        attempted_child_agent,
        depth_attempted,
        max_depth,
        ..
    } = &depth_exceeded.event
    {
        assert_eq!(attempted_child_agent, "child");
        assert_eq!(*depth_attempted, 1);
        assert_eq!(*max_depth, 0);
    }

    // Denial reaches the model as a tool-result string, so the
    // ToolCallRecorded envelope carries it verbatim — that's the
    // bounded-effects feed-back contract applied to the depth case.
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must still fire for a denied subagent call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.contains("exceeding max_subagent_depth"),
            "denial excerpt should name the gate: {output_excerpt:?}",
        );
    }
}

fn child_agent_config() -> AgentConfig {
    AgentConfig {
        model: "gpt-5".into(),
        provider: "openai".into(),
        system: None,
        allowed_tools: Vec::new(),
        policy: AgentPolicy {
            max_iterations: 1,
            max_total_tokens: 1_000,
            max_tool_calls: 0,
            max_subagent_depth: 0,
            max_tool_output_bytes: None,
            allow_mutations: false,
            allow_network: false,
            allow_context_writes: false,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
            roots: Vec::new(),
            on_parse_error: OnParseError::Fail,
            max_message_history_bytes: None,
        },
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "WU-3.3 regression test that wires a parent + subagent + recording transport end-to-end; splitting it would obscure the propagation invariant being asserted"
)]
async fn parent_token_budget_covers_subagent_spend() {
    // WU-3.3 regression: a subagent loop's per-LLM-response usage must charge
    // against the parent loop's `max_total_tokens`. The assertion runs against
    // the recording transport's captured `max_tokens` field on the parent's
    // SECOND request — that's the iteration the parent enters after the
    // subagent returned.
    //
    // Wiring: parent budget = 100 tokens, child uses 80 tokens inside a single
    // subagent dispatch. Parent's second iteration must request `Some(20)`, not
    // `Some(100)` — the latter would mean child usage was lost.
    //
    // Transport call ordering across the same recorder:
    //   [0] parent iter 0 → tool_call subagent_child (max=Some(100))
    //   [1] child  iter 0 → text answer with usage 80 (max=Some(<child policy>))
    //   [2] parent iter 1 → text answer terminating loop (max=Some(20))
    let sink = Arc::new(InMemorySink::new());

    let parent_first = LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
        tool_calls: vec![OrnoChatToolCall {
            call_id: "c1".into(),
            fn_name: "subagent_child".into(),
            fn_arguments: serde_json::json!({"prompt": "do thing"}),
        }],
    };
    let child_only = LlmResponse {
        content: "child done".into(),
        finish_reason: Some("stop".to_string()),
        usage: Some(Usage {
            prompt_tokens: 40,
            completion_tokens: 40,
            total_tokens: 80,
        }),
        tool_calls: Vec::new(),
    };
    let parent_second = LlmResponse {
        content: "all done".into(),
        finish_reason: Some("stop".to_string()),
        usage: None,
        tool_calls: Vec::new(),
    };
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        parent_first,
        child_only,
        parent_second,
    ]));

    let event_sink: Arc<dyn EventSink> = sink.clone();
    let transport_dyn: Arc<dyn LlmTransport> = transport.clone();

    // `Arc::new_cyclic` matches the production wiring in `cli/run.rs`: a
    // `SubagentHandler` holds a `Weak<LoopAgent>` back-pointer to break the
    // Arc cycle.
    let agent: Arc<LoopAgent> = Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
        let subagent: Arc<dyn ToolHandler> = Arc::new(crate::tool::SubagentHandler::new(
            "subagent.child".into(),
            "child".into(),
            child_agent_config(),
            weak.clone(),
            event_sink.clone(),
        ));
        LoopAgent::new(LoopAgentConfig {
            transport: transport_dyn,
            sink: event_sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![subagent],
        })
    });

    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.max_total_tokens = 100;
    req.policy.max_tool_calls = 5;
    req.policy.max_subagent_depth = 1;
    req.allowed_tools = vec!["subagent.child".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("parent loop must terminate cleanly within budget");
    assert_eq!(
        out.total_tokens, 80,
        "parent's reported total_tokens must include the subagent's 80 tokens",
    );

    let max_tokens_seen = transport.max_tokens_seen();
    assert_eq!(
        max_tokens_seen.len(),
        3,
        "transport must observe 3 calls (parent#1, child#1, parent#2)",
    );
    assert_eq!(
        max_tokens_seen[0],
        Some(100),
        "parent's first call must request the full 100-token budget",
    );
    assert_eq!(
        max_tokens_seen[2],
        Some(20),
        "parent's second call must request 20 tokens (100 - 80 child spend)",
    );
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "WU-3.3 regression test that wires a parent + subagent + recording transport end-to-end and asserts terminal-variant behavior after the subagent overruns; splitting it would obscure the bound being proven"
)]
async fn parent_token_budget_breach_on_subagent_overrun() {
    // Companion: when a subagent's spend pushes the parent's running total past
    // `max_total_tokens`, the parent's NEXT iteration entry detects the breach.
    // The parent never reaches a third LLM request; it terminates with
    // `BudgetExceeded { Tokens }` because the per-iteration check at the top
    // of `run()` saturates `remaining` to zero, but the post-response check
    // from the previous iteration already breached.
    //
    // Wiring: parent budget = 50; child uses 80 tokens. The loop sees 80 > 50
    // on the post-child iteration's assistant turn and terminates.
    let sink = Arc::new(InMemorySink::new());

    let parent_first = LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: None,
        tool_calls: vec![OrnoChatToolCall {
            call_id: "c1".into(),
            fn_name: "subagent_child".into(),
            fn_arguments: serde_json::json!({"prompt": "do thing"}),
        }],
    };
    let child_only = LlmResponse {
        content: "child done".into(),
        finish_reason: Some("stop".to_string()),
        usage: Some(Usage {
            prompt_tokens: 40,
            completion_tokens: 40,
            total_tokens: 80,
        }),
        tool_calls: Vec::new(),
    };
    // Parent's second response would charge another 30 tokens; the
    // post-response check totals 80 + 30 = 110 > 50 → BudgetExceeded.
    let parent_second = LlmResponse {
        content: "after child".into(),
        finish_reason: Some("stop".to_string()),
        usage: Some(Usage {
            prompt_tokens: 15,
            completion_tokens: 15,
            total_tokens: 30,
        }),
        tool_calls: Vec::new(),
    };
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        parent_first,
        child_only,
        parent_second,
    ]));

    let event_sink: Arc<dyn EventSink> = sink.clone();
    let transport_dyn: Arc<dyn LlmTransport> = transport.clone();

    let agent: Arc<LoopAgent> = Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
        let subagent: Arc<dyn ToolHandler> = Arc::new(crate::tool::SubagentHandler::new(
            "subagent.child".into(),
            "child".into(),
            child_agent_config(),
            weak.clone(),
            event_sink.clone(),
        ));
        LoopAgent::new(LoopAgentConfig {
            transport: transport_dyn,
            sink: event_sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools: vec![subagent],
        })
    });

    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.max_total_tokens = 50;
    req.policy.max_tool_calls = 5;
    req.policy.max_subagent_depth = 1;
    req.allowed_tools = vec!["subagent.child".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("budget breach via child spend must terminate the parent");
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
