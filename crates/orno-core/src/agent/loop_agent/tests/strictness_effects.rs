use super::helpers::{EchoTool, request};
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::events::{Event, InMemorySink, Redactor};
use crate::llm::dummy::ScriptedTransport;
use crate::tool::ToolEffect;
use std::sync::Arc;

#[tokio::test]
async fn context_writes_denied_feeds_back_as_tool_result_and_loop_continues() {
    // `ContextSelf` tools are gated by `allow_context_writes`. Like
    // `Mutations` / `Network` denials, a refusal is non-terminal — the denial
    // string reaches the model as a `ToolResult` and the loop continues. A
    // SetState call with the flag off must not mutate the node-state buffer
    // and must not terminate the loop.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::ContextSelf, "should not run"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("noted — state writes disabled"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_context_writes = false;
    req.allowed_tools = vec!["EchoTool".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("context-writes denial must feed back, not terminate");

    assert!(
        out.content.contains("state writes disabled"),
        "final output should carry the model's acknowledgment: {:?}",
        out.content,
    );
    // The denial string must be routed as the tool's `ToolResult` so a
    // downstream pair of `ToolCallRecorded` + next `LlmRequestStarted` shows
    // the gate fired.
    let events = sink.snapshot();
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must still fire for a gated ContextSelf call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.contains("allow_context_writes=false"),
            "denial excerpt must name the gate: {output_excerpt:?}",
        );
    }
    // Because no SetState write landed, the buffer stays empty and
    // `AgentOutput.state` collapses to `None`.
    assert!(
        out.state.is_none(),
        "no SetState call landed, state must be None: {:?}",
        out.state,
    );
}

#[tokio::test]
async fn mutations_denied_feeds_back_as_tool_result_and_loop_continues() {
    // Bounded effects via the feed-back mechanism. A denied mutation is *not*
    // a terminal error — the denial string reaches the next LLM turn as a
    // `ToolResult` and the model adapts. The loop only terminates on iteration
    // limit, budget, unknown tool, or a final text turn.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Mutations, "mutation done"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("understood, I cannot mutate"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_mutations = false;
    req.allowed_tools = vec!["EchoTool".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("policy denial must not terminate the loop — it feeds back to the model");

    assert!(
        out.content.contains("cannot mutate"),
        "final output should carry the model's acknowledgment: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let denied_event = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolDenied { .. }))
        .expect("ToolDenied must fire for a mutations denial");
    if let Event::ToolDenied {
        tool_name, reason, ..
    } = &denied_event.event
    {
        assert_eq!(tool_name, "EchoTool");
        assert!(
            reason.contains("allow_mutations=false"),
            "reason was: {reason}",
        );
    } else {
        panic!("unexpected event type");
    }
}

#[tokio::test]
async fn tool_denied_emits_yaml_name_for_dotted_subagent() {
    // M3 regression: when a dotted YAML name (`subagent.child`) is sanitized
    // to wire form (`subagent_child`) at the schema boundary, a policy denial
    // must still surface the YAML form operators wrote in their pipeline.
    // Otherwise an event reader grepping for the tool name in their YAML
    // cannot find the matching `ToolDenied` and the feed-back string back to
    // the model uses a name that does not appear anywhere in the
    // operator-authored config.
    use super::helpers::DottedMutationTool;

    let sink = Arc::new(InMemorySink::new());
    let dotted = Arc::new(DottedMutationTool);

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "subagent_child", serde_json::json!({})),
        ScriptedTransport::text_response("acknowledging mutation denial"),
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
    req.policy.max_subagent_depth = 1;
    req.policy.allow_mutations = false;
    req.allowed_tools = vec!["subagent.child".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("mutations denial must feed back, not terminate");

    let events = sink.snapshot();
    let denied = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolDenied { .. }))
        .expect("ToolDenied must fire for a dotted-name mutations denial");
    if let Event::ToolDenied {
        tool_name, reason, ..
    } = &denied.event
    {
        assert_eq!(
            tool_name, "subagent.child",
            "ToolDenied.tool_name must be the YAML form, not the wire form `subagent_child`",
        );
        assert!(
            reason.contains("allow_mutations=false"),
            "denial reason must name the gate: {reason:?}",
        );
    } else {
        panic!("unexpected event type");
    }

    // The feed-back string back to the model must also use the YAML form so
    // the model sees consistent naming with the pipeline YAML the operator wrote.
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must fire for a denied dotted-name call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.starts_with("denied: tool `subagent.child`"),
            "feed-back string must use the YAML form: {output_excerpt:?}",
        );
    }
}

