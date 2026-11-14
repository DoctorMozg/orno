//! Blocking-filesystem helpers shared by the file tools.

use std::io::Write as _;
use std::path::Path;

use crate::error::ToolError;

/// Run blocking filesystem work on the blocking pool so a slow disk or
/// pipe cannot stall a runtime worker.
pub(super) async fn run_blocking<T, F>(name: &'static str, f: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ToolError::Invocation {
            name: name.to_string(),
            source: Box::new(std::io::Error::other(format!("blocking task failed: {e}"))),
        })?
}

/// Stage `content` in a sibling temp file and rename it onto `target`, so
/// readers never see a half-written file. Creates missing parent
/// directories and keeps the permissions of an existing target.
pub(super) fn atomic_write(name: &str, target: &Path, content: &[u8]) -> Result<(), ToolError> {
    let invocation = |e: std::io::Error| ToolError::Invocation {
        name: name.to_string(),
        source: Box::new(e),
    };

    let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(invocation)?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent.unwrap_or_else(|| Path::new(".")))
        .map_err(invocation)?;
    temp.as_file_mut().write_all(content).map_err(invocation)?;
    // NamedTempFile is created 0600; without this an edit would silently
    // strip the executable bit from a 0755 script.
    if let Ok(meta) = std::fs::metadata(target) {
        temp.as_file()
            .set_permissions(meta.permissions())
            .map_err(invocation)?;
    }
    temp.as_file().sync_all().map_err(invocation)?;
    temp.persist(target).map_err(|e| invocation(e.error))?;
    Ok(())
}
