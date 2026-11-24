//! `fs/*` 的实现：目录选择器、文件管理器与编辑器读写的文件系统面。
//!
//! `list_dir` 默认只列目录，`include_files` 时附带文件；符号链接跟随到目标
//! 判定类型。读不出元信息的项直接跳过（权限不足、悬空链接），不让单个坏项
//! 拖垮整个列表。

use moc_i18n::t;
use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use moc_protocol::{
    FS_LIST_DIR_MAX_ENTRIES, FS_READ_FILE_DEFAULT_MAX_BYTES, FS_READ_FILE_MAX_BYTES,
    FsCreateDirResult, FsCreateFileResult, FsDirEntry, FsEntryKind, FsListDirResult,
    FsRenameResult,
};

/// 路径解析失败：两种原因都对应前端的 `-32602`。
#[derive(Debug)]
pub enum PathError {
    NoHome,
    NotAbsolute,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str(&t!("core.fs.no_home")),
            Self::NotAbsolute => f.write_str(&t!("core.fs.not_absolute")),
        }
    }
}

impl std::error::Error for PathError {}

/// 目录列举失败的原因；`Display` 文案直接回给前端。
#[derive(Debug)]
pub(crate) enum ListDirError {
    Path(PathError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    NotDirectory {
        path: PathBuf,
    },
}

impl From<PathError> for ListDirError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for ListDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::Io { path, source } => f.write_str(&t!(
                "core.fs.dir_io",
                path = path.display().to_string(),
                source = source
            )),
            Self::NotDirectory { path } => {
                f.write_str(&t!("core.fs.not_dir", path = path.display().to_string()))
            }
        }
    }
}

impl std::error::Error for ListDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::NotDirectory { .. } => None,
        }
    }
}

/// 新建目录失败的原因；`Display` 文案直接回给前端。
#[derive(Debug)]
pub(crate) enum CreateDirError {
    Path(PathError),
    InvalidName(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    NotDirectory {
        path: PathBuf,
    },
    Exists {
        path: PathBuf,
    },
}

impl From<PathError> for CreateDirError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for CreateDirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::InvalidName(name) => f.write_str(&t!("core.fs.invalid_dir_name", name = name)),
            Self::Io { path, source } => f.write_str(&t!(
                "core.fs.dir_io",
                path = path.display().to_string(),
                source = source
            )),
            Self::NotDirectory { path } => {
                f.write_str(&t!("core.fs.not_dir", path = path.display().to_string()))
            }
            Self::Exists { path } => {
                f.write_str(&t!("core.fs.dir_exists", path = path.display().to_string()))
            }
        }
    }
}

impl std::error::Error for CreateDirError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 新建文件失败的原因；与 [`CreateDirError`] 同构，文案指向文件。
#[derive(Debug)]
pub(crate) enum CreateFileError {
    Path(PathError),
    InvalidName(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    NotDirectory {
        path: PathBuf,
    },
    Exists {
        path: PathBuf,
    },
}

impl From<PathError> for CreateFileError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for CreateFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::InvalidName(name) => f.write_str(&t!("core.fs.invalid_file_name", name = name)),
            Self::Io { path, source } => f.write_str(&t!(
                "core.fs.dir_io",
                path = path.display().to_string(),
                source = source
            )),
            Self::NotDirectory { path } => {
                f.write_str(&t!("core.fs.not_dir", path = path.display().to_string()))
            }
            Self::Exists { path } => f.write_str(&t!(
                "core.fs.file_exists",
                path = path.display().to_string()
            )),
        }
    }
}

impl std::error::Error for CreateFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 文件读取/写入/重命名共用的失败原因；`Display` 文案直接回给前端。
#[derive(Debug)]
pub(crate) enum FileIoError {
    Path(PathError),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    NotFile {
        path: PathBuf,
    },
    Exists {
        path: PathBuf,
    },
}

impl From<PathError> for FileIoError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for FileIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::Io { path, source } => f.write_str(&t!(
                "core.fs.path_io",
                path = path.display().to_string(),
                source = source
            )),
            Self::NotFile { path } => {
                f.write_str(&t!("core.fs.not_file", path = path.display().to_string()))
            }
            Self::Exists { path } => f.write_str(&t!(
                "core.fs.path_exists",
                path = path.display().to_string()
            )),
        }
    }
}

