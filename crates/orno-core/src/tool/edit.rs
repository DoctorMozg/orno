//! `Edit` tool — replace a unique substring in a file.
//! Requires `allow_mutations`.

use std::path::PathBuf;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, instrument};

use super::fs_util::{atomic_write, run_blocking};
use super::path_guard::jail_file;
use super::{ToolEffect, ToolHandler, ToolInvocation};
use crate::error::{RecoverableKind, ToolError};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    #[schemars(description = "File path to edit.")]
    path: String,
    #[schemars(description = "Unique substring to replace.")]
    old_string: String,
    #[schemars(description = "Replacement text.")]
    new_string: String,
}

#[derive(Debug, Default, Clone)]
pub struct EditHandler;

#[async_trait]
impl ToolHandler for EditHandler {
    fn name(&self) -> &str {
        "Edit"
    }
    fn description(&self) -> &str {
        "Replace old_string with new_string in the file at path. Fails if old_string is not unique or not found."
    }
    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(EditArgs)).expect("static schema")
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutations
    }

    fn requires_jail(&self) -> bool {
        true
    }

    #[instrument(skip(self, args), fields(tool.name = "Edit", tool.call_id = %inv.call_id))]
    async fn invoke(&self, inv: ToolInvocation<'_>, args: Value) -> Result<String, ToolError> {
        let EditArgs {
            path,
            old_string,
            new_string,
        } = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            name: "Edit".to_string(),
            message: e.to_string(),
        })?;

        let resolved: PathBuf = if let Some(root) = inv.roots.first() {
            jail_file(root, &path).await?
        } else {
            // BS1: fail closed when roots is empty — no boundary, no edit.
            return Err(ToolError::Denied {
                reason: "Edit tool requires a non-empty `roots` list; \
                         no jail boundary is configured for this agent"
                    .to_string(),
            });
        };

        let target = resolved.clone();
        run_blocking("Edit", move || {
            let original = std::fs::read_to_string(&target)
                .map_err(|e| ToolError::from_io("Edit", &target.display().to_string(), e))?;

            let occurrences = original.matches(&old_string).count();
            if occurrences == 0 {
                return Err(ToolError::Recoverable {
                    name: "Edit".to_string(),
                    kind: RecoverableKind::NoMatch,
                    message: format!("old_string not found in {}", target.display()),
                });
            }
            if occurrences > 1 {
                return Err(ToolError::Recoverable {
                    name: "Edit".to_string(),
                    kind: RecoverableKind::NoMatch,
                    message: format!(
                        "old_string is not unique in {}: found {occurrences} occurrences",
                        target.display(),
                    ),
                });
            }

            let updated = original.replacen(&old_string, &new_string, 1);
            atomic_write("Edit", &target, updated.as_bytes())
        })
        .await?;

        debug!(file.path = %resolved.display(), call_id = %inv.call_id, "edited file");
        Ok(format!(
            "Edited {} (replaced 1 occurrence)",
            resolved.display(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn replaces_unique_occurrence() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("doc.txt");
        std::fs::write(&path, "hello world").expect("seed file");
        let roots = vec![tmp.path().to_path_buf()];

        let handler = EditHandler;
        let result = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "world",
                    "new_string": "there",
                }),
            )
            .await
            .expect("edit succeeds");

        assert!(result.contains("replaced 1 occurrence"));
        let updated = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(updated, "hello there");
    }

    #[tokio::test]
    async fn returns_error_when_old_string_not_found() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("doc.txt");
        std::fs::write(&path, "hello world").expect("seed file");
        let roots = vec![tmp.path().to_path_buf()];

        let handler = EditHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "missing",
                    "new_string": "irrelevant",
                }),
            )
            .await
            .expect_err("edit must fail when old_string absent");

        match err {
            ToolError::Recoverable {
                name,
                kind,
                message,
            } => {
                assert_eq!(name, "Edit");
                assert_eq!(kind, RecoverableKind::NoMatch);
                assert!(
                    message.contains("not found"),
                    "message should mention 'not found': {message}"
                );
            },
            other => panic!("expected Recoverable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_error_when_old_string_ambiguous() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("doc.txt");
        std::fs::write(&path, "foo bar foo baz").expect("seed file");
        let roots = vec![tmp.path().to_path_buf()];

        let handler = EditHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                }),
            )
            .await
            .expect_err("edit must fail when old_string is ambiguous");

        match err {
            ToolError::Recoverable {
                name,
                kind,
                message,
            } => {
                assert_eq!(name, "Edit");
                assert_eq!(kind, RecoverableKind::NoMatch);
                assert!(
                    message.contains("not unique") && message.contains('2'),
                    "message should mention uniqueness and count: {message}"
                );
            },
            other => panic!("expected Recoverable, got {other:?}"),
        }
    }

    #[test]
    fn schema_contains_expected_fields() {
        let schema = EditHandler.schema();

        assert_eq!(
            schema["type"].as_str(),
            Some("object"),
            "schema root must be an object: {schema}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("schema must expose a properties object");
        for field in ["path", "old_string", "new_string"] {
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
        for field in ["path", "old_string", "new_string"] {
            assert!(
                required.contains(&field),
                "`{field}` must be required: {required:?}"
            );
        }
    }

    #[tokio::test]
    async fn missing_old_string_arg_returns_invalid_args() {
        let handler = EditHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({ "path": "x", "new_string": "y" }),
            )
            .await
            .expect_err("missing old_string must fail");

        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "Edit");
                assert!(
                    message.contains("old_string"),
                    "message should name the missing field: {message}"
                );
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_keeps_executable_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("run.sh");
        std::fs::write(&path, "echo old\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let roots = vec![tmp.path().to_path_buf()];

        EditHandler
            .invoke(
                ToolInvocation::for_test_with_roots("call-1", &roots),
                json!({ "path": "run.sh", "old_string": "old", "new_string": "new" }),
            )
            .await
            .expect("edit succeeds");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo new\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_rejects_fifo() {
        let tmp = TempDir::new().expect("create tempdir");
        let fifo = tmp.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let roots = vec![tmp.path().to_path_buf()];
        let call = EditHandler.invoke(
            ToolInvocation::for_test_with_roots("call-1", &roots),
            json!({ "path": "pipe", "old_string": "a", "new_string": "b" }),
        );
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("Edit must not block on a FIFO")
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
    }
}
