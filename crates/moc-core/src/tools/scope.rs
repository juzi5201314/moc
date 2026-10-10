use std::fs;
use std::path::{Path, PathBuf};

use super::ToolError;

pub(crate) fn resolve_scan_root(cwd: &Path, requested: Option<&str>) -> Result<PathBuf, ToolError> {
    let requested = requested.map_or_else(|| Path::new("."), Path::new);
    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        cwd.join(requested)
    };
    fs::canonicalize(&candidate)
        .map_err(|error| ToolError::new("E_ACCESS", format!("{}: {error}", candidate.display())))
}
