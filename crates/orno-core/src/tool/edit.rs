//! `Edit` tool — replace a unique substring in a file.
//! Requires `allow_mutations`.

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
            jail_path(root, &path)?
        } else {
            PathBuf::from(&path)
        };

        let original = std::fs::read_to_string(&resolved).map_err(|e| ToolError::Invocation {
            name: "Edit".to_string(),
            source: Box::new(e),
        })?;

        let occurrences = original.matches(&old_string).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidArgs {
                name: "Edit".to_string(),
                message: format!("old_string not found in {}", resolved.display()),
            });
        }
        if occurrences > 1 {
            return Err(ToolError::InvalidArgs {
                name: "Edit".to_string(),
                message: format!(
                    "old_string is not unique in {}: found {occurrences} occurrences",
                    resolved.display(),
                ),
            });
        }

        let updated = original.replacen(&old_string, &new_string, 1);

        // Atomic write: stage in a sibling temp file and rename onto
        // the target. Same rationale as `Write` — avoids a half-written
        // file on a crash and prevents another reader from observing
        // an empty file mid-write.
        let parent_dir = resolved.parent().unwrap_or_else(|| Path::new("."));
        let mut temp =
            tempfile::NamedTempFile::new_in(parent_dir).map_err(|e| ToolError::Invocation {
                name: "Edit".to_string(),
                source: Box::new(e),
            })?;
        temp.as_file_mut()
            .write_all(updated.as_bytes())
            .map_err(|e| ToolError::Invocation {
                name: "Edit".to_string(),
                source: Box::new(e),
            })?;
        temp.as_file()
            .sync_all()
            .map_err(|e| ToolError::Invocation {
                name: "Edit".to_string(),
                source: Box::new(e),
            })?;
        temp.persist(&resolved).map_err(|e| ToolError::Invocation {
            name: "Edit".to_string(),
            source: Box::new(e.error),
        })?;

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

        let handler = EditHandler;
        let result = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
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

        let handler = EditHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "missing",
                    "new_string": "irrelevant",
                }),
            )
            .await
            .expect_err("edit must fail when old_string absent");

        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "Edit");
                assert!(
                    message.contains("not found"),
                    "message should mention 'not found': {message}"
                );
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_error_when_old_string_ambiguous() {
        let tmp = TempDir::new().expect("create tempdir");
        let path = tmp.path().join("doc.txt");
        std::fs::write(&path, "foo bar foo baz").expect("seed file");

        let handler = EditHandler;
        let err = handler
            .invoke(
                ToolInvocation::for_test("call-1"),
                json!({
                    "path": path.to_str().unwrap(),
                    "old_string": "foo",
                    "new_string": "qux",
                }),
            )
            .await
            .expect_err("edit must fail when old_string is ambiguous");

        match err {
            ToolError::InvalidArgs { name, message } => {
                assert_eq!(name, "Edit");
                assert!(
                    message.contains("not unique") && message.contains('2'),
                    "message should mention uniqueness and count: {message}"
                );
            },
            other => panic!("expected InvalidArgs, got {other:?}"),
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
}
