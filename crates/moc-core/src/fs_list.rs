//! `fs/list_dir` 的实现：列出目录下的子目录，供前端选工作目录。
//!
//! 只列目录、不列文件；符号链接跟随到目标判定是否为目录。读不出元信息的项
//! 直接跳过（权限不足、悬空链接），不让单个坏项拖垮整个列表。

use std::path::{Path, PathBuf};

use moc_protocol::{FS_LIST_DIR_MAX_ENTRIES, FsDirEntry, FsListDirResult};

/// 目录列举失败的原因；`Display` 文案直接回给前端。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ListDirError {
    #[error("无法解析主目录")]
    NoHome,
    #[error("需要绝对路径")]
    NotAbsolute,
    #[error("目录无效：{}：{source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("目录无效：{}：不是目录", path.display())]
    NotDirectory { path: PathBuf },
}

/// 列出 `path` 下的子目录；`None` 取 `default_cwd`。
pub(crate) fn list_dir(
    path: Option<&str>,
    default_cwd: &Path,
    show_hidden: bool,
) -> Result<FsListDirResult, ListDirError> {
    let target = resolve(path, default_cwd)?;
    let canonical = std::fs::canonicalize(&target).map_err(|source| ListDirError::Io {
        path: target.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(ListDirError::NotDirectory { path: canonical });
    }
    let reader = std::fs::read_dir(&canonical).map_err(|source| ListDirError::Io {
        path: canonical.clone(),
        source,
    })?;

    let mut entries = Vec::new();
    for item in reader.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = item.path();
        let Ok(metadata) = std::fs::metadata(&entry_path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        entries.push(FsDirEntry {
            git: entry_path.join(".git").exists(),
            path: entry_path.to_string_lossy().into_owned(),
            name,
        });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    let truncated = entries.len() > FS_LIST_DIR_MAX_ENTRIES;
    entries.truncate(FS_LIST_DIR_MAX_ENTRIES);

    Ok(FsListDirResult {
        parent: canonical
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned()),
        home: crate::paths::home_dir().map(|home| home.to_string_lossy().into_owned()),
        path: canonical.to_string_lossy().into_owned(),
        entries,
        truncated,
    })
}

fn resolve(path: Option<&str>, default_cwd: &Path) -> Result<PathBuf, ListDirError> {
    let Some(raw) = path else {
        return Ok(default_cwd.to_path_buf());
    };
    if let Some(rest) = raw.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
    {
        let home = crate::paths::home_dir().ok_or(ListDirError::NoHome)?;
        let rest = rest.trim_start_matches(['/', '\\']);
        return Ok(if rest.is_empty() {
            home
        } else {
            home.join(rest)
        });
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(ListDirError::NotAbsolute);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("临时目录");
        std::fs::create_dir_all(root.path().join("b")).unwrap();
        std::fs::create_dir_all(root.path().join("A/.git")).unwrap();
        std::fs::create_dir_all(root.path().join(".hidden")).unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        root
    }

    #[test]
    fn lists_only_directories_sorted_case_insensitively() {
        let root = fixture();
        let result = list_dir(root.path().to_str(), Path::new("/"), false).unwrap();
        let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "b"]);
        assert!(result.entries[0].git, "A 下有 .git");
        assert!(!result.entries[1].git);
        assert!(!result.truncated);
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(result.path, canonical.to_string_lossy());
        assert_eq!(
            result.parent.as_deref(),
            canonical.parent().map(|p| p.to_string_lossy()).as_deref()
        );
    }

    #[test]
    fn hidden_directories_are_opt_in() {
        let root = fixture();
        let result = list_dir(root.path().to_str(), Path::new("/"), true).unwrap();
        let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".hidden", "A", "b"]);
    }

    #[test]
    fn none_uses_default_cwd() {
        let root = fixture();
        let result = list_dir(None, root.path(), false).unwrap();
        assert_eq!(result.entries.len(), 2);
    }

    #[test]
    fn rejects_missing_relative_and_file_paths() {
        let root = fixture();
        let missing = root.path().join("nope");
        let error = list_dir(missing.to_str(), Path::new("/"), false).unwrap_err();
        assert!(matches!(error, ListDirError::Io { .. }));
        assert!(error.to_string().starts_with("目录无效："), "{error}");
        let error = list_dir(Some("relative/dir"), Path::new("/"), false).unwrap_err();
        assert!(matches!(error, ListDirError::NotAbsolute));
        assert_eq!(error.to_string(), "需要绝对路径");
        let file = root.path().join("file.txt");
        assert!(matches!(
            list_dir(file.to_str(), Path::new("/"), false).unwrap_err(),
            ListDirError::NotDirectory { .. }
        ));
    }
}
