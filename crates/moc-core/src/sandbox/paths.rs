//! 沙盒路径归一化与包含判断。
//!
//! 授权阶段与执行阶段必须用同一套解析：已存在的路径展开符号链接，不存在的写入目标
//! 先展开最近存在祖先上的链接，再按词法折叠剩余的 `..`。

use std::path::{Component, Path, PathBuf};

/// 把路径解析成真实落点；路径不存在时返回按写入语义折叠后的目标。
pub(crate) fn resolve_access_path(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    resolve_landing(path)
}

/// canonicalize 路径中已存在的最深前缀，再按文件系统语义词法折叠剩余部分。
pub(crate) fn resolve_landing(joined: &Path) -> Option<PathBuf> {
    let mut existing = joined;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            let tail = joined.strip_prefix(existing).ok()?;
            let mut landing = resolved;
            for component in tail.components() {
                match component {
                    Component::Normal(part) => landing.push(part),
                    Component::ParentDir => {
                        landing.pop();
                    }
                    Component::CurDir => {}
                    Component::RootDir | Component::Prefix(_) => return None,
                }
            }
            return Some(landing);
        }
        existing = existing.parent()?;
    }
}

/// `path` 是否位于 `root` 内（含 root 本身）。
///
/// Windows 路径大小写不敏感；其他平台交给 `Path::starts_with` 按组件比较。
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let path = normalize_for_compare(path);
        let root = normalize_for_compare(root);
        path == root
            || root == "/"
            || path
                .strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

#[cfg(windows)]
fn normalize_for_compare(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/").to_lowercase();
    if text.len() > 1 {
        text.trim_end_matches('/').to_owned()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landing_folds_parent_components_after_existing_prefix() {
        let directory = tempfile::tempdir().expect("临时目录");
        let existing = directory.path().join("existing");
        std::fs::create_dir_all(&existing).unwrap();
        let landing = resolve_landing(&existing.join("a/../b")).expect("可解析");
        assert_eq!(landing, existing.join("b"));
    }

    #[test]
    fn within_is_component_aware() {
        let root = Path::new("/work");
        assert!(is_within(Path::new("/work/a"), root));
        assert!(!is_within(Path::new("/workspace/a"), root));
        assert!(!is_within(Path::new("/other"), root));
    }
}
