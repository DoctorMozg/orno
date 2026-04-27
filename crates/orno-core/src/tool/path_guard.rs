//! Filesystem root-jail for the `Read` / `Write` / `Edit` builtin tools.
//!
//! The jail sits inside the handler — distinct from the effect-based
//! policy gate in `LoopAgent::check_policy_and_invoke`, which short-
//! circuits with a tool-result denial string. A path-traversal attempt
//! is a runtime safety violation, not a policy mismatch the model can
//! retry around, so it returns `ToolError::Denied` and surfaces the
//! offending path to the operator.
//!
//! The check is symlink-safe: both the configured root and the
//! requested path are canonicalized via `Path::canonicalize` before the
//! prefix comparison so a symlink inside the root pointing outside is
//! rejected. Paths containing literal `..` components are rejected
//! before canonicalization to keep the error message readable.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

/// Validate that `requested` resolves inside `root`. Returns the
/// canonical absolute path on success; `ToolError::Denied` on rejection.
///
/// The contract:
/// - Reject any literal `..` component in `requested`.
/// - Canonicalize `root` (which must exist).
/// - For an existing `requested`, canonicalize it.
/// - For a not-yet-existing `requested` (a `Write` target), canonicalize
///   the parent and append the file name. The parent must already exist.
/// - Require the canonical requested path to start with the canonical
///   root path.
pub(super) fn jail_path(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let req_path = Path::new(requested);

    if req_path.components().any(|c| c == Component::ParentDir) {
        return Err(ToolError::Denied {
            reason: format!("path contains `..` components: {requested}"),
        });
    }

    let canon_root = root.canonicalize().map_err(|e| ToolError::Invocation {
        name: "path_guard".to_string(),
        source: Box::new(e),
    })?;

    let canon_req = if req_path.exists() {
        req_path.canonicalize().map_err(|e| ToolError::Invocation {
            name: "path_guard".to_string(),
            source: Box::new(e),
        })?
    } else {
        let parent = req_path.parent().unwrap_or_else(|| Path::new("."));
        let canon_parent = parent.canonicalize().map_err(|e| ToolError::Invocation {
            name: "path_guard".to_string(),
            source: Box::new(e),
        })?;
        let file_name = req_path.file_name().ok_or_else(|| ToolError::Denied {
            reason: format!("path has no file-name component: {requested}"),
        })?;
        canon_parent.join(file_name)
    };

    if !canon_req.starts_with(&canon_root) {
        return Err(ToolError::Denied {
            reason: format!(
                "path `{}` is outside the allowed root `{}`",
                canon_req.display(),
                canon_root.display(),
            ),
        });
    }

    Ok(canon_req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn accepts_path_inside_root() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let inner = tmp.path().join("file.txt");
        fs::write(&inner, b"x").expect("seed file");

        let resolved = jail_path(tmp.path(), inner.to_str().unwrap()).expect("jail accepts");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn accepts_not_yet_existing_path_inside_root() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let target = tmp.path().join("new.txt");

        let resolved = jail_path(tmp.path(), target.to_str().unwrap())
            .expect("jail accepts not-yet-existing target");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_dotdot_components() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let escape = format!("{}/../etc/passwd", tmp.path().display());

        let err = jail_path(tmp.path(), &escape).expect_err("must reject `..`");
        match err {
            ToolError::Denied { reason } => assert!(reason.contains("..")),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn rejects_path_outside_root() {
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");
        let outside = other.path().join("file.txt");
        fs::write(&outside, b"x").expect("seed file");

        let err =
            jail_path(root.path(), outside.to_str().unwrap()).expect_err("must reject outside");
        match err {
            ToolError::Denied { reason } => {
                assert!(reason.contains("outside"), "unexpected reason: {reason}");
            },
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_path_with_no_file_name_component() {
        // An empty path string has no `file_name` component, so a `Write`
        // target would have nowhere to land. Must surface as `Denied` with
        // the file-name reason rather than panic on the missing parent.
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let err = jail_path(tmp.path(), "").expect_err("empty path must be rejected");
        match err {
            ToolError::Denied { reason } => {
                assert!(
                    reason.contains("file-name"),
                    "reason must name the missing file-name component, got {reason:?}",
                );
            },
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn rejects_root_path_outside_temp_root() {
        // `/` exists and canonicalizes to itself, so it goes through the
        // existing-path branch and surfaces as an "outside" denial. The
        // assertion is that the function refuses it rather than letting a
        // request escape to the filesystem root.
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let err = jail_path(tmp.path(), "/").expect_err("root path must be rejected");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied, got {err:?}",
        );
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");
        let target = other.path().join("real.txt");
        fs::write(&target, b"x").expect("seed file");

        let link = root.path().join("link.txt");
        if std::os::unix::fs::symlink(&target, &link).is_err() {
            // Skip on platforms where the test cannot create a symlink
            // (Windows without dev-mode, restricted CI sandboxes).
            return;
        }

        let err = jail_path(root.path(), link.to_str().unwrap())
            .expect_err("symlink escape must be rejected");
        match err {
            ToolError::Denied { reason } => {
                assert!(reason.contains("outside"), "unexpected reason: {reason}");
            },
            other => panic!("expected Denied, got {other:?}"),
        }
    }
}
