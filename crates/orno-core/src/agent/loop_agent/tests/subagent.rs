use super::helpers::{
    DottedEchoTool, RecordingScriptedTransport, request, text_response, tool_calls_response,
};
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
            unconfined_shell: false,
        },
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "wires a parent, a subagent and a recording transport end-to-end; splitting would obscure the propagation invariant"
)]
async fn parent_token_budget_covers_subagent_spend() {
    // A subagent loop's per-LLM-response usage must charge
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
    reason = "wires a parent, a subagent and a recording transport end-to-end and asserts the terminal variant after an overrun; splitting would obscure the bound"
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

#[rstest::rstest]
#[case::dotted_yaml_name("subagent.child")]
#[case::wire_name("subagent_child")]
#[tokio::test]
async fn unlisted_subagent_call_terminates_with_unknown_tool(#[case] called: &str) {
    // `subagent.child` is registered on the agent but absent from
    // `allowed_tools`, so neither spelling may reach its handler.
    let transport = ScriptedTransport::new(vec![ScriptedTransport::tool_call_response(
        "c1",
        called,
        serde_json::json!({}),
    )]);
    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: Arc::new(InMemorySink::new()),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![Arc::new(DottedEchoTool)],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.max_subagent_depth = 1;
    req.allowed_tools = Vec::new();

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("unlisted subagent must terminate the loop");
    assert!(
        matches!(&err, AgentError::UnknownToolCalled { name } if name == called),
        "got {err:?}",
    );
}

/// Tool that counts how many times it ran.
struct CountingTool(std::sync::atomic::AtomicU32);

#[async_trait::async_trait]
impl ToolHandler for CountingTool {
    fn name(&self) -> &str {
        "CountingTool"
    }
    fn description(&self) -> &str {
        "Counts calls."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn effect(&self) -> crate::tool::ToolEffect {
        crate::tool::ToolEffect::ReadOnly
    }
    async fn invoke(
        &self,
        _inv: crate::tool::ToolInvocation<'_>,
        _args: serde_json::Value,
    ) -> Result<String, crate::error::ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok("counted".into())
    }
}

/// Parent loop wired with a `SubagentHandler` per `(name, config)` pair, the
/// same way the CLI does it.
fn agent_with_subagents(
    transport: Arc<dyn LlmTransport>,
    sink: Arc<dyn EventSink>,
    children: Vec<(&'static str, AgentConfig)>,
    extra_tools: Vec<Arc<dyn ToolHandler>>,
) -> Arc<LoopAgent> {
    Arc::new_cyclic(|weak: &Weak<LoopAgent>| {
        let mut tools = extra_tools;
        for (name, config) in children {
            tools.push(Arc::new(crate::tool::SubagentHandler::new(
                format!("subagent.{name}"),
                name.into(),
                config,
                weak.clone(),
                sink.clone(),
            )));
        }
        LoopAgent::new(LoopAgentConfig {
            transport,
            sink,
            redactor: Arc::new(Redactor::default()),
            body_excerpt_max_bytes: 256,
            tools,
        })
    })
}

#[tokio::test]
async fn grandchild_token_spend_trips_the_root_budget() {
    // The grandchild's own limit is generous; only the root's 100 can stop it.
    let mut child = child_agent_config();
    child.allowed_tools = vec!["subagent.grandchild".into()];
    child.policy.max_iterations = 3;
    child.policy.max_subagent_depth = 2;
    let mut grandchild = child_agent_config();
    grandchild.policy.max_total_tokens = 10_000;

    let grandchild_reply = LlmResponse {
        usage: Some(Usage {
            prompt_tokens: 75,
            completion_tokens: 75,
            total_tokens: 150,
        }),
        ..text_response("grandchild done")
    };
    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_calls_response(&[("c1", "subagent_child", serde_json::json!({"prompt": "go"}))]),
        tool_calls_response(&[(
            "c2",
            "subagent_grandchild",
            serde_json::json!({"prompt": "go deeper"}),
        )]),
        grandchild_reply,
        text_response("never requested"),
    ]));
    let agent = agent_with_subagents(
        transport.clone(),
        Arc::new(InMemorySink::new()),
        vec![("child", child), ("grandchild", grandchild)],
        Vec::new(),
    );

    let mut req = request();
    req.policy.max_iterations = 4;
    req.policy.max_total_tokens = 100;
    req.policy.max_subagent_depth = 2;
    req.allowed_tools = vec!["subagent.child".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("depth-2 spend must reach the root budget");

    assert!(
        matches!(
            err,
            AgentError::BudgetExceeded {
                kind: BudgetKind::Tokens
            }
        ),
        "got {err:?}",
    );
    assert_eq!(
        transport.max_tokens_seen().len(),
        3,
        "no LLM call may follow the breach",
    );
}

