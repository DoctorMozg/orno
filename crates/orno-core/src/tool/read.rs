//! `Read` tool — read a file's contents. Read-only effect.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tracing::instrument;

use super::fs_util::run_blocking;
use super::path_guard::jail_file;
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

    #[instrument(skip(self, args), fields(tool.name = "Read", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let ReadArgs { path } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "Read".to_string(),
                message: e.to_string(),
            })?;

        let resolved: PathBuf = if let Some(root) = inv.roots.first() {
            jail_file(root, &path).await?
        } else {
            // BS1: empty roots means no jail boundary — refuse rather than
            // allowing unrestricted host filesystem access.
            return Err(ToolError::Denied {
                reason: "Read tool requires a non-empty `roots` list; \
                         no jail boundary is configured for this agent"
                    .to_string(),
            });
        };

        run_blocking("Read", move || read_capped_lossy(&resolved)).await
    }
}

/// Read up to `MAX_READ_BYTES` and decode lossily, so invalid UTF-8
/// behaves the same for small and large files.
fn read_capped_lossy(path: &Path) -> Result<String, ToolError> {
    let io_failure = |e: std::io::Error| ToolError::from_io("Read", &path.display().to_string(), e);
    let file = std::fs::File::open(path).map_err(io_failure)?;
    let file_len = file.metadata().map_err(io_failure)?.len();
    let mut buf = Vec::new();
    file.take(MAX_READ_BYTES)
        .read_to_end(&mut buf)
        .map_err(io_failure)?;
    let mut content = String::from_utf8_lossy(&buf).into_owned();
    if file_len > MAX_READ_BYTES {
        let _ = write!(
            content,
            "\n[truncated: file is {file_len} bytes, showing first {MAX_READ_BYTES} bytes]",
        );
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RecoverableKind;
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
    async fn nonexistent_file_returns_recoverable_not_found() {
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
            ToolError::Recoverable { name, kind, .. } => {
                assert_eq!(name, "Read");
                assert_eq!(kind, RecoverableKind::NotFound);
            },
            other => panic!("expected Recoverable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_roots_returns_denied() {
        let handler = ReadHandler;
        let args = json!({ "path": "/etc/passwd" });
        let err = handler
            .invoke(ToolInvocation::for_test("call-1"), args)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied when roots is empty, got {err:?}"
        );
    }

    #[tokio::test]
    async fn relative_path_reads_root_copy_not_cwd_copy() {
        // The test cwd (the crate dir) also contains a Cargo.toml.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "root copy").unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let out = ReadHandler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": "Cargo.toml" }),
            )
            .await
            .unwrap();
        assert_eq!(out, "root copy");
    }

    #[tokio::test]
    async fn invalid_utf8_is_decoded_lossily() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("bin.txt"), b"ok\xff\xfeend").unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        let out = ReadHandler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": "bin.txt" }),
            )
            .await
            .unwrap();
        assert_eq!(out, "ok\u{fffd}\u{fffd}end");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_is_rejected_instead_of_hanging() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fifo = tmp.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let roots = vec![tmp.path().to_path_buf()];
        let call = ReadHandler.invoke(
            ToolInvocation::for_test_with_roots("call-1", &roots),
            json!({ "path": "pipe" }),
        );
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("Read must not block on a FIFO")
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
    }
}
