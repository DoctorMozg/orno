//! Filesystem root-jail for the `Read` / `Write` / `Edit` / `Bash`
//! builtin tools.
//!
//! The jail sits inside the handler — distinct from the effect-based
//! policy gate in `LoopAgent::check_policy_and_invoke`, which short-
//! circuits with a tool-result denial string. A path-traversal attempt
//! is a runtime safety violation, not a policy mismatch the model can
//! retry around, so it returns `ToolError::Denied` and surfaces the
//! offending path to the operator.
//!
//! An agent may declare more than one allowed root. `jail_path_any`
//! accepts a path that resolves inside any one of them; `jail_path` is
//! the single-root primitive it loops over.
//!
//! The check is symlink-safe for both existing and not-yet-existing
//! paths. For an existing path, the requested path is canonicalized via
//! `Path::canonicalize` before the prefix comparison. For a not-yet-
//! existing path (a `Write` / `Edit` create target), the deepest
//! ancestor that does exist is canonicalized instead — so a symlinked
//! intermediate directory pointing outside the root is rejected before
//! the non-existing tail is re-appended. Paths containing literal `..`
//! components are rejected before canonicalization to keep the error
//! message readable.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

/// Resolve `.` components in `path` without touching the filesystem.
/// `..` is NOT handled here — callers must reject `..` before calling.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {},
            other => out.push(other),
        }
    }
    out
}

/// Validate that `requested` resolves inside `root`. Returns the
/// canonical absolute path on success; `ToolError::Denied` on rejection.
///
/// The contract:
/// - Reject any literal `..` component in `requested`.
/// - Canonicalize `root` (which must exist).
/// - For an existing `requested`, canonicalize it.
/// - For a not-yet-existing `requested` (a `Write` target), build the
///   absolute path by joining the canonical root with the requested path
///   (relative) or normalizing an absolute path. Intermediate parents
///   are NOT required to exist — the handler creates them after the
///   jail check succeeds.
/// - Require the resolved requested path to start with the canonical
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
        resolve_nonexisting(req_path, requested, &canon_root)?
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

/// Resolve a `requested` path that does not yet exist on disk into an
/// absolute path safe to hand back to the handler. `canon_root` is the
/// already-canonicalized jail root; `requested` is only used for error
/// messages. `..` components are assumed already rejected by the caller.
///
/// The deepest ancestor that *does* exist is canonicalized so a
/// symlinked intermediate directory pointing outside the root is caught
/// before the prefix check. The non-existing tail beyond that ancestor
/// cannot itself contain a symlink (it does not exist), so re-appending
/// it onto the canonical ancestor stays inside the jail.
fn resolve_nonexisting(
    req_path: &Path,
    requested: &str,
    canon_root: &Path,
) -> Result<PathBuf, ToolError> {
    if req_path.file_name().is_none() {
        return Err(ToolError::Denied {
            reason: format!("path has no file-name component: {requested}"),
        });
    }
    let abs = if req_path.is_absolute() {
        normalize_path(req_path)
    } else {
        normalize_path(&canon_root.join(req_path))
    };
    let mut ancestor: &Path = abs.as_path();
    while !ancestor.exists() {
        ancestor = match ancestor.parent() {
            Some(parent) => parent,
            None => {
                return Err(ToolError::Denied {
                    reason: format!("path `{}` has no existing ancestor", abs.display()),
                });
            },
        };
    }
    let canon_ancestor = ancestor.canonicalize().map_err(|e| ToolError::Invocation {
        name: "path_guard".to_string(),
        source: Box::new(e),
    })?;
    if !canon_ancestor.starts_with(canon_root) {
        return Err(ToolError::Denied {
            reason: format!(
                "path `{}` resolves outside the allowed root `{}` (via `{}`)",
                abs.display(),
                canon_root.display(),
                canon_ancestor.display(),
            ),
        });
    }
    let tail = abs
        .strip_prefix(ancestor)
        .expect("ancestor is a prefix of abs by construction");
    Ok(canon_ancestor.join(tail))
}