#[tokio::test]
async fn network_denied_feeds_back_as_tool_result_and_loop_continues() {
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "net done"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("understood, I cannot reach network"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = false;
    req.allowed_tools = vec!["EchoTool".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("network denial must feed back, not terminate");

    assert!(
        out.content.contains("cannot reach network"),
        "final output should carry the model's acknowledgment: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must fire for a denied network call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.contains("allow_network=false"),
            "denial excerpt must name the gate: {output_excerpt:?}",
        );
    }
    let tool_denied = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolDenied { .. }))
        .expect("ToolDenied must fire for a network denial");
    if let Event::ToolDenied {
        tool_name, reason, ..
    } = &tool_denied.event
    {
        assert_eq!(tool_name, "EchoTool");
        assert!(
            reason.contains("allow_network=false"),
            "ToolDenied.reason must name the gate: {reason:?}",
        );
    }
}

#[tokio::test]
async fn domain_blocked_feeds_back_and_emits_tool_denied_event() {
    use crate::tool::WebFetchHandler;

    let sink = Arc::new(InMemorySink::new());
    let handler: Arc<dyn crate::tool::ToolHandler> = Arc::new(WebFetchHandler::default());

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "WebFetch",
            serde_json::json!({ "url": "https://evil.com/data" }),
        ),
        ScriptedTransport::text_response("acknowledging domain denial"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![handler],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.blocked_domains = vec!["evil.com".into()];
    req.allowed_tools = vec!["WebFetch".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("domain denial must feed back, not terminate");
    assert!(
        out.content.contains("acknowledging domain denial"),
        "parent should continue after denial: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let tool_denied = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolDenied { .. }))
        .expect("ToolDenied must fire for a blocked domain");
    if let Event::ToolDenied { reason, .. } = &tool_denied.event {
        assert!(
            reason.contains("evil.com"),
            "reason should name the domain: {reason:?}",
        );
        assert!(
            reason.contains("blocked_domains"),
            "reason should name the gate: {reason:?}",
        );
    }

    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must still fire for a domain-blocked call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.starts_with("denied: tool `WebFetch`"),
            "denial string must route through the tool result: {output_excerpt:?}",
        );
    }
}

