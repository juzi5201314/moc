use std::fs;
use std::path::{Path, PathBuf};

use super::ToolError;

/// 展示相对化用的规范化路径：walker 产出的是 canonical 路径，
/// cwd 若还带符号链接段（macOS 上 `/var`→`/private/var`）`strip_prefix` 会失败，
/// 输出就变成绝对路径。canonicalize 失败时保留原值。
pub(crate) fn normalized(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

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
