//! `Read` tool — read a file's contents (ADR 0008). Read-only effect.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[schemars(description = "File path to read.")]
    path: String,
}

#[derive(Debug, Default, Clone)]
pub struct ReadHandler;

#[async_trait]
impl ToolHandler for ReadHandler {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read the contents of a file at the given path. Returns the file's text content."
    }
    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn invoke(&self, _inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let ReadArgs { path } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "Read".to_string(),
                message: e.to_string(),
            })?;

        std::fs::read_to_string(&path).map_err(|err| ToolError::Invocation {
            name: "Read".to_string(),
            source: Box::new(err),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    #[tokio::test]
    async fn reads_existing_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let contents = "line one\nline two\n";
        tmp.write_all(contents.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let handler = ReadHandler;
        let args = json!({ "path": tmp.path().to_str().unwrap() });
        let out = handler
            .invoke(ToolInvocation::for_test("call-1"), args)
            .await
            .unwrap();
        assert_eq!(out, contents);
    }

    #[tokio::test]
    async fn missing_path_arg_returns_invalid_args() {
        let handler = ReadHandler;
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), json!({}))
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "Read");
                assert!(message.contains("path"), "unexpected message: {message}");
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonexistent_file_returns_invocation_error() {
        let handler = ReadHandler;
        let args = json!({ "path": "/nonexistent/path/that/should/not/exist/orno-read-test" });
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), args)
            .await
            .unwrap_err();
        match err {
            ToolError::Invocation { name, .. } => assert_eq!(name, "Read"),
            other => panic!("expected Invocation, got {other:?}"),
        }
    }
}
