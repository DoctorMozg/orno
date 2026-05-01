use super::helpers::{EchoTool, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::LlmError;
use crate::events::{InMemorySink, Redactor};
use crate::llm::{LlmRequest, LlmResponse, LlmTransport, OrnoChatMessage, OrnoChatToolCall, Usage};
use crate::tool::ToolEffect;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Transport stub that retains an `Arc::clone` of every `LlmRequest.messages`
/// it receives. Forces the agent loop's `Arc::make_mut` calls onto the
/// copy-on-write path: refcount > 1 means the loop must deep-clone the
/// history into a fresh allocation, leaving the retained clone untouched.
struct RetainingTransport {
    /// One entry per `complete()` call, in observation order.
    captured: Mutex<Vec<Arc<Vec<OrnoChatMessage>>>>,
    /// Scripted responses, popped front-to-back.
    responses: Mutex<std::collections::VecDeque<LlmResponse>>,
}

impl RetainingTransport {
    fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            captured: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }

    fn captured(&self) -> Vec<Arc<Vec<OrnoChatMessage>>> {
        self.captured
            .lock()
            .expect("RetainingTransport mutex poisoned")
            .clone()
    }
}

#[async_trait]
impl LlmTransport for RetainingTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        // The load-bearing line: retain a strong clone of the messages
        // Arc so the next iteration's `Arc::make_mut` cannot mutate in
        // place. If the loop's invariant is broken (e.g., a future
        // refactor accidentally shares the same Arc with no CoW guard),
        // this clone would alias the loop's storage and mutations
        // would leak across iterations.
        self.captured
            .lock()
            .expect("RetainingTransport mutex poisoned")
            .push(Arc::clone(&req.messages));
        self.responses
            .lock()
            .expect("RetainingTransport mutex poisoned")
            .pop_front()
            .ok_or_else(|| LlmError::Rejected("no more scripted responses".to_string()))
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "single-iteration loop with multi-step retain+assert CoW verification"
)]
async fn transport_that_retains_arc_does_not_cause_deep_clone() {
    // F-H4 invariant test: a transport that retains an Arc::clone of
    // `LlmRequest.messages` must not corrupt the agent loop's
    // history. The loop calls `Arc::make_mut` on every push; with
    // refcount > 1 (because the transport held a clone), make_mut
    // deep-clones into a fresh allocation. The retained clone stays
    // pinned to the original (empty) vector while the loop continues
    // to push into its own copy.
    //
    // Asserting that the retained Arc from the first call and the Arc
    // received on the second call point to DIFFERENT allocations
    // proves CoW happened. If they pointed to the same allocation,
    // `Arc::make_mut` would have mutated in place and the retained
    // clone would have observed the new ToolCalls/ToolResult messages
    // — a determinism breach across iterations.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::ReadOnly, "ok"));

    // First response is a tool call (forces a `messages` push), second
    // is a text answer that terminates the loop.
    let first = LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
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
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
        }),
        tool_calls: Vec::new(),
    };
    let transport = Arc::new(RetainingTransport::new(vec![first, second]));

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: transport.clone(),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("loop must complete");

    let captured = transport.captured();
    assert_eq!(
        captured.len(),
        2,
        "transport must observe two requests across the iteration",
    );
    // The retained clone of the first request and the Arc received on
    // the second request must point to DIFFERENT allocations. If they
    // shared an allocation, `Arc::make_mut` skipped its CoW path and
    // mutated the history the transport already held — exactly the
    // determinism breach the invariant guards against.
    assert!(
        !Arc::ptr_eq(&captured[0], &captured[1]),
        "Arc::make_mut must deep-clone when the transport retains a clone; \
         got identical allocation across iterations",
    );
    // The retained first-call clone must still observe the original
    // (empty) message history. If the loop mutated in place, the
    // pushed ToolCalls turn would be visible here.
    assert!(
        captured[0].is_empty(),
        "first-call retained clone must remain empty; loop must have pushed into its own copy",
    );
    // The second-call clone carries the assistant's tool-call turn
    // followed by the tool result the loop pushed after dispatch.
    assert_eq!(
        captured[1].len(),
        2,
        "second-call clone must carry the new ToolCalls and ToolResult messages",
    );
}
