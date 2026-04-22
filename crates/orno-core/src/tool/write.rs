//! `Write` tool — write a file (ADR 0008). Requires `allow_mutations`.

use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{debug, instrument};

use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

#[derive(Debug, Default, Clone)]
pub struct WriteHandler;

#[async_trait]
impl ToolHandler for WriteHandler {
    fn name(&self) -> &str {
        "Write"
    }
    fn description(&self) -> &str {
        "Write content to a file at the given path, overwriting if it exists."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path to write." },
                "content": { "type": "string", "description": "Content to write." }
            },
            "required": ["path", "content"]
        })
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutations
    }

    #[instrument(skip(self, args), fields(tool.name = "Write", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "Write".to_string(),
                message: "missing or non-string `path`".to_string(),
            })?
            .to_string();

        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "Write".to_string(),
                message: "missing or non-string `content`".to_string(),
            })?
            .to_string();

        // Create parent dirs so writes to nested paths (e.g. `tmp/out/file.txt`)
        // succeed without the caller pre-creating the hierarchy.
        if let Some(parent) = std::path::Path::new(&path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::Invocation {
                name: "Write".to_string(),
                source: Box::new(e),
            })?;
        }

        std::fs::write(&path, &content).map_err(|e| ToolError::Invocation {
            name: "Write".to_string(),
            source: Box::new(e),
        })?;

        let bytes = content.len();
        debug!(file.path = %path, file.bytes = bytes, call_id = %inv.call_id, "wrote file");
        Ok(format!("Wrote {bytes} bytes to {path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[tokio::test]
    async fn writes_new_file() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("out.txt");
        let handler = WriteHandler;

        let result = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "path": path.to_str().unwrap(), "content": "hello" }),
            )
            .await
            .expect("write succeeds");

        assert!(result.contains("5 bytes"));
        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("out.txt");
        std::fs::write(&path, "old").expect("seed file");

        let handler = WriteHandler;
        handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "path": path.to_str().unwrap(), "content": "new content" }),
            )
            .await
            .expect("overwrite succeeds");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "new content");
    }

    #[tokio::test]
    async fn missing_path_returns_invalid_args() {
        let handler = WriteHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "content": "x" }),
            )
            .await
            .expect_err("missing path must fail");

        match err {
            ToolError::InvalidArgs { name, .. } => assert_eq!(name, "Write"),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("subdir").join("nested").join("file.txt");
        let handler = WriteHandler;

        handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "path": path.to_str().unwrap(), "content": "deep" }),
            )
            .await
            .expect("write with nested parents succeeds");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "deep");
    }
}