#[tokio::test]
async fn child_tool_calls_count_against_the_parent_limit() {
    let mut child = child_agent_config();
    child.allowed_tools = vec!["CountingTool".into()];
    child.policy.max_iterations = 5;
    let counting = Arc::new(CountingTool(std::sync::atomic::AtomicU32::new(0)));

    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_calls_response(&[("c1", "subagent_child", serde_json::json!({"prompt": "go"}))]),
        tool_calls_response(&[("c2", "CountingTool", serde_json::json!({}))]),
        tool_calls_response(&[("c3", "CountingTool", serde_json::json!({}))]),
        text_response("parent carries on"),
    ]));
    let sink = Arc::new(InMemorySink::new());
    let agent = agent_with_subagents(
        transport,
        sink.clone(),
        vec![("child", child)],
        vec![counting.clone()],
    );

    let mut req = request();
    req.policy.max_iterations = 4;
    // One call for the subagent itself plus one child call fit; the second child call does not.
    req.policy.max_tool_calls = 2;
    req.policy.max_subagent_depth = 1;
    req.allowed_tools = vec!["subagent.child".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("the parent survives its child's failure");

    assert_eq!(
        counting.0.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the child's second call must be refused",
    );
    let failed = sink.snapshot().into_iter().find_map(|e| match e.event {
        Event::SubagentFailed { error, .. } => Some(error),
        _ => None,
    });
    assert!(
        failed.is_some_and(|e| e.contains("ToolCalls")),
        "child must fail on the tool-call budget",
    );
}

fn templated_child(roots: Vec<std::path::PathBuf>) -> AgentConfig {
    let mut child = child_agent_config();
    child.system = Some("You are {{ vars.name }}".into());
    child.allowed_tools = vec!["Read".into()];
    child.policy.max_iterations = 3;
    child.policy.roots = roots;
    child
}

fn render_scope(vars: &serde_json::Value) -> crate::pipeline::template::RenderScope {
    crate::pipeline::template::RenderScope::new(
        Arc::new(crate::pipeline::template::TemplateEngine::new()),
        Arc::new(serde_json::json!({ "vars": vars })),
    )
}

#[tokio::test]
async fn child_system_and_roots_are_rendered_against_the_parent_scope() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("sub/hello.txt"), "hello contents").unwrap();
    let root = tmp.path().to_path_buf();

    let transport = Arc::new(RecordingScriptedTransport::new(vec![
        tool_calls_response(&[("c1", "subagent_child", serde_json::json!({"prompt": "go"}))]),
        tool_calls_response(&[("c2", "Read", serde_json::json!({"path": "hello.txt"}))]),
        text_response("child done"),
        text_response("parent done"),
    ]));
    let agent = agent_with_subagents(
        transport.clone(),
        Arc::new(InMemorySink::new()),
        vec![("child", templated_child(vec!["{{ vars.root }}/sub".into()]))],
        vec![Arc::new(crate::tool::ReadHandler)],
    );

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.max_subagent_depth = 1;
    req.policy.roots = vec![root.clone()];
    req.allowed_tools = vec!["subagent.child".into()];
    req.render_scope = Some(render_scope(
        &serde_json::json!({ "name": "alpha", "root": root }),
    ));

    let out = agent.run("run_test", "n", req).await.expect("run succeeds");

    assert_eq!(out.content, "parent done");
    assert_eq!(
        transport.systems_seen()[1].as_deref(),
        Some("You are alpha"),
        "child system prompt must be rendered",
    );
    let child_second_request = &transport.messages_seen()[2];
    assert!(
        child_second_request.iter().any(|m| matches!(
            m,
            crate::llm::OrnoChatMessage::ToolResult { content, .. } if content == "hello contents"
        )),
        "the rendered root must let the child read inside it: {child_second_request:?}",
    );
}

#[rstest::rstest]
#[case::sibling_directory("{{ vars.outside }}")]
#[case::parent_dir_component("{{ vars.root }}/../escape")]
#[tokio::test]
async fn rendered_child_root_outside_the_parent_roots_is_rejected(#[case] child_root: &str) {
    let parent_dir = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();

    let transport = Arc::new(RecordingScriptedTransport::new(vec![tool_calls_response(
        &[("c1", "subagent_child", serde_json::json!({"prompt": "go"}))],
    )]));
    let agent = agent_with_subagents(
        transport.clone(),
        Arc::new(InMemorySink::new()),
        vec![("child", templated_child(vec![child_root.into()]))],
        vec![Arc::new(crate::tool::ReadHandler)],
    );

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.max_subagent_depth = 1;
    req.policy.roots = vec![parent_dir.path().to_path_buf()];
    req.allowed_tools = vec!["subagent.child".into()];
    req.render_scope = Some(render_scope(&serde_json::json!({
        "name": "alpha",
        "root": parent_dir.path(),
        "outside": outside.path(),
    })));

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("a root outside the parent's must be a terminal policy error");

    assert!(
        matches!(
            &err,
            AgentError::Tool {
                source: crate::error::ToolError::Denied { .. },
                ..
            }
        ),
        "got {err:?}",
    );
    assert_eq!(
        transport.max_tokens_seen().len(),
        1,
        "the child loop must never start",
    );
}
