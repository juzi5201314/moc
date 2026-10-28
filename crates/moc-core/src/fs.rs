//! `fs/list_dir` / `fs/create_dir` 的实现：前端目录选择器浏览与新建目录。
//!
//! 只列目录、不列文件；符号链接跟随到目标判定是否为目录。读不出元信息的项
//! 直接跳过（权限不足、悬空链接），不让单个坏项拖垮整个列表。

use std::path::{Component, Path, PathBuf};

use moc_protocol::{FS_LIST_DIR_MAX_ENTRIES, FsCreateDirResult, FsDirEntry, FsListDirResult};

/// 路径解析失败：两种原因都对应前端的 `-32602`。
#[derive(Debug, thiserror::Error)]
pub(crate) enum PathError {
    #[error("无法解析主目录")]
    NoHome,
    #[error("需要绝对路径")]
    NotAbsolute,
}

/// 目录列举失败的原因；`Display` 文案直接回给前端。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ListDirError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("目录无效：{}：{source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("目录无效：{}：不是目录", path.display())]
    NotDirectory { path: PathBuf },
}

/// 新建目录失败的原因；`Display` 文案直接回给前端。
#[derive(Debug, thiserror::Error)]
pub(crate) enum CreateDirError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("目录名无效：{0}")]
    InvalidName(String),
    #[error("目录无效：{}：{source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("目录无效：{}：不是目录", path.display())]
    NotDirectory { path: PathBuf },
    #[error("目录已存在：{}", path.display())]
    Exists { path: PathBuf },
}

/// 列出 `path` 下的子目录；`None` 取 `default_cwd`。
pub(crate) fn list_dir(
    path: Option<&str>,
    default_cwd: &Path,
    show_hidden: bool,
) -> Result<FsListDirResult, ListDirError> {
    let target = match path {
        None => default_cwd.to_path_buf(),
        Some(raw) => expand(raw)?,
    };
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

/// 在 `parent` 下新建名为 `name` 的一层子目录，返回规范化后的绝对路径。
pub(crate) fn create_dir(parent: &str, name: &str) -> Result<FsCreateDirResult, CreateDirError> {
    let name = name.trim();
    let mut components = Path::new(name).components();
    let single_segment =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !single_segment {
        return Err(CreateDirError::InvalidName(name.to_owned()));
    }

    let parent = expand(parent)?;
    let canonical = std::fs::canonicalize(&parent).map_err(|source| CreateDirError::Io {
        path: parent.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(CreateDirError::NotDirectory { path: canonical });
    }

    let target = canonical.join(name);
    match std::fs::create_dir(&target) {
        Ok(()) => Ok(FsCreateDirResult {
            path: target.to_string_lossy().into_owned(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateDirError::Exists { path: target })
        }
        Err(source) => Err(CreateDirError::Io {
            path: target,
            source,
        }),
    }
}

/// 用户输入 → 绝对路径：`~` 展开到主目录，相对路径一律拒绝。
fn expand(raw: &str) -> Result<PathBuf, PathError> {
    if let Some(rest) = raw.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\'))
    {
        let home = crate::paths::home_dir().ok_or(PathError::NoHome)?;
        let rest = rest.trim_start_matches(['/', '\\']);
        return Ok(if rest.is_empty() {
            home
        } else {
            home.join(rest)
        });
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(PathError::NotAbsolute);
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
        assert!(matches!(error, ListDirError::Path(PathError::NotAbsolute)));
        assert_eq!(error.to_string(), "需要绝对路径");
        let file = root.path().join("file.txt");
        assert!(matches!(
            list_dir(file.to_str(), Path::new("/"), false).unwrap_err(),
            ListDirError::NotDirectory { .. }
        ));
    }

    #[test]
    fn creates_one_level_directory_under_parent() {
        let root = fixture();
        let result = create_dir(&root.path().to_string_lossy(), " 新项目 ").unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(result.path, canonical.join("新项目").to_string_lossy());
        assert!(canonical.join("新项目").is_dir());
        let listed = list_dir(root.path().to_str(), Path::new("/"), false).unwrap();
        assert!(listed.entries.iter().any(|entry| entry.name == "新项目"));
    }

    #[test]
    fn rejects_invalid_names_and_existing_directories() {
        let root = fixture();
        let parent = root.path().to_string_lossy().into_owned();
        for name in ["", "   ", ".", "..", "a/b", "../escape"] {
            let error = create_dir(&parent, name).unwrap_err();
            assert!(
                matches!(error, CreateDirError::InvalidName(_)),
                "{name:?} 应被拒绝，得到 {error}"
            );
        }
        let error = create_dir(&parent, "b").unwrap_err();
        assert!(matches!(error, CreateDirError::Exists { .. }));
        assert!(error.to_string().starts_with("目录已存在："), "{error}");
    }

    #[test]
    fn rejects_missing_parent_and_relative_parent() {
        let root = fixture();
        let missing = root.path().join("nope");
        let error = create_dir(&missing.to_string_lossy(), "x").unwrap_err();
        assert!(matches!(error, CreateDirError::Io { .. }));
        let error = create_dir("relative/dir", "x").unwrap_err();
        assert!(matches!(
            error,
            CreateDirError::Path(PathError::NotAbsolute)
        ));
        assert_eq!(error.to_string(), "需要绝对路径");
        let file = root.path().join("file.txt");
        let error = create_dir(&file.to_string_lossy(), "x").unwrap_err();
        assert!(matches!(error, CreateDirError::NotDirectory { .. }));
    }
}
