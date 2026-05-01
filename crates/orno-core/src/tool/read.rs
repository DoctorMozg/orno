//! `Read` tool — read a file's contents. Read-only effect.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::path_guard::jail_path;
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

/// Hard cap on bytes returned by a single `Read`. Larger files are
/// truncated to this size with a marker so a runaway tool call cannot
/// drag the next LLM request through the provider's prompt-size
/// ceiling. Distinct from the policy-level
/// `AgentPolicy.max_tool_output_bytes` (which truncates on the way back
/// into conversation history); this cap fires at the source so the
/// handler never holds the full file in memory.
const MAX_READ_BYTES: u64 = 10 * 1024 * 1024;

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

    fn requires_jail(&self) -> bool {
        true
    }

    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let ReadArgs { path } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "Read".to_string(),
                message: e.to_string(),
            })?;

        let resolved: PathBuf = if let Some(root) = inv.roots.first() {
            jail_path(root, &path)?
        } else {
            // BS1: empty roots means no jail boundary — refuse rather than
            // allowing unrestricted host filesystem access.
            return Err(ToolError::Denied {
                reason: "Read tool requires a non-empty `roots` list; \
                         no jail boundary is configured for this agent"
                    .to_string(),
            });
        };

        let metadata = std::fs::metadata(&resolved).map_err(|e| ToolError::Invocation {
            name: "Read".to_string(),
            source: Box::new(e),
        })?;

        if metadata.len() > MAX_READ_BYTES {
            // Stream-read only the cap so a giant file does not pin
            // 10 MiB plus the rest in RSS while we copy.
            let cap = usize::try_from(MAX_READ_BYTES).expect("MAX_READ_BYTES=10MiB fits in usize");
            let mut buf = Vec::with_capacity(cap);
            std::fs::File::open(&resolved)
                .and_then(|f| f.take(MAX_READ_BYTES).read_to_end(&mut buf).map(|_| ()))
                .map_err(|e| ToolError::Invocation {
                    name: "Read".to_string(),
                    source: Box::new(e),
                })?;
            let mut content = String::from_utf8_lossy(&buf).into_owned();
            let _ = write!(
                content,
                "\n[truncated: file is {} bytes, showing first {} bytes]",
                metadata.len(),
                MAX_READ_BYTES,
            );
            return Ok(content);
        }

        std::fs::read_to_string(&resolved).map_err(|err| ToolError::Invocation {
            name: "Read".to_string(),
            source: Box::new(err),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn reads_existing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("file.txt");
        let contents = "line one\nline two\n";
        std::fs::write(&path, contents).unwrap();

        let roots = vec![tmp.path().to_path_buf()];
        let handler = ReadHandler;
        let args = json!({ "path": path.to_str().unwrap() });
        let out = handler
            .invoke(ToolInvocation::for_test_with_roots("call-1", &roots), args)
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

    #[test]
    fn schema_contains_expected_fields() {
        let schema = ReadHandler.schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        assert!(
            properties.contains_key("path"),
            "properties missing path: {schema}"
        );

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema must expose a required array")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        assert!(
            required.contains(&"path"),
            "`path` must be required: {required:?}"
        );
    }

    #[tokio::test]
    async fn nonexistent_file_returns_invocation_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.txt");
        let roots = vec![tmp.path().to_path_buf()];
        let handler = ReadHandler;
        let args = json!({ "path": path.to_str().unwrap() });
        let err = handler
            .invoke(ToolInvocation::for_test_with_roots("call-1", &roots), args)
            .await
            .unwrap_err();
        match err {
            ToolError::Invocation { name, .. } => assert_eq!(name, "Read"),
            other => panic!("expected Invocation, got {other:?}"),
        }
    }
}
