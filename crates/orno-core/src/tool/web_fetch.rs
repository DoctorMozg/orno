//! `WebFetch` tool — HTTP GET a URL. Requires `allow_network`.
//! Domain policy is enforced by `LoopAgent` before dispatch.

use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

// Response bodies above this cap are truncated before being returned to
// the agent. Keeps a runaway page from blowing out the LLM context
// window on the next turn.
const MAX_BODY_BYTES: usize = 1_048_576;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebFetchArgs {
    #[schemars(description = "URL to fetch.")]
    url: String,
    #[schemars(description = "Per-request timeout in seconds. Defaults to 30.")]
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// List the top-level field names of a JSON-object argument bundle
/// without echoing their values, for inclusion in `InvalidArgs`
/// messages.
fn arg_field_names(args: &Value) -> String {
    args.as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

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
        serde_json::to_value(schemars::schema_for!(WebFetchArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Network
    }
    async fn invoke(&self, _inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        // Retain the args' field names (not values) so the error message
        // pins the offending field even when serde only reports a
        // type-level mismatch (e.g. `invalid type: integer …, expected a
        // string`). Values are omitted to avoid echoing caller-supplied
        // payloads into the log.
        let fields = arg_field_names(&args);
        let WebFetchArgs { url, timeout_secs } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "WebFetch".to_string(),
                message: format!("{e} (fields: {fields})"),
            })?;

        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|err| ToolError::Invocation {
                name: "WebFetch".to_string(),
                source: Box::new(err),
            })?;
        let response = client
            .get(&url)
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
    use serde_json::json;

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

    #[test]
    fn schema_contains_expected_fields() {
        let schema = WebFetchHandler.schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        assert!(
            properties.contains_key("url"),
            "properties missing url: {schema}"
        );
        assert!(
            properties.contains_key("timeout_secs"),
            "properties missing timeout_secs: {schema}"
        );

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema must expose a required array")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert!(
            required.contains(&"url"),
            "`url` must be required: {required:?}"
        );
        assert!(
            !required.contains(&"timeout_secs"),
            "timeout_secs must be optional: {required:?}"
        );
    }

    #[test]
    fn default_timeout_used_when_unset() {
        let args: WebFetchArgs = serde_json::from_value(json!({ "url": "https://example.com" }))
            .expect("WebFetchArgs must accept a body without timeout_secs");
        assert_eq!(args.url, "https://example.com");
        assert!(
            args.timeout_secs.is_none(),
            "timeout_secs must default to None when omitted"
        );
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
