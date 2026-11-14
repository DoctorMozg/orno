//! `Write` tool — write a file. Requires `allow_mutations`.

use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};

use super::fs_util::{atomic_write, run_blocking};
use super::path_guard::jail_file;
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

    fn requires_jail(&self) -> bool {
        true
    }

    #[instrument(skip(self, args), fields(tool.name = "Write", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let WriteArgs { path, content } =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
                name: "Write".to_string(),
                message: e.to_string(),
            })?;

        // Jail FIRST, then create parent dirs of the resolved (jailed)
        // path. Creating directories before the jail check would let an
        // LLM with `allow_mutations` mkdir anywhere the process can,
        // even if the subsequent write is rejected — a containment
        // violation independent of the actual file content (F1).
        // BS1: fail closed when roots is empty — no boundary, no write.
        let resolved: PathBuf = if let Some(root) = inv.roots.first() {
            jail_file(root, &path).await?
        } else {
            return Err(ToolError::Denied {
                reason: "Write tool requires a non-empty `roots` list; \
                         no jail boundary is configured for this agent"
                    .to_string(),
            });
        };

        let target = resolved.clone();
        let bytes = content.len();
        run_blocking("Write", move || {
            atomic_write("Write", &target, content.as_bytes())
        })
        .await?;
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
        let roots = vec![tmp.path().to_path_buf()];
        let handler = WriteHandler;

        let result = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
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
        let roots = vec![tmp.path().to_path_buf()];

        let handler = WriteHandler;
        handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
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
        let roots = vec![tmp.path().to_path_buf()];
        let handler = WriteHandler;

        handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": path.to_str().unwrap(), "content": "deep" }),
            )
            .await
            .expect("write with nested parents succeeds");

        let written = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(written, "deep");
    }

    #[tokio::test]
    async fn write_jail_does_not_create_dirs_outside_root() {
        // F1 regression: `create_dir_all` must NOT fire on the raw path before
        // jail_path runs. An LLM attempting to write outside the root must get
        // `Denied` WITHOUT any side-effects (no dirs created outside root).
        let root = TempDir::new().expect("create root");
        let outside = TempDir::new().expect("create outside");
        // Target is under `outside` dir, which is outside `root`
        let evil_path = outside.path().join("injected").join("evil.txt");
        let roots = vec![root.path().to_path_buf()];
        let handler = WriteHandler;

        let err = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": evil_path.to_str().unwrap(), "content": "evil" }),
            )
            .await
            .expect_err("write outside root must fail");

        // Jail must fire BEFORE any mkdir
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied, got {err:?}",
        );
        // No side-effects: the injected/ dir must NOT have been created
        assert!(
            !outside.path().join("injected").exists(),
            "create_dir_all must not have run before the jail check",
        );
    }

    #[tokio::test]
    async fn write_creates_nested_dirs_inside_root() {
        // When the path is inside the root but parent dirs don't exist,
        // write must create them after the jail check succeeds.
        let root = TempDir::new().expect("create root");
        let nested = root.path().join("a").join("b").join("c").join("file.txt");
        let roots = vec![root.path().to_path_buf()];
        let handler = WriteHandler;

        handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": nested.to_str().unwrap(), "content": "ok" }),
            )
            .await
            .expect("nested write inside root must succeed");

        let written = std::fs::read_to_string(&nested).expect("read back");
        assert_eq!(written, "ok");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_under_escaping_symlink_is_denied_and_creates_nothing() {
        let root = TempDir::new().expect("create root");
        let outside = TempDir::new().expect("create outside");
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).expect("symlink");
        let roots = vec![root.path().to_path_buf()];

        let err = WriteHandler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": "link/sub/new.txt", "content": "evil" }),
            )
            .await
            .expect_err("symlink escape must be denied");

        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
        assert_eq!(
            std::fs::read_dir(outside.path()).unwrap().count(),
            0,
            "nothing may be created outside the root",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn overwrite_keeps_executable_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("run.sh");
        std::fs::write(&path, "#!/bin/sh\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let roots = vec![tmp.path().to_path_buf()];

        WriteHandler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": "run.sh", "content": "#!/bin/sh\necho hi\n" }),
            )
            .await
            .expect("overwrite succeeds");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}
