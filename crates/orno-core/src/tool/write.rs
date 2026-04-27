//! `Write` tool — write a file. Requires `allow_mutations`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};

use super::path_guard::jail_path;
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::ToolError;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[schemars(description = "File path to write.")]
    path: String,
    #[schemars(description = "Content to write.")]
    content: String,
}

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
        serde_json::to_value(schemars::schema_for!(WriteArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutations
    }

    #[instrument(skip(self, args), fields(tool.name = "Write", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let WriteArgs { path, content } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "Write".to_string(),
                message: e.to_string(),
            })?;

        // When the agent declared a root, the jail check requires the
        // parent directory to exist for not-yet-existing targets — so
        // create parent dirs first, then jail. The `create_dir_all`
        // call is itself bounded by the original requested path's
        // structure; if a path manages to escape the root, the jail
        // check still rejects after the parent exists.
        let requested = PathBuf::from(&path);
        if let Some(parent) = requested.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::Invocation {
                name: "Write".to_string(),
                source: Box::new(e),
            })?;
        }

        let resolved: PathBuf = if let Some(root) = inv.roots.first() {
            jail_path(root, &path)?
        } else {
            requested
        };

        // Atomic write: stage in a sibling temp file and rename onto
        // the target. Avoids leaving a half-written file on a crash and
        // closes the read-after-truncate window where another reader
        // can observe the file as empty between the truncate and the
        // final flush.
        let parent_dir = resolved.parent().unwrap_or_else(|| Path::new("."));
        let mut temp =
            tempfile::NamedTempFile::new_in(parent_dir).map_err(|e| ToolError::Invocation {
                name: "Write".to_string(),
                source: Box::new(e),
            })?;
        temp.as_file_mut()
            .write_all(content.as_bytes())
            .map_err(|e| ToolError::Invocation {
                name: "Write".to_string(),
                source: Box::new(e),
            })?;
        temp.as_file()
            .sync_all()
            .map_err(|e| ToolError::Invocation {
                name: "Write".to_string(),
                source: Box::new(e),
            })?;
        temp.persist(&resolved).map_err(|e| ToolError::Invocation {
            name: "Write".to_string(),
            source: Box::new(e.error),
        })?;

        let bytes = content.len();
        debug!(file.path = %resolved.display(), file.bytes = bytes, call_id = %inv.call_id, "wrote file");
        Ok(format!("Wrote {bytes} bytes to {}", resolved.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
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

    #[test]
    fn schema_contains_expected_fields() {
        let schema = WriteHandler.schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        for field in ["path", "content"] {
            assert!(
                properties.contains_key(field),
                "properties missing {field}: {schema}"
            );
        }

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema must expose a required array")
            .iter()
            .map(|v| v.as_str().expect("required entries are strings"))
            .collect();
        for field in ["path", "content"] {
            assert!(
                required.contains(&field),
                "`{field}` must be required: {required:?}"
            );
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
