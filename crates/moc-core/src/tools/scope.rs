use std::fs;
use std::path::{Path, PathBuf};

use super::ToolError;

pub(crate) fn resolve_scan_root(cwd: &Path, requested: Option<&str>) -> Result<PathBuf, ToolError> {
    let workspace = fs::canonicalize(cwd).map_err(|error| {
        ToolError::new("E_SCAN_SCOPE", format!("workspace is unavailable: {error}"))
    })?;
    if is_filesystem_mount(&workspace) {
        return Err(scope_error(&workspace));
    }
    let requested = requested.map_or_else(|| Path::new("."), Path::new);
    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        workspace.join(requested)
    };
    let root = fs::canonicalize(&candidate)
        .map_err(|error| ToolError::new("E_ACCESS", format!("{}: {error}", candidate.display())))?;
    if !root.starts_with(&workspace) {
        return Err(ToolError::new(
            "E_SCAN_SCOPE",
            format!("scan root {} is outside the workspace", root.display()),
        ));
    }
    if is_slow_filesystem_path(&root) || is_filesystem_mount(&root) {
        return Err(scope_error(&root));
    }
    Ok(root)
}

pub(crate) fn is_slow_filesystem_path(path: &Path) -> bool {
    #[cfg(unix)]
    {
        path.starts_with("/mnt") || path.starts_with("/media")
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

pub(crate) fn is_filesystem_mount(path: &Path) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return true;
    };
    let Ok(path_metadata) = fs::metadata(&path) else {
        return false;
    };
    let Ok(parent_metadata) = fs::metadata(parent) else {
        return false;
    };
    filesystem_device(&path_metadata) != filesystem_device(&parent_metadata)
}

#[cfg(unix)]
fn filesystem_device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.dev()
}

#[cfg(not(unix))]
fn filesystem_device(_metadata: &fs::Metadata) -> u64 {
    0
}

fn scope_error(path: &Path) -> ToolError {
    ToolError::new(
        "E_SCAN_SCOPE",
        format!(
            "refusing to recursively scan slow or mounted filesystem {}",
            path.display()
        ),
    )
}