/// Validate `requested` against every entry in `roots`, returning the
/// canonical path from the first root that accepts it.
///
/// An agent's `AgentPolicy.roots` may list more than one directory; a
/// path is in-jail when it resolves inside *any* of them. Roots that
/// fail to canonicalize (e.g. a configured directory that does not
/// exist) are skipped rather than aborting the whole check, so one
/// stale entry does not deny an otherwise-valid path. A literal `..`
/// component is path-intrinsic and rejected up front. When no root
/// accepts the path — including the empty-`roots` case — a single
/// `ToolError::Denied` is returned naming what was tried.
pub(super) fn jail_path_any(roots: &[PathBuf], requested: &str) -> Result<PathBuf, ToolError> {
    if Path::new(requested)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return Err(ToolError::Denied {
            reason: format!("path contains `..` components: {requested}"),
        });
    }
    if roots.is_empty() {
        return Err(ToolError::Denied {
            reason: "no jail boundary configured (empty roots)".to_string(),
        });
    }
    for root in roots {
        if let Ok(resolved) = jail_path(root, requested) {
            return Ok(resolved);
        }
    }
    let tried = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(ToolError::Denied {
        reason: format!(
            "path `{requested}` is outside all {} configured roots: [{tried}]",
            roots.len(),
        ),
    })
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

    #[test]
    fn accepts_nonexisting_multi_level_path_inside_root() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        // subdir/nested/ does not exist — the old code would fail here
        let target = tmp.path().join("subdir").join("nested").join("file.txt");
        let resolved = jail_path(tmp.path(), target.to_str().unwrap())
            .expect("multi-level non-existing path inside root must be accepted");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_absolute_path_outside_root_when_nonexistent() {
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");
        // Target is in `other` dir and does not exist — must be rejected
        let outside = other.path().join("secret").join("file.txt");
        let err = jail_path(root.path(), outside.to_str().unwrap())
            .expect_err("absolute non-existing path outside root must be rejected");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied, got {err:?}"
        );
    }

    #[test]
    fn accepts_relative_path_resolving_to_curdot_inside_root() {
        // ./subdir/file.txt — the `.` component must be normalized away
        // when the relative path is joined against the canonical root.
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let inner = tmp.path().join("subdir").join("file.txt");
        // Seed the file so the assertion has something to round-trip
        // against, but the jail goes through the non-existing branch
        // because the relative path is resolved against the test's cwd.
        fs::create_dir_all(inner.parent().unwrap()).expect("create subdir");
        fs::write(&inner, b"x").expect("seed file");
        let rel = "./subdir/file.txt".to_string();
        let resolved =
            jail_path(tmp.path(), &rel).expect("relative path with ./ prefix must be accepted");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_symlink_escape_via_nonexisting_intermediate_dir() {
        // A symlinked directory inside the root points outside it. A
        // *not-yet-existing* target under that symlink must be rejected:
        // the textual `starts_with` would accept it, but canonicalizing
        // the deepest existing ancestor (the symlink) resolves to the
        // outside directory and the prefix check fails.
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");

        let link = root.path().join("link");
        if std::os::unix::fs::symlink(other.path(), &link).is_err() {
            // Skip where symlink creation is unavailable.
            return;
        }

        let escape = link.join("new.txt");
        let err = jail_path(root.path(), escape.to_str().unwrap())
            .expect_err("non-existing target under a symlinked dir must be rejected");
        match err {
            ToolError::Denied { reason } => {
                assert!(reason.contains("outside"), "unexpected reason: {reason}");
            },
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn jail_path_any_accepts_path_in_second_root() {
        let first = tempfile::TempDir::new().expect("create first root");
        let second = tempfile::TempDir::new().expect("create second root");
        let inner = second.path().join("file.txt");
        fs::write(&inner, b"x").expect("seed file");

        let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        let resolved = jail_path_any(&roots, inner.to_str().unwrap())
            .expect("path inside the second root must be accepted");
        assert!(resolved.starts_with(second.path().canonicalize().unwrap()));
    }

    #[test]
    fn jail_path_any_denies_path_in_no_root() {
        let first = tempfile::TempDir::new().expect("create first root");
        let second = tempfile::TempDir::new().expect("create second root");
        let other = tempfile::TempDir::new().expect("create other");
        let outside = other.path().join("file.txt");
        fs::write(&outside, b"x").expect("seed file");

        let roots = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        let err = jail_path_any(&roots, outside.to_str().unwrap())
            .expect_err("path outside every root must be denied");
        match err {
            ToolError::Denied { reason } => {
                assert!(
                    reason.contains("outside all"),
                    "unexpected reason: {reason}"
                );
            },
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn jail_path_any_skips_nonexistent_root() {
        // A configured root that does not exist must be skipped, not
        // abort the whole check — a later valid root still resolves.
        let missing = PathBuf::from("/orno-nonexistent-root-xyz");
        let valid = tempfile::TempDir::new().expect("create valid root");
        let inner = valid.path().join("file.txt");
        fs::write(&inner, b"x").expect("seed file");

        let roots = vec![missing, valid.path().to_path_buf()];
        let resolved = jail_path_any(&roots, inner.to_str().unwrap())
            .expect("a nonexistent root must be skipped, not fatal");
        assert!(resolved.starts_with(valid.path().canonicalize().unwrap()));
    }

    #[test]
    fn jail_path_any_rejects_dotdot() {
        let root = tempfile::TempDir::new().expect("create root");
        let roots = vec![root.path().to_path_buf()];
        let escape = format!("{}/../etc/passwd", root.path().display());
        let err = jail_path_any(&roots, &escape).expect_err("must reject `..`");
        match err {
            ToolError::Denied { reason } => assert!(reason.contains("..")),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn jail_path_any_empty_roots_denied() {
        let err = jail_path_any(&[], "/etc/passwd").expect_err("empty roots must deny every path");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied, got {err:?}",
        );
    }
}
