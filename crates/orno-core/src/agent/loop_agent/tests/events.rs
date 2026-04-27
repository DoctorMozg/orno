use super::helpers::{FailingTransport, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::AgentError;
use crate::error::LlmError;
use crate::events::{Event, InMemorySink, LlmFailure, Redactor};
use crate::llm::DummyTransport;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn emits_request_and_response_events_in_order() {
    let sink = Arc::new(InMemorySink::new());
    let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());

    let out = agent
        .run("run_test", "n", request())
        .await
        .expect("dummy transport always succeeds");

    assert!(out.content.contains("[dummy]"));

    let events = sink.snapshot();
    let starts = events
        .iter()
        .enumerate()
        .find_map(|(i, e)| matches!(e.event, Event::LlmRequestStarted { .. }).then_some(i))
        .expect("LlmRequestStarted emitted");
    let recvs = events
        .iter()
        .enumerate()
        .find_map(|(i, e)| matches!(e.event, Event::LlmResponseReceived { .. }).then_some(i))
        .expect("LlmResponseReceived emitted");
    assert!(
        starts < recvs,
        "LlmRequestStarted must precede LlmResponseReceived",
    );

    // The excerpt fields must round-trip the rendered prompt and the model
    // response, not be silently empty. A consumer pairing the two envelopes
    // must see what went in and what came back without folding
    // `NodeResponse.output`.
    if let Event::LlmRequestStarted {
        provider,
        model,
        prompt_excerpt,
        system_excerpt,
        ..
    } = &events[starts].event
    {
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-5");
        assert!(
            prompt_excerpt.contains("say hi"),
            "prompt_excerpt must carry the rendered prompt: {prompt_excerpt:?}",
        );
        assert!(
            system_excerpt.is_none(),
            "baseline request has no system prompt — excerpt must be None, got {system_excerpt:?}",
        );
    } else {
        panic!("LlmRequestStarted event not destructurable");
    }

    if let Event::LlmResponseReceived {
        content_excerpt, ..
    } = &events[recvs].event
    {
        assert!(
            content_excerpt.contains("[dummy]"),
            "content_excerpt must carry the transport response: {content_excerpt:?}",
        );
    } else {
        panic!("LlmResponseReceived event not destructurable");
    }
}

#[tokio::test]
async fn transport_error_emits_llm_request_failed_before_propagating() {
    // The Phase 3 invariant: a transport failure leaves a typed
    // `LlmRequestFailed` on the wire next to the dangling `LlmRequestStarted`.
    // Without this event, a downstream consumer can only see the opaque error
    // chain and cannot tell `auth_failed` from a stray parse error.
    let sink = Arc::new(InMemorySink::new());
    let agent = LoopAgent::with_defaults(Arc::new(FailingTransport::auth()), sink.clone());

    let err = agent
        .run("run_test", "n", request())
        .await
        .expect_err("transport failure must propagate as AgentError");
    match err {
        AgentError::Llm(LlmError::AuthFailed { provider }) => assert_eq!(provider, "openai"),
        other => panic!("expected AgentError::Llm(AuthFailed), got {other:?}"),
    }

    let events = sink.snapshot();
    let mut started_idx = None;
    let mut failed_idx = None;
    for (i, env) in events.iter().enumerate() {
        match &env.event {
            Event::LlmRequestStarted { .. } => started_idx = Some(i),
            Event::LlmRequestFailed { failure, .. } => {
                assert!(
                    matches!(failure, LlmFailure::AuthFailed),
                    "expected AuthFailed classification, got {failure:?}",
                );
                failed_idx = Some(i);
            },
            Event::LlmResponseReceived { .. } => {
                panic!("LlmResponseReceived must not fire on a transport failure");
            },
            _ => {},
        }
    }
    let started = started_idx.expect("LlmRequestStarted must still fire");
    let failed = failed_idx.expect("LlmRequestFailed must fire on transport error");
    assert!(
        started < failed,
        "LlmRequestFailed must follow LlmRequestStarted in stream order",
    );
}