#[tokio::test]
async fn allowed_domains_allowlist_denies_host_not_in_list() {
    use crate::tool::WebFetchHandler;

    let sink = Arc::new(InMemorySink::new());
    let handler: Arc<dyn crate::tool::ToolHandler> = Arc::new(WebFetchHandler::default());

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "WebFetch",
            serde_json::json!({ "url": "https://random.example/data" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![handler],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.allowed_domains = vec!["api.trusted.com".into()];
    req.allowed_tools = vec!["WebFetch".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("allowlist denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire when host is not on allowlist");
    assert!(
        reason.contains("allowed_domains"),
        "reason should name the gate: {reason:?}",
    );
    assert!(
        reason.contains("random.example"),
        "reason should name the rejected host: {reason:?}",
    );
}

#[tokio::test]
async fn blocked_domains_matches_subdomain_by_suffix() {
    // Suffix matching invariant: `blocked_domains: ["evil.com"]` must deny
    // `sub.evil.com`, closing the footgun where naive equality let subdomains
    // through.
    use crate::tool::WebFetchHandler;

    let sink = Arc::new(InMemorySink::new());
    let handler: Arc<dyn crate::tool::ToolHandler> = Arc::new(WebFetchHandler::default());

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "WebFetch",
            serde_json::json!({ "url": "https://sub.evil.com/data" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![handler],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.blocked_domains = vec!["evil.com".into()];
    req.allowed_tools = vec!["WebFetch".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("subdomain denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire for a subdomain of a blocked domain");
    assert!(
        reason.contains("sub.evil.com"),
        "reason should name the rejected host: {reason:?}",
    );
    assert!(
        reason.contains("blocked_domains"),
        "reason should name the gate: {reason:?}",
    );
}

#[tokio::test]
async fn blocked_domains_does_not_match_unrelated_host_with_shared_suffix() {
    // Suffix matching must not misfire on `notevil.com` when `blocked_domains:
    // ["evil.com"]`: the match requires either exact equality or a
    // `.`-separated suffix, not a raw string suffix. Without this guard an
    // attacker could register a sibling name that passes the filter.
    //
    // Uses `EchoTool` with `ToolEffect::Network` so the policy gate runs
    // (examining the `url` arg) without the handler actually hitting the
    // network.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "ok"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "EchoTool",
            serde_json::json!({ "url": "https://notevil.com/data" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.blocked_domains = vec!["evil.com".into()];
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("unrelated host must not terminate");

    let events = sink.snapshot();
    let denied = events.iter().find_map(|e| match &e.event {
        Event::ToolDenied { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert!(
        denied.is_none(),
        "ToolDenied must NOT fire for notevil.com when only evil.com is blocked: {denied:?}",
    );
}

#[tokio::test]
async fn blocked_domains_still_matches_exact_host() {
    // Suffix matching must preserve the exact-equality case: the existing
    // `evil.com` deny-all behavior must not regress.
    use crate::tool::WebFetchHandler;

    let sink = Arc::new(InMemorySink::new());
    let handler: Arc<dyn crate::tool::ToolHandler> = Arc::new(WebFetchHandler::default());

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "WebFetch",
            serde_json::json!({ "url": "https://evil.com/data" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![handler],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.blocked_domains = vec!["evil.com".into()];
    req.allowed_tools = vec!["WebFetch".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("exact-host denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must still fire on the exact blocked domain");
    assert!(
        reason.contains("evil.com"),
        "reason should name the rejected host: {reason:?}",
    );
    assert!(
        reason.contains("blocked_domains"),
        "reason should name the gate: {reason:?}",
    );
}

#[tokio::test]
async fn allowed_domains_matches_subdomain_by_suffix() {
    // Mirror of the blocked-domain test: `allowed_domains:
    // ["api.trusted.com"]` must ALLOW `sub.api.trusted.com` under suffix
    // matching. An exact-match allowlist would force operators to enumerate
    // every subdomain.
    //
    // Uses `EchoTool` with `ToolEffect::Network` so the policy gate runs
    // (examining the `url` arg) without the handler actually hitting the
    // network — the sibling `blocked_domains_*` tests use `WebFetchHandler`
    // because they rely on the denial path firing before any network call, but
    // the allowed path here would otherwise attempt a real DNS lookup.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "ok"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "EchoTool",
            serde_json::json!({ "url": "https://sub.api.trusted.com/data" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.policy.allowed_domains = vec!["api.trusted.com".into()];
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("subdomain of allowlisted host must not terminate");

    let events = sink.snapshot();
    let denied = events.iter().find_map(|e| match &e.event {
        Event::ToolDenied { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert!(
        denied.is_none(),
        "ToolDenied must NOT fire for sub.api.trusted.com on an api.trusted.com allowlist: {denied:?}",
    );
}

#[tokio::test]
async fn non_http_scheme_denied() {
    // file://, ftp://, data:// are disallowed regardless of domain lists.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "EchoTool",
            serde_json::json!({ "url": "file:///etc/passwd" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("scheme denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire for non-HTTP scheme");
    assert!(
        reason.contains("file"),
        "reason should name the disallowed scheme: {reason:?}",
    );
}

#[tokio::test]
async fn loopback_ip_denied() {
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "EchoTool",
            serde_json::json!({ "url": "http://127.0.0.1/secret" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("IP denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire for loopback IP");
    assert!(
        reason.contains("127.0.0.1"),
        "reason should name the IP: {reason:?}",
    );
}

#[tokio::test]
async fn private_ip_denied() {
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::Network, "should not reach here"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "EchoTool",
            serde_json::json!({ "url": "http://192.168.1.100/internal" }),
        ),
        ScriptedTransport::text_response("ok"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_network = true;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("private IP denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire for private IP");
    assert!(
        reason.contains("192.168.1.100"),
        "reason should name the IP: {reason:?}",
    );
}

#[tokio::test]
async fn mutations_and_network_tool_denied_when_mutations_false() {
    // `MutationsAndNetwork` is the effect class for Bash and any tool that
    // both mutates state and opens network sockets. The gate must deny when
    // EITHER underlying flag is off — here `allow_mutations=false` blocks
    // even when network is allowed. Denial is non-terminal: the loop feeds
    // the denial string back as a `ToolResult` and the model continues.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(
        ToolEffect::MutationsAndNetwork,
        "should not run",
    ));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("acknowledged mutation denial"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_mutations = false;
    req.policy.allow_network = true;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("MutationsAndNetwork denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire when mutations are disallowed");
    assert!(
        reason.contains("allow_mutations=false"),
        "reason must name the mutations gate: {reason:?}",
    );
}

#[tokio::test]
async fn mutations_and_network_tool_denied_when_network_false() {
    // Mirror of the prior test on the second underlying flag. With
    // `allow_mutations=true` and `allow_network=false`, the same combined
    // effect must still be denied — neither half can be satisfied alone.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(
        ToolEffect::MutationsAndNetwork,
        "should not run",
    ));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("acknowledged network denial"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_mutations = true;
    req.policy.allow_network = false;
    req.allowed_tools = vec!["EchoTool".into()];

    agent
        .run("run_test", "n", req)
        .await
        .expect("MutationsAndNetwork denial must feed back, not terminate");

    let events = sink.snapshot();
    let reason = events
        .iter()
        .find_map(|e| match &e.event {
            Event::ToolDenied { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .expect("ToolDenied must fire when network is disallowed");
    assert!(
        reason.contains("allow_network=false"),
        "reason must name the network gate: {reason:?}",
    );
}

#[tokio::test]
async fn bash_with_allowed_domains_is_denied_hard() {
    // F20 contract: `Bash` opens arbitrary network sockets that orno
    // cannot intercept, so an `allowed_domains` allowlist cannot be
    // honored on a Bash invocation. Silently letting the call through
    // would give a false sense of egress confinement; instead the gate
    // refuses the call up front. Denial is non-terminal — the message
    // feeds back as a `ToolResult` and the loop continues — but the
    // refusal must explicitly name `allowed_domains` so the operator
    // can correct the misconfiguration.
    use crate::tool::BashHandler;

    let sink = Arc::new(InMemorySink::new());
    let handler: Arc<dyn crate::tool::ToolHandler> = Arc::new(BashHandler);

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response(
            "c1",
            "Bash",
            serde_json::json!({ "command": "echo hello" }),
        ),
        ScriptedTransport::text_response("acknowledging Bash denial"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![handler],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    // Both Mutations and Network must be ON so the F20 gate is the only
    // refusal in play — otherwise the prior `allow_mutations` /
    // `allow_network` gates would mask the allowed_domains denial.
    req.policy.allow_mutations = true;
    req.policy.allow_network = true;
    req.policy.allowed_domains = vec!["example.com".into()];
    req.allowed_tools = vec!["Bash".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("Bash + allowed_domains denial must feed back, not terminate");

    assert!(
        out.content.contains("acknowledging Bash denial"),
        "loop must continue past the denial: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must fire for a denied Bash call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.contains("cannot enforce allowed_domains"),
            "feed-back string must name the F20 refusal: {output_excerpt:?}",
        );
    }

    let denied = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolDenied { .. }))
        .expect("ToolDenied must fire for the F20 refusal");
    if let Event::ToolDenied {
        tool_name, reason, ..
    } = &denied.event
    {
        assert_eq!(tool_name, "Bash");
        assert!(
            reason.contains("cannot enforce allowed_domains"),
            "ToolDenied.reason must explain the F20 refusal: {reason:?}",
        );
    }
}

#[tokio::test]
async fn mutations_and_network_tool_allowed_when_both_true() {
    // Positive control: when both underlying flags are on, the combined
    // effect must pass through to the handler and produce a real tool
    // result. `MutationsAndNetwork` is not subject to the URL/domain gate
    // (it covers shells that open arbitrary connections orno cannot
    // intercept), so no `ToolDenied` event must fire.
    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(EchoTool::new(ToolEffect::MutationsAndNetwork, "ran ok"));

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "EchoTool", serde_json::json!({})),
        ScriptedTransport::text_response("done"),
    ]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink: sink.clone(),
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 3;
    req.policy.allow_mutations = true;
    req.policy.allow_network = true;
    req.allowed_tools = vec!["EchoTool".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("both flags on must allow the tool to execute");

    assert!(
        out.content.contains("done"),
        "loop must terminate on the final text response: {:?}",
        out.content,
    );

    let events = sink.snapshot();
    let denied = events.iter().find_map(|e| match &e.event {
        Event::ToolDenied { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert!(
        denied.is_none(),
        "ToolDenied must NOT fire when both gates are open: {denied:?}",
    );
    let recorded = events
        .iter()
        .find(|e| matches!(e.event, Event::ToolCallRecorded { .. }))
        .expect("ToolCallRecorded must fire for an allowed call");
    if let Event::ToolCallRecorded { output_excerpt, .. } = &recorded.event {
        assert!(
            output_excerpt.contains("ran ok"),
            "recorded output must carry the handler's payload: {output_excerpt:?}",
        );
    }
}
