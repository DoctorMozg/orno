use super::helpers::request;
use crate::agent::Agent;
use crate::agent::loop_agent::{LoopAgent, LoopAgentConfig};
use crate::error::AgentError;
use crate::error::ToolError;
use crate::events::{InMemorySink, Redactor};
use crate::llm::{LlmResponse, OrnoChatToolCall, Usage, dummy::ScriptedTransport};
use crate::pipeline::OnParseError;
use crate::tool::{ToolEffect, ToolHandler, ToolInvocation};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn retry_once_feeds_invalid_args_back_as_tool_result() {
    struct FlakyParse {
        attempts: Mutex<u32>,
    }
    #[async_trait]
    impl ToolHandler for FlakyParse {
        fn name(&self) -> &str {
            "FlakyParse"
        }
        fn description(&self) -> &str {
            "Fails with InvalidArgs once."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: serde_json::Value,
        ) -> Result<String, ToolError> {
            let mut n = self.attempts.lock().expect("mutex");
            *n += 1;
            if *n == 1 {
                Err(ToolError::InvalidArgs {
                    name: "FlakyParse".into(),
                    message: "missing field `x`".into(),
                })
            } else {
                Ok("second call ok".into())
            }
        }
    }

    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(FlakyParse {
        attempts: Mutex::new(0),
    });

    let transport = ScriptedTransport::new(vec![
        ScriptedTransport::tool_call_response("c1", "FlakyParse", serde_json::json!({})),
        ScriptedTransport::tool_call_response("c2", "FlakyParse", serde_json::json!({})),
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
    req.policy.on_parse_error = OnParseError::RetryOnce;
    req.allowed_tools = vec!["FlakyParse".into()];

    let out = agent
        .run("run_test", "n", req)
        .await
        .expect("RetryOnce must feed back, loop continues");
    assert!(out.content.contains("done"));
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the inline `AlwaysInvalid` ToolHandler stub plus the dual-call response literal push the body past 60 lines; both stay inline so the test's intent is readable end-to-end"
)]
async fn retry_once_second_parse_error_on_same_call_id_terminates_with_parse_failed() {
    struct AlwaysInvalid;
    #[async_trait]
    impl ToolHandler for AlwaysInvalid {
        fn name(&self) -> &str {
            "AlwaysInvalid"
        }
        fn description(&self) -> &str {
            "Always returns InvalidArgs."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: serde_json::Value,
        ) -> Result<String, ToolError> {
            Err(ToolError::InvalidArgs {
                name: "AlwaysInvalid".into(),
                message: "bad schema".into(),
            })
        }
    }

    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(AlwaysInvalid);

    // Both tool_calls share the same call_id and ride in ONE assistant turn so
    // the parse-retry de-dup runs within a single iteration's tool dispatch
    // loop. Per-iteration scope (post-M2) means a fresh `retried_parse_errors`
    // set at the start of every iteration; the second identical-call_id call
    // within the same iteration still terminates.
    let same_call = OrnoChatToolCall {
        call_id: "same".into(),
        fn_name: "AlwaysInvalid".into(),
        fn_arguments: serde_json::json!({}),
    };
    let dual_call = LlmResponse {
        content: String::new(),
        finish_reason: Some("tool_calls".to_string()),
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        }),
        tool_calls: vec![same_call.clone(), same_call],
    };
    let transport = ScriptedTransport::new(vec![dual_call]);

    let agent = LoopAgent::new(LoopAgentConfig {
        transport: Arc::new(transport),
        sink,
        redactor: Arc::new(Redactor::default()),
        body_excerpt_max_bytes: 256,
        tools: vec![tool],
    });

    let mut req = request();
    req.policy.max_iterations = 5;
    req.policy.on_parse_error = OnParseError::RetryOnce;
    req.allowed_tools = vec!["AlwaysInvalid".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("second InvalidArgs on same call_id must terminate");
    match err {
        AgentError::ParseFailed { tool, error } => {
            assert_eq!(tool, "AlwaysInvalid");
            assert!(error.contains("bad schema"));
        },
        other => panic!("expected ParseFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn on_parse_error_fail_terminates_on_first_invalid_args() {
    struct InvalidOnce;
    #[async_trait]
    impl ToolHandler for InvalidOnce {
        fn name(&self) -> &str {
            "InvalidOnce"
        }
        fn description(&self) -> &str {
            "InvalidArgs."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn effect(&self) -> ToolEffect {
            ToolEffect::ReadOnly
        }
        async fn invoke(
            &self,
            _inv: ToolInvocation<'_>,
            _args: serde_json::Value,
        ) -> Result<String, ToolError> {
            Err(ToolError::InvalidArgs {
                name: "InvalidOnce".into(),
                message: "missing x".into(),
            })
        }
    }

    let sink = Arc::new(InMemorySink::new());
    let tool = Arc::new(InvalidOnce);

    let transport = ScriptedTransport::new(vec![ScriptedTransport::tool_call_response(
        "c1",
        "InvalidOnce",
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
    req.policy.on_parse_error = OnParseError::Fail;
    req.allowed_tools = vec!["InvalidOnce".into()];

    let err = agent
        .run("run_test", "n", req)
        .await
        .expect_err("OnParseError::Fail must terminate on first InvalidArgs");
    match err {
        AgentError::ParseFailed { tool, .. } => assert_eq!(tool, "InvalidOnce"),
        other => panic!("expected ParseFailed, got {other:?}"),
    }
}