#[tokio::test]
async fn system_excerpt_present_when_agent_config_declared_a_system_prompt() {
    // The sibling of the baseline test: when the agent config carries a
    // `system:` block, its redacted excerpt must reach the wire so a consumer
    // pairs request intent with the behavioral contract the operator set.
    let sink = Arc::new(InMemorySink::new());
    let agent = LoopAgent::with_defaults(Arc::new(DummyTransport), sink.clone());
    let mut req = request();
    req.system = Some(Arc::from("You are a terse assistant."));

    agent
        .run("run_test", "n", req)
        .await
        .expect("dummy transport always succeeds");

    let events = sink.snapshot();
    let got = events.iter().find_map(|e| match &e.event {
        Event::LlmRequestStarted { system_excerpt, .. } => Some(system_excerpt.clone()),
        _ => None,
    });
    assert_eq!(
        got,
        Some(Some("You are a terse assistant.".to_string())),
        "system_excerpt must round-trip the configured system prompt",
    );
}

#[tokio::test]
async fn prompt_excerpt_redacts_known_secret_values() {
    // The agent shares the engine's `Redactor` so a prompt that embedded a
    // rendered `secrets.*` value never reaches the sink in cleartext. Without
    // this, enabling prompt excerpts would regress the secrets-namespace
    // contract.
    let mut secret_map = BTreeMap::new();
    secret_map.insert(
        "OPENROUTER_API_KEY".to_string(),
        "sk-very-secret-12345".to_string(),
    );
    let redactor = Arc::new(Redactor::new(&secret_map));

    let sink = Arc::new(InMemorySink::new());
    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(DummyTransport),
        sink: sink.clone(),
        redactor,
        body_excerpt_max_bytes: 2048,
        tools: Vec::new(),
    });
    let mut req = request();
    req.initial_prompt = Arc::from("Use key sk-very-secret-12345 to authorize this request.");

    agent
        .run("run_test", "n", req)
        .await
        .expect("dummy transport always succeeds");

    let events = sink.snapshot();
    let prompt = events
        .iter()
        .find_map(|e| match &e.event {
            Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
            _ => None,
        })
        .expect("LlmRequestStarted must be present");
    assert!(
        !prompt.contains("sk-very-secret-12345"),
        "raw secret leaked into prompt_excerpt: {prompt:?}",
    );
    assert!(
        prompt.contains("***"),
        "redactor must substitute `***` for the secret value: {prompt:?}",
    );
}

#[tokio::test]
async fn prompt_excerpt_truncates_at_configured_cap() {
    // A multi-KB rendered prompt must not flood the event stream. Same
    // truncation policy as LlmFailure::ApiError.body_excerpt — head bytes win
    // (the operator instruction sits at the front), ellipsis marker appended
    // when truncation happened.
    let sink = Arc::new(InMemorySink::new());
    // Explicit 32-byte cap makes truncation observable without needing a
    // megabyte prompt.
    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(DummyTransport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 32,
        tools: Vec::new(),
    });
    let mut req = request();
    req.initial_prompt = Arc::from("A".repeat(1000).as_str());

    agent
        .run("run_test", "n", req)
        .await
        .expect("dummy transport always succeeds");

    let events = sink.snapshot();
    let prompt = events
        .iter()
        .find_map(|e| match &e.event {
            Event::LlmRequestStarted { prompt_excerpt, .. } => Some(prompt_excerpt.clone()),
            _ => None,
        })
        .expect("LlmRequestStarted must be present");
    assert!(
        prompt.ends_with('…'),
        "truncation marker missing from long prompt: {prompt:?}",
    );
    assert!(
        prompt.len() <= 32 + '…'.len_utf8(),
        "excerpt exceeds cap+marker ({} bytes): {prompt:?}",
        prompt.len(),
    );
    assert!(
        prompt.starts_with("AAAA"),
        "excerpt must keep the head of the prompt: {prompt:?}",
    );
}
