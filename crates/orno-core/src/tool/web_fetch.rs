//! `WebFetch` tool — HTTP GET a URL (ADR 0008). Requires `allow_network`.
//! Domain policy is enforced by `LoopAgent` before dispatch.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

// Response bodies above this cap are truncated before being returned to
// the agent. Keeps a runaway page from blowing out the LLM context
// window on the next turn.
const MAX_BODY_BYTES: usize = 1_048_576;

#[derive(Debug, Default, Clone)]
pub struct WebFetchHandler;

#[async_trait]
impl ToolHandler for WebFetchHandler {
    fn name(&self) -> &str {
        "WebFetch"
    }
    fn description(&self) -> &str {
        "HTTP GET a URL and return the response body and content-type. Up to 1 MiB."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch." }
            },
            "required": ["url"]
        })
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Network
    }
    async fn invoke(&self, _inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let url =
            args.get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::InvalidArgs {
                    name: "WebFetch".to_string(),
                    message: "missing required field `url`".to_string(),
                })?;

        let client = reqwest::Client::new();
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|err| ToolError::Invocation {
                name: "WebFetch".to_string(),
                source: Box::new(err),
            })?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let bytes = response
            .bytes()
            .await
            .map_err(|err| ToolError::Invocation {
                name: "WebFetch".to_string(),
                source: Box::new(err),
            })?;

        let truncated = bytes.len() > MAX_BODY_BYTES;
        let slice = bytes
            .get(..MAX_BODY_BYTES.min(bytes.len()))
            .unwrap_or(&bytes);
        let body = String::from_utf8_lossy(slice).into_owned();

        let mut out = format!("status: {status}\ncontent-type: {content_type}\n\n{body}");
        if truncated {
            out.push_str("\n[truncated at 1 MiB]");
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_url_arg_returns_invalid_args() {
        let handler = WebFetchHandler;
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "WebFetch");
                assert!(message.contains("url"), "unexpected message: {message}");
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_string_url_returns_invalid_args() {
        let handler = WebFetchHandler;
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({ "url": 42 }))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "WebFetch");
                assert!(message.contains("url"), "unexpected message: {message}");
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_url_returns_invocation_error() {
        let handler = WebFetchHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "url": "not-a-url" }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Invocation { name, .. } => assert_eq!(name, "WebFetch"),
            other => panic!("expected Invocation, got {other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "requires network — run with: cargo test -- --ignored"]
    async fn fetches_real_url() {
        let handler = WebFetchHandler;
        let args = json!({ "url": "https://example.com" });
        let out = handler
            .invoke(ToolInvocation::for_test("call-1"), args)
            .await
            .unwrap();
        assert!(out.starts_with("status: 200"));
        assert!(out.contains("content-type:"));
    }
}
