//! Filesystem root-jail for the `Read` / `Write` / `Edit` builtin tools
//! and the `Bash` `cwd`.
//!
//! The jail sits inside the handler, distinct from the effect-based
//! policy gate in `LoopAgent::check_policy_and_invoke`. A path-traversal
//! attempt is a runtime safety violation, not a policy mismatch the
//! model can retry around, so it returns `ToolError::Denied`.
//!
//! Resolution is symlink-safe even for targets that do not exist yet:
//! the deepest existing ancestor is canonicalized and must sit inside the
//! canonical root, then the not-yet-existing tail is re-appended. Literal
//! `..` components are rejected up front, so the tail cannot climb out.

use std::path::{Component, Path, PathBuf};

use crate::error::ToolError;

use super::fs_util::run_blocking;

fn denied(reason: String) -> ToolError {
    ToolError::Denied { reason }
}

fn io_err(source: std::io::Error) -> ToolError {
    ToolError::Invocation {
        name: "path_guard".to_string(),
        source: Box::new(source),
    }
}

/// Drop `.` components without touching the filesystem. `..` is NOT
/// handled here; callers reject it first.
fn normalize_path(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| *c != Component::CurDir)
        .collect()
}

/// Resolve `requested` against `root` and verify the result stays inside
/// it. Returns the canonical absolute path; `ToolError::Denied` on
/// rejection. Relative inputs are joined to the canonical root (never the
/// process cwd). The target and intermediate directories need not exist.
pub(super) fn resolve_in_root(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let req_path = Path::new(requested);

    if req_path.components().any(|c| c == Component::ParentDir) {
        return Err(denied(format!(
            "path contains `..` components: {requested}"
        )));
    }
    if requested.is_empty() {
        return Err(denied(
            "path has no file-name component: empty path".to_string(),
        ));
    }

    let canon_root = root.canonicalize().map_err(io_err)?;
    let joined = normalize_path(&canon_root.join(req_path));

    // `symlink_metadata` so a dangling symlink counts as existing and
    // fails canonicalization instead of being treated as a free slot.
    let existing = joined
        .ancestors()
        .find(|a| a.symlink_metadata().is_ok())
        .ok_or_else(|| denied(format!("no existing ancestor for path: {requested}")))?;
    let canon_existing = existing.canonicalize().map_err(io_err)?;

    if !canon_existing.starts_with(&canon_root) {
        return Err(denied(format!(
            "path `{}` is outside the allowed root `{}`",
            canon_existing.display(),
            canon_root.display(),
        )));
    }

    let tail = joined
        .strip_prefix(existing)
        .expect("ancestor is a prefix of the path it came from");
    if tail.as_os_str().is_empty() {
        return Ok(canon_existing);
    }
    if joined.file_name().is_none() {
        return Err(denied(format!(
            "path has no file-name component: {requested}"
        )));
    }
    Ok(canon_existing.join(tail))
}

/// Reject an existing target that is not a regular file (FIFO, device,
/// directory). Opening a FIFO blocks forever, so this must run before any
/// open. A missing target is fine.
fn ensure_regular_file(path: &Path) -> Result<(), ToolError> {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => Err(denied(format!(
            "path `{}` is not a regular file",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(e)),
    }
}

/// Resolve a directory-or-file path inside `root` off the async runtime.
pub(super) async fn jail_path(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let (root, requested) = (root.to_path_buf(), requested.to_string());
    run_blocking("path_guard", move || resolve_in_root(&root, &requested)).await
}

/// Like [`jail_path`], additionally rejecting non-regular existing targets.
pub(super) async fn jail_file(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let (root, requested) = (root.to_path_buf(), requested.to_string());
    run_blocking("path_guard", move || {
        let resolved = resolve_in_root(&root, &requested)?;
        ensure_regular_file(&resolved)?;
        Ok(resolved)
    })
    .await
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

        let resolved = resolve_in_root(tmp.path(), inner.to_str().unwrap()).expect("jail accepts");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn accepts_not_yet_existing_path_inside_root() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let target = tmp.path().join("new.txt");

        let resolved = resolve_in_root(tmp.path(), target.to_str().unwrap())
            .expect("jail accepts not-yet-existing target");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_dotdot_components() {
        let tmp = tempfile::TempDir::new().expect("create tempdir");
        let escape = format!("{}/../etc/passwd", tmp.path().display());

        let err = resolve_in_root(tmp.path(), &escape).expect_err("must reject `..`");
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

        let err = resolve_in_root(root.path(), outside.to_str().unwrap())
            .expect_err("must reject outside");
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
        let err = resolve_in_root(tmp.path(), "").expect_err("empty path must be rejected");
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
        let err = resolve_in_root(tmp.path(), "/").expect_err("root path must be rejected");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "expected Denied, got {err:?}",
        );
    }

    #[cfg(unix)]
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

        let err = resolve_in_root(root.path(), link.to_str().unwrap())
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
        let resolved = resolve_in_root(tmp.path(), target.to_str().unwrap())
            .expect("multi-level non-existing path inside root must be accepted");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn rejects_absolute_path_outside_root_when_nonexistent() {
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");
        // Target is in `other` dir and does not exist — must be rejected
        let outside = other.path().join("secret").join("file.txt");
        let err = resolve_in_root(root.path(), outside.to_str().unwrap())
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
        let resolved = resolve_in_root(tmp.path(), &rel)
            .expect("relative path with ./ prefix must be accepted");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_new_file_under_symlinked_dir_pointing_outside() {
        let root = tempfile::TempDir::new().expect("create root");
        let other = tempfile::TempDir::new().expect("create other");
        std::os::unix::fs::symlink(other.path(), root.path().join("link")).expect("symlink");

        let err = resolve_in_root(root.path(), "link/sub/new.txt")
            .expect_err("write below an escaping symlink must be denied");
        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn accepts_new_file_under_symlinked_dir_pointing_inside() {
        let root = tempfile::TempDir::new().expect("create root");
        fs::create_dir(root.path().join("real")).expect("mkdir");
        std::os::unix::fs::symlink(root.path().join("real"), root.path().join("link"))
            .expect("symlink");

        let resolved = resolve_in_root(root.path(), "link/new.txt").expect("inside symlink ok");
        assert_eq!(
            resolved,
            root.path().canonicalize().unwrap().join("real/new.txt")
        );
    }

    #[test]
    fn relative_existing_path_resolves_against_root_not_cwd() {
        // The test process cwd is the crate dir, which has a Cargo.toml;
        // the root has one too, and the root's must win.
        let root = tempfile::TempDir::new().expect("create root");
        fs::write(root.path().join("Cargo.toml"), b"root copy").expect("seed");

        let resolved = resolve_in_root(root.path(), "Cargo.toml").expect("resolves in root");
        assert_eq!(
            resolved,
            root.path().canonicalize().unwrap().join("Cargo.toml")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn jail_file_rejects_fifo_and_directory() {
        let root = tempfile::TempDir::new().expect("create root");
        let fifo = root.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo available");
        assert!(status.success());
        fs::create_dir(root.path().join("dir")).expect("mkdir");

        for name in ["pipe", "dir"] {
            let err = jail_file(root.path(), name)
                .await
                .expect_err("non-regular file must be rejected");
            match err {
                ToolError::Denied { reason } => assert!(reason.contains("regular"), "{reason}"),
                other => panic!("expected Denied, got {other:?}"),
            }
        }
    }
}