impl std::error::Error for FileIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 覆盖写失败；`Conflict` 单独成变体，service 层映射 `-32016`。
#[derive(Debug)]
pub(crate) enum WriteFileError {
    Io(FileIoError),
    Conflict { modified_ms: Option<u64> },
}

impl From<FileIoError> for WriteFileError {
    fn from(error: FileIoError) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for WriteFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Conflict { .. } => f.write_str(&t!("core.fs.conflict")),
        }
    }
}

impl std::error::Error for WriteFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Conflict { .. } => None,
        }
    }
}

/// 移入回收站失败。
#[derive(Debug)]
pub(crate) enum DeleteError {
    Path(PathError),
    Trash { path: PathBuf, message: String },
}

impl From<PathError> for DeleteError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(error) => error.fmt(f),
            Self::Trash { path, message } => f.write_str(&t!(
                "core.fs.trash_failed",
                path = path.display().to_string(),
                message = message
            )),
        }
    }
}

impl std::error::Error for DeleteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Path(error) => Some(error),
            Self::Trash { .. } => None,
        }
    }
}

/// 列出 `path` 下的目录项；`None` 取 `default_cwd`。
/// `include_files` 为真时同时列出普通文件，目录在前、文件在后。
pub(crate) fn list_dir(
    path: Option<&str>,
    default_cwd: &Path,
    show_hidden: bool,
    include_files: bool,
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
        let symlink = item
            .file_type()
            .is_ok_and(|file_type| file_type.is_symlink());
        let entry_path = item.path();
        let Ok(metadata) = std::fs::metadata(&entry_path) else {
            continue;
        };
        let kind = if metadata.is_dir() {
            FsEntryKind::Dir
        } else if metadata.is_file() {
            FsEntryKind::File
        } else {
            continue;
        };
        if kind == FsEntryKind::File && !include_files {
            continue;
        }
        entries.push(FsDirEntry {
            git: kind == FsEntryKind::Dir && entry_path.join(".git").exists(),
            path: entry_path.to_string_lossy().into_owned(),
            name,
            kind,
            size: (kind == FsEntryKind::File).then_some(metadata.len()),
            modified_ms: modified_ms(&metadata),
            symlink,
        });
    }
    // 目录在前、文件在后，各自按名称不区分大小写排序。
    entries.sort_by(|left, right| {
        (left.kind != FsEntryKind::Dir)
            .cmp(&(right.kind != FsEntryKind::Dir))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
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
    let Some(name) = single_segment(name) else {
        return Err(CreateDirError::InvalidName(name.trim().to_owned()));
    };

    let parent = expand(parent)?;
    let canonical = std::fs::canonicalize(&parent).map_err(|source| CreateDirError::Io {
        path: parent.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(CreateDirError::NotDirectory { path: canonical });
    }

    let target = canonical.join(name);
    let lock_path = crate::hashline::apply::resolve_commit_lock_path(&target);
    let _commit_guards = crate::hashline::apply::lock_commit_paths_blocking(&lock_path, None);
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

/// 在 `parent` 下新建名为 `name` 的空文件，返回规范化后的绝对路径。
pub(crate) fn create_file(parent: &str, name: &str) -> Result<FsCreateFileResult, CreateFileError> {
    let Some(name) = single_segment(name) else {
        return Err(CreateFileError::InvalidName(name.trim().to_owned()));
    };

    let parent = expand(parent)?;
    let canonical = std::fs::canonicalize(&parent).map_err(|source| CreateFileError::Io {
        path: parent.clone(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(CreateFileError::NotDirectory { path: canonical });
    }

    let target = canonical.join(name);
    let _commit_guards = crate::hashline::apply::lock_commit_paths_blocking(&target, None);
    match std::fs::File::create_new(&target) {
        Ok(_) => Ok(FsCreateFileResult {
            path: target.to_string_lossy().into_owned(),
        }),
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateFileError::Exists { path: target })
        }
        Err(source) => Err(CreateFileError::Io {
            path: target,
            source,
        }),
    }
}

/// 二进制嗅探窗口：只看文件头 8 KiB 是否含 NUL。
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// `fs/read_file` 与 `resource/read` 共用的文件内容视图。
#[derive(Debug)]
pub(crate) struct FileContent {
    /// 规范化绝对路径。
    pub path: PathBuf,
    /// 文件实际字节数（不受截断影响）。
    pub size: u64,
    /// 修改时间（epoch 毫秒）。
    pub modified_ms: Option<u64>,
    /// 内容编码判定。
    pub binary: bool,
    /// UTF-8 正文；`binary` 时为 `None`。
    pub content: Option<String>,
    /// 文件超过读取上限，只读了头部（在字符边界截断）。
    pub truncated: bool,
}

/// 按字符串路径读取（展开 `~`）；`resource/read` 等已定位路径的调用方用 [`read_path`]。
pub(crate) fn read_file(raw: &str, max_bytes: Option<u64>) -> Result<FileContent, FileIoError> {
    read_path(&expand(raw)?, max_bytes)
}

/// 读取已定位的文件；`path` 不必预先规范化。
pub(crate) fn read_path(path: &Path, max_bytes: Option<u64>) -> Result<FileContent, FileIoError> {
    let metadata = std::fs::metadata(path).map_err(|source| FileIoError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(FileIoError::NotFile {
            path: path.to_owned(),
        });
    }
    let canonical = std::fs::canonicalize(path).map_err(|source| FileIoError::Io {
        path: path.to_owned(),
        source,
    })?;
    let limit = max_bytes
        .unwrap_or(FS_READ_FILE_DEFAULT_MAX_BYTES)
        .min(FS_READ_FILE_MAX_BYTES);
    let truncated = metadata.len() > limit;
    let mut buffer = Vec::new();
    std::fs::File::open(&canonical)
        .map_err(|source| FileIoError::Io {
            path: canonical.clone(),
            source,
        })?
        .take(limit)
        .read_to_end(&mut buffer)
        .map_err(|source| FileIoError::Io {
            path: canonical.clone(),
            source,
        })?;

    // 二进制判定：头部含 NUL 必二进制；否则要求读到的部分是合法 UTF-8。
    // 截断读时末尾半个多字节字符不算非法——它属于被截掉的尾部。
    let has_nul = buffer[..buffer.len().min(BINARY_SNIFF_BYTES)].contains(&0);
    let utf8_error = std::str::from_utf8(&buffer).err();
    let binary =
        has_nul || utf8_error.is_some_and(|error| !(truncated && error.error_len().is_none()));
    let content = (!binary).then(|| match utf8_error {
        None => String::from_utf8(buffer).expect("已验证为合法 UTF-8"),
        Some(error) => String::from_utf8(buffer[..error.valid_up_to()].to_owned())
            .expect("valid_up_to 前缀是合法 UTF-8"),
    });

    Ok(FileContent {
        path: canonical,
        size: metadata.len(),
        modified_ms: modified_ms(&metadata),
        binary,
        content,
        truncated,
    })
}

/// 覆盖写的结果：路径、新大小与新 mtime。
#[derive(Debug)]
pub(crate) struct FileWrite {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ms: Option<u64>,
}

/// 覆盖写入已存在的普通文件；给了 `expected_modified_ms` 且磁盘 mtime 不等时报冲突。
pub(crate) fn write_file(
    raw: &str,
    content: &str,
    expected_modified_ms: Option<u64>,
) -> Result<FileWrite, WriteFileError> {
    let target = expand(raw).map_err(FileIoError::Path)?;
    let lock_path = crate::hashline::apply::resolve_commit_lock_path(&target);
    let _commit_guards = crate::hashline::apply::lock_commit_paths_blocking(&lock_path, None);
    let metadata = std::fs::metadata(&target).map_err(|source| FileIoError::Io {
        path: target.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(FileIoError::NotFile { path: target }.into());
    }
    if let Some(expected) = expected_modified_ms {
        let current = modified_ms(&metadata);
        if current != Some(expected) {
            return Err(WriteFileError::Conflict {
                modified_ms: current,
            });
        }
    }
    std::fs::write(&target, content.as_bytes()).map_err(|source| FileIoError::Io {
        path: target.clone(),
        source,
    })?;
    let metadata = std::fs::metadata(&target).map_err(|source| FileIoError::Io {
        path: target.clone(),
        source,
    })?;
    let canonical = std::fs::canonicalize(&target).map_err(|source| FileIoError::Io {
        path: target.clone(),
        source,
    })?;
    Ok(FileWrite {
        path: canonical,
        size: metadata.len(),
        modified_ms: modified_ms(&metadata),
    })
}

/// 重命名/移动 `from` 到 `to`；目标已存在时报错，仅改大小写除外。
pub(crate) fn rename(from: &str, to: &str) -> Result<FsRenameResult, FileIoError> {
    let from = expand(from)?;
    let to = expand(to)?;
    let from_lock_path = crate::hashline::apply::resolve_commit_lock_path(&from);
    let to_lock_path = crate::hashline::apply::resolve_commit_lock_path(&to);
    let _commit_guards =
        crate::hashline::apply::lock_commit_paths_blocking(&from_lock_path, Some(&to_lock_path));
    if to.symlink_metadata().is_ok() && !is_case_only_rename(&from, &to) {
        return Err(FileIoError::Exists { path: to });
    }
    std::fs::rename(&from, &to).map_err(|source| FileIoError::Io {
        path: from.clone(),
        source,
    })?;
    let canonical = std::fs::canonicalize(&to).map_err(|source| FileIoError::Io {
        path: to.clone(),
        source,
    })?;
    Ok(FsRenameResult {
        path: canonical.to_string_lossy().into_owned(),
    })
}

/// `to` 是否只是 `from` 改了大小写，且大小写不敏感卷把 `to` 解析回了 `from` 本身。
///
/// 要求同一目录下名称仅大小写不同、目录里没有与 `to` 逐字节同名的项，且 `to` 解析到的
/// 正是 `from` 本身；否则卷可能按 Unicode 规范化把 `to` 解析到了另一项，不能放行。
fn is_case_only_rename(from: &Path, to: &Path) -> bool {
    let (Some(from_parent), Some(to_parent), Some(from_name), Some(to_name)) =
        (from.parent(), to.parent(), from.file_name(), to.file_name())
    else {
        return false;
    };
    if from_name == to_name
        || from_name.to_string_lossy().to_lowercase() != to_name.to_string_lossy().to_lowercase()
    {
        return false;
    }
    let same_parent = match (
        std::fs::canonicalize(from_parent),
        std::fs::canonicalize(to_parent),
    ) {
        (Ok(from_parent), Ok(to_parent)) => from_parent == to_parent,
        _ => false,
    };
    same_parent
        && is_same_entry(from, to)
        && std::fs::read_dir(from_parent)
            .is_ok_and(|entries| entries.flatten().all(|entry| entry.file_name() != to_name))
}

#[cfg(unix)]
fn is_same_entry(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// NTFS 不做 Unicode 规范化，大小写折叠命中的只会是 `from`，名称检查已足够。
#[cfg(not(unix))]
fn is_same_entry(a: &Path, _b: &Path) -> bool {
    a.symlink_metadata().is_ok()
}

/// 把文件或目录整体移入系统回收站。
pub(crate) fn delete(raw: &str) -> Result<(), DeleteError> {
    let target = expand(raw)?;
    let lock_path = crate::hashline::apply::resolve_commit_lock_path(&target);
    let _commit_guards = crate::hashline::apply::lock_commit_paths_blocking(&lock_path, None);
    trash::delete(&target).map_err(|error| DeleteError::Trash {
        path: target.clone(),
        message: error.to_string(),
    })
}

/// 目录项名单段校验：trim 后必须只剩一个 `Normal` 段（不含分隔符与 `..`）。
fn single_segment(name: &str) -> Option<&str> {
    let name = name.trim();
    let mut components = Path::new(name).components();
    (matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none())
        .then_some(name)
}

/// 元数据 mtime → epoch 毫秒；取不到时为 `None`。
fn modified_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    let time = metadata.modified().ok()?;
    let millis = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    u64::try_from(millis).ok()
}

/// 用户输入 → 绝对路径：`~` 展开到主目录，相对路径一律拒绝。
pub fn expand(raw: &str) -> Result<PathBuf, PathError> {
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
        let result = list_dir(root.path().to_str(), Path::new("/"), false, false).unwrap();
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
        let result = list_dir(root.path().to_str(), Path::new("/"), true, false).unwrap();
        let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, [".hidden", "A", "b"]);
    }

    #[test]
    fn none_uses_default_cwd() {
        let root = fixture();
        let result = list_dir(None, root.path(), false, false).unwrap();
        assert_eq!(result.entries.len(), 2);
    }

    #[test]
    fn rejects_missing_relative_and_file_paths() {
        let root = fixture();
        let missing = root.path().join("nope");
        let error = list_dir(missing.to_str(), Path::new("/"), false, false).unwrap_err();
        assert!(matches!(error, ListDirError::Io { .. }));
        assert!(error.to_string().starts_with("目录无效："), "{error}");
        let error = list_dir(Some("relative/dir"), Path::new("/"), false, false).unwrap_err();
        assert!(matches!(error, ListDirError::Path(PathError::NotAbsolute)));
        assert_eq!(error.to_string(), "需要绝对路径");
        let file = root.path().join("file.txt");
        assert!(matches!(
            list_dir(file.to_str(), Path::new("/"), false, false).unwrap_err(),
            ListDirError::NotDirectory { .. }
        ));
    }

    #[test]
    fn include_files_lists_files_after_directories() {
        let root = fixture();
        std::fs::write(root.path().join("a.txt"), "x").unwrap();
        let result = list_dir(root.path().to_str(), Path::new("/"), false, true).unwrap();
        let names: Vec<_> = result
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind))
            .collect();
        assert_eq!(
            names,
            [
                ("A", FsEntryKind::Dir),
                ("b", FsEntryKind::Dir),
                ("a.txt", FsEntryKind::File),
                ("file.txt", FsEntryKind::File),
            ]
        );
        let file = &result.entries[2];
        assert_eq!(file.size, Some(1));
        assert!(file.modified_ms.is_some());
        assert!(!file.symlink);
        assert!(
            result
                .entries
                .iter()
                .all(|entry| entry.kind == FsEntryKind::Dir || entry.size.is_some()),
            "目录不带 size，文件带 size"
        );
    }

    #[test]
    fn creates_one_level_directory_under_parent() {
        let root = fixture();
        let result = create_dir(&root.path().to_string_lossy(), " 新项目 ").unwrap();
        let canonical = std::fs::canonicalize(root.path()).unwrap();
        assert_eq!(result.path, canonical.join("新项目").to_string_lossy());
        assert!(canonical.join("新项目").is_dir());
        let listed = list_dir(root.path().to_str(), Path::new("/"), false, false).unwrap();
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

    #[test]
    fn read_file_returns_utf8_content_or_marks_binary() {
        let root = fixture();
        let text = root.path().join("note.txt");
        std::fs::write(&text, "你好 moc").unwrap();
        let read = read_file(text.to_str().unwrap(), None).unwrap();
        assert!(!read.binary);
        assert_eq!(read.content.as_deref(), Some("你好 moc"));
        assert_eq!(read.size, "你好 moc".len() as u64);
        assert_eq!(read.path, std::fs::canonicalize(&text).unwrap());

        let binary = root.path().join("blob.bin");
        std::fs::write(&binary, b"abc\0def").unwrap();
        let read = read_file(binary.to_str().unwrap(), None).unwrap();
        assert!(read.binary);
        assert_eq!(read.content, None);
        // 非 NUL 的非法 UTF-8 同样是二进制。
        let invalid = root.path().join("invalid.bin");
        std::fs::write(&invalid, b"\xff\xfe").unwrap();
        assert!(read_file(invalid.to_str().unwrap(), None).unwrap().binary);
        // 目录与缺失路径按无效路径报。
        assert!(matches!(
            read_file(root.path().to_str().unwrap(), None).unwrap_err(),
            FileIoError::NotFile { .. }
        ));
        assert!(matches!(
            read_file("relative/x", None).unwrap_err(),
            FileIoError::Path(PathError::NotAbsolute)
        ));
    }

    #[test]
    fn read_file_truncates_at_char_boundary() {
        let root = fixture();
        // "你" 是 3 字节：max_bytes=4 会切在多字节字符中间。
        let text = root.path().join("cjk.txt");
        std::fs::write(&text, "你好世界").unwrap();
        let read = read_file(text.to_str().unwrap(), Some(4)).unwrap();
        assert!(!read.binary);
        assert!(read.truncated);
        assert_eq!(read.size, "你好世界".len() as u64);
        assert_eq!(read.content.as_deref(), Some("你"));

        // 上限内的截断仍是 utf8 且补齐 char 边界；二进制文件截断也标记 binary。
        let binary = root.path().join("head.bin");
        std::fs::write(&binary, b"\0abc").unwrap();
        let read = read_file(binary.to_str().unwrap(), Some(2)).unwrap();
        assert!(read.binary);
        assert!(read.truncated);
    }

    #[test]
    fn write_file_overwrites_and_reports_mtime_conflict() {
        let root = fixture();
        let target = root.path().join("file.txt");
        let first = write_file(target.to_str().unwrap(), "hello", None).unwrap();
        assert_eq!(first.size, 5);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        let current = first.modified_ms.expect("mtime 可读");
        let conflict =
            write_file(target.to_str().unwrap(), "changed", Some(current + 1)).unwrap_err();
        assert!(
            matches!(conflict, WriteFileError::Conflict { .. }),
            "{conflict}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        let second = write_file(target.to_str().unwrap(), "changed", Some(current)).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed");
        assert_eq!(second.size, 7);

        let missing = root.path().join("nope.txt");
        assert!(matches!(
            write_file(missing.to_str().unwrap(), "x", None).unwrap_err(),
            WriteFileError::Io(FileIoError::Io { .. })
        ));
        assert!(matches!(
            write_file(root.path().to_str().unwrap(), "x", None).unwrap_err(),
            WriteFileError::Io(FileIoError::NotFile { .. })
        ));
    }

    #[test]
    fn create_file_validates_name_and_existing() {
        let root = fixture();
        let parent = root.path().to_string_lossy().into_owned();
        let created = create_file(&parent, " new.md ").unwrap();
        assert!(std::path::Path::new(&created.path).is_file());
        for name in ["", ".", "..", "a/b", "../escape"] {
            assert!(
                matches!(
                    create_file(&parent, name).unwrap_err(),
                    CreateFileError::InvalidName(_)
                ),
                "{name:?} 应被拒绝"
            );
        }
        assert!(matches!(
            create_file(&parent, "file.txt").unwrap_err(),
            CreateFileError::Exists { .. }
        ));
        // 目录重名也算已存在。
        assert!(matches!(
            create_file(&parent, "b").unwrap_err(),
            CreateFileError::Exists { .. }
        ));
    }

    #[test]
    fn rename_moves_and_rejects_existing_target() {
        let root = fixture();
        let from = root.path().join("file.txt");
        let to = root.path().join("b").join("moved.txt");
        let renamed = rename(from.to_str().unwrap(), to.to_str().unwrap()).unwrap();
        assert_eq!(
            renamed.path,
            std::fs::canonicalize(&to).unwrap().to_string_lossy()
        );
        assert!(!from.exists());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "x");

        let occupied = root.path().join("b").join("moved.txt");
        let other = root.path().join("b").join("other.txt");
        std::fs::write(&other, "y").unwrap();
        assert!(matches!(
            rename(occupied.to_str().unwrap(), other.to_str().unwrap()).unwrap_err(),
            FileIoError::Exists { .. }
        ));
        assert!(matches!(
            rename("relative/x", "/tmp/y").unwrap_err(),
            FileIoError::Path(PathError::NotAbsolute)
        ));
    }

    #[test]
    fn rename_allows_case_only_change() {
        let root = fixture();
        let from = root.path().join("file.txt");
        let to = root.path().join("File.txt");
        let renamed = rename(from.to_str().unwrap(), to.to_str().unwrap()).unwrap();
        assert!(renamed.path.ends_with("File.txt"), "{}", renamed.path);
        let names: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.eq_ignore_ascii_case("file.txt"))
            .collect();
        assert_eq!(names, ["File.txt"]);

        // 大小写不敏感卷上，折叠后命中另一项的目标仍算已存在，不能覆盖。
        std::fs::write(root.path().join("Other.txt"), "y").unwrap();
        let other = root.path().join("other.txt");
        if other.exists() {
            assert!(matches!(
                rename(to.to_str().unwrap(), other.to_str().unwrap()).unwrap_err(),
                FileIoError::Exists { .. }
            ));
        }

        // 区分大小写但不区分 Unicode 规范化的卷（如大小写敏感 APFS）上，
        // `É`（NFC）会解析到另一个文件 `E\u{301}`（NFD），不能借仅改大小写的放行覆盖它。
        let lower = root.path().join("\u{e9}.md");
        let decomposed = root.path().join("E\u{301}.md");
        std::fs::write(&lower, "lower").unwrap();
        std::fs::write(&decomposed, "decomposed").unwrap();
        if std::fs::read_to_string(&lower).unwrap() == "lower" {
            let upper = root.path().join("\u{c9}.md");
            let _ = rename(lower.to_str().unwrap(), upper.to_str().unwrap());
            assert_eq!(std::fs::read_to_string(&decomposed).unwrap(), "decomposed");
        }
    }
}
