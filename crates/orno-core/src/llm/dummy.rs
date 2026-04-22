//! Canned `LlmTransport` for the skeleton and for tests — returns a
//! deterministic response without touching the network.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::LlmError;

use super::{LlmRequest, LlmResponse, LlmTransport, OrnoChatToolCall, Usage};

#[derive(Debug, Default, Clone)]
pub struct DummyTransport;

#[async_trait]
impl LlmTransport for DummyTransport {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        Ok(LlmResponse {
            content: format!(
                "[dummy] model={} prompt={:?}",
                req.model,
                req.prompt.chars().take(40).collect::<String>()
            ),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
            tool_calls: Vec::new(),
        })
    }
}

/// A test transport that returns a scripted sequence of `LlmResponse` values.
/// Each `complete` call pops the next entry from the queue. Returns
/// `LlmError::Rejected("no more scripted responses")` when the queue is
/// exhausted — this makes "model never stops calling tools" tests fail loudly
/// rather than hang.
pub struct ScriptedTransport {
    responses: Mutex<VecDeque<LlmResponse>>,
}

impl ScriptedTransport {
    #[must_use]
    pub fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }

    /// Build a response that looks like a plain text stop.
    #[must_use]
    pub fn text_response(content: impl Into<String>) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            finish_reason: Some("stop".to_string()),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            tool_calls: Vec::new(),
        }
    }

    /// Build a response that carries one tool call.
    #[must_use]
    pub fn tool_call_response(
        call_id: impl Into<String>,
        fn_name: impl Into<String>,
        args: Value,
    ) -> LlmResponse {
        LlmResponse {
            content: String::new(),
            finish_reason: Some("tool_calls".to_string()),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            tool_calls: vec![OrnoChatToolCall {
                call_id: call_id.into(),
                fn_name: fn_name.into(),
                fn_arguments: args,
            }],
        }
    }
}

#[async_trait]
impl LlmTransport for ScriptedTransport {
    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
        self.responses
            .lock()
            .expect("ScriptedTransport mutex poisoned")
            .pop_front()
            .ok_or_else(|| LlmError::Rejected("no more scripted responses".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_request() -> LlmRequest {
        LlmRequest::from_prompt(
            "openai".into(),
            "gpt-5".into(),
            "hello".into(),
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn scripted_returns_responses_in_order() {
        let transport = ScriptedTransport::new(vec![
            ScriptedTransport::text_response("first"),
            ScriptedTransport::tool_call_response("call_1", "bash", json!({"cmd": "ls"})),
            ScriptedTransport::text_response("third"),
        ]);

        let first = transport.complete(sample_request()).await.unwrap();
        assert_eq!(first.content, "first");
        assert!(first.tool_calls.is_empty());

        let second = transport.complete(sample_request()).await.unwrap();
        assert!(second.content.is_empty());
        assert_eq!(second.tool_calls.len(), 1);
        assert_eq!(second.tool_calls[0].call_id, "call_1");
        assert_eq!(second.tool_calls[0].fn_name, "bash");
        assert_eq!(second.tool_calls[0].fn_arguments, json!({"cmd": "ls"}));

        let third = transport.complete(sample_request()).await.unwrap();
        assert_eq!(third.content, "third");
    }

    #[tokio::test]
    async fn scripted_returns_error_when_exhausted() {
        let transport = ScriptedTransport::new(vec![ScriptedTransport::text_response("only one")]);

        transport.complete(sample_request()).await.unwrap();

        let err = transport.complete(sample_request()).await.unwrap_err();
        match err {
            LlmError::Rejected(msg) => assert_eq!(msg, "no more scripted responses"),
            other => panic!("expected LlmError::Rejected, got {other:?}"),
        }
    }
}
