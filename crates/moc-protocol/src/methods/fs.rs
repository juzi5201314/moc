//! `fs/*`：目录选择器与文件管理器的文件系统服务面。
//!
//! `fs/list_dir` 默认只列子目录，`include_files` 时附带文件；`fs/read_file` /
//! `fs/write_file` / `fs/create_file` / `fs/rename` / `fs/delete` 供文件管理器
//! 与编辑器使用。路径必须是绝对路径或以 `~` 开头。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// 单次返回的目录项上限；超出时置 [`FsListDirResult::truncated`]。
pub const FS_LIST_DIR_MAX_ENTRIES: usize = 1000;

/// `fs/read_file` / `resource/read` 未给 `max_bytes` 时的默认读取上限。
pub const FS_READ_FILE_DEFAULT_MAX_BYTES: u64 = 1024 * 1024;
/// `fs/read_file` / `resource/read` 的 `max_bytes` 取值上限。
pub const FS_READ_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// `fs/list_dir`：列出目录下的子目录。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsListDir;

impl Method for FsListDir {
    const NAME: &'static str = "fs/list_dir";
    type Params = FsListDirParams;
    type Result = FsListDirResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsListDir {}

/// `fs/list_dir` 的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsListDirParams {
    /// 绝对路径或以 `~` 开头；`None` 取 core 的当前目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 是否包含以 `.` 开头的目录。
    #[serde(default)]
    pub show_hidden: bool,
    /// 是否同时列出文件（目录在前、文件在后）。
    #[serde(default)]
    pub include_files: bool,
}

/// `fs/list_dir` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsListDirResult {
    /// 规范化后的绝对路径。
    pub path: String,
    /// 上级目录；已经是根时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// 用户主目录；取不到时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// 目录项：目录在前、文件在后，各自按名称（不区分大小写）排序。
    pub entries: Vec<FsDirEntry>,
    /// 超过 [`FS_LIST_DIR_MAX_ENTRIES`] 被截断。
    pub truncated: bool,
}

/// 一个目录项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsDirEntry {
    /// 条目名。
    pub name: String,
    /// 绝对路径。
    pub path: String,
    /// 目录下有 `.git`。
    pub git: bool,
    /// 条目类型。
    pub kind: FsEntryKind,
    /// 字节数；仅文件有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 修改时间（epoch 毫秒）；取不到时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
    /// 本身是符号链接。
    #[serde(default, skip_serializing_if = "is_false")]
    pub symlink: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// 目录项类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    Dir,
    File,
}

/// 文件内容编码判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsFileEncoding {
    Utf8,
    Binary,
}

/// `fs/create_dir`：在指定目录下新建一个子目录。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsCreateDir;

impl Method for FsCreateDir {
    const NAME: &'static str = "fs/create_dir";
    type Params = FsCreateDirParams;
    type Result = FsCreateDirResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsCreateDir {}

/// `fs/create_dir` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsCreateDirParams {
    /// 父目录：绝对路径或以 `~` 开头，必须已存在。
    pub parent: String,
    /// 新目录名：单段名字，不接受路径分隔符与 `..`。
    pub name: String,
}

/// `fs/create_dir` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsCreateDirResult {
    /// 新目录的绝对路径。
    pub path: String,
}

/// `fs/read_file`：读取文件内容，文本直接回、二进制只回元信息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsReadFile;

impl Method for FsReadFile {
    const NAME: &'static str = "fs/read_file";
    type Params = FsReadFileParams;
    type Result = FsReadFileResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsReadFile {}

/// `fs/read_file` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsReadFileParams {
    /// 绝对路径或以 `~` 开头。
    pub path: String,
    /// 最多读取的字节数；缺省 [`FS_READ_FILE_DEFAULT_MAX_BYTES`]，
    /// 上限 [`FS_READ_FILE_MAX_BYTES`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

/// `fs/read_file` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsReadFileResult {
    /// 规范化后的绝对路径。
    pub path: String,
    /// 文件实际字节数（不受 `max_bytes` 截断影响）。
    pub size: u64,
    /// 修改时间（epoch 毫秒）；取不到时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
    /// 内容编码判定。
    pub encoding: FsFileEncoding,
    /// UTF-8 正文；`encoding == "binary"` 时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 文件超过 `max_bytes`，只回了头部（在字符边界截断）。
    pub truncated: bool,
}

/// `fs/write_file`：整体覆盖已存在的普通文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsWriteFile;

impl Method for FsWriteFile {
    const NAME: &'static str = "fs/write_file";
    type Params = FsWriteFileParams;
    type Result = FsWriteFileResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsWriteFile {}

/// `fs/write_file` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsWriteFileParams {
    /// 绝对路径或以 `~` 开头；必须是已存在的普通文件。
    pub path: String,
    /// 新内容（UTF-8）。
    pub content: String,
    /// 乐观锁：给定时要求磁盘当前 mtime 相等，否则回 `-32016`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_modified_ms: Option<u64>,
}

/// `fs/write_file` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsWriteFileResult {
    /// 规范化后的绝对路径。
    pub path: String,
    /// 写入后的字节数。
    pub size: u64,
    /// 写入后的修改时间（epoch 毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
}

/// `fs/create_file`：在指定目录下新建空文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsCreateFile;

impl Method for FsCreateFile {
    const NAME: &'static str = "fs/create_file";
    type Params = FsCreateFileParams;
    type Result = FsCreateFileResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsCreateFile {}

/// `fs/create_file` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsCreateFileParams {
    /// 父目录：绝对路径或以 `~` 开头，必须已存在。
    pub parent: String,
    /// 新文件名：单段名字，不接受路径分隔符与 `..`。
    pub name: String,
}

/// `fs/create_file` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsCreateFileResult {
    /// 新文件的绝对路径。
    pub path: String,
}

/// `fs/rename`：重命名或移动；`to` 必须是尚不存在的绝对路径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsRename;

impl Method for FsRename {
    const NAME: &'static str = "fs/rename";
    type Params = FsRenameParams;
    type Result = FsRenameResult;
    crate::method::json_codec!();
}

impl ClientRequest for FsRename {}

/// `fs/rename` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsRenameParams {
    /// 源路径：绝对路径或以 `~` 开头。
    pub from: String,
    /// 目标路径：绝对路径或以 `~` 开头；已存在时报错。
    pub to: String,
}

/// `fs/rename` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsRenameResult {
    /// 目标的规范化绝对路径。
    pub path: String,
}

/// `fs/delete`：把文件或目录整体移入系统回收站。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsDelete;

impl Method for FsDelete {
    const NAME: &'static str = "fs/delete";
    type Params = FsDeleteParams;
    type Result = crate::methods::Empty;
    crate::method::json_codec!();
}

impl ClientRequest for FsDelete {}

/// `fs/delete` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsDeleteParams {
    /// 绝对路径或以 `~` 开头。
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_name_and_wire_shape_are_stable() {
        assert_eq!(<FsListDir as Method>::NAME, "fs/list_dir");
        let params = serde_json::to_value(FsListDirParams::default()).expect("参数可序列化");
        assert_eq!(
            params,
            serde_json::json!({"show_hidden": false, "include_files": false})
        );
        let parsed: FsListDirParams =
            serde_json::from_value(serde_json::json!({"path": "~/x"})).expect("可反序列化");
        assert_eq!(parsed.path.as_deref(), Some("~/x"));
        assert!(!parsed.show_hidden);
        assert!(!parsed.include_files);
        assert!(
            serde_json::from_value::<FsListDirParams>(serde_json::json!({"extra": true})).is_err()
        );
        let result = serde_json::to_value(FsListDirResult {
            path: "/a".into(),
            parent: None,
            home: None,
            entries: vec![
                FsDirEntry {
                    name: "b".into(),
                    path: "/a/b".into(),
                    git: true,
                    kind: FsEntryKind::Dir,
                    size: None,
                    modified_ms: Some(7),
                    symlink: false,
                },
                FsDirEntry {
                    name: "c.txt".into(),
                    path: "/a/c.txt".into(),
                    git: false,
                    kind: FsEntryKind::File,
                    size: Some(3),
                    modified_ms: None,
                    symlink: true,
                },
            ],
            truncated: false,
        })
        .expect("结果可序列化");
        assert_eq!(
            result,
            serde_json::json!({
                "path": "/a",
                "entries": [
                    {"name": "b", "path": "/a/b", "git": true, "kind": "dir", "modified_ms": 7},
                    {"name": "c.txt", "path": "/a/c.txt", "git": false, "kind": "file", "size": 3, "symlink": true}
                ],
                "truncated": false
            })
        );
    }

    #[test]
    fn create_dir_wire_shape_is_stable() {
        assert_eq!(<FsCreateDir as Method>::NAME, "fs/create_dir");
        let params = serde_json::to_value(FsCreateDirParams {
            parent: "/a".into(),
            name: "b".into(),
        })
        .expect("参数可序列化");
        assert_eq!(params, serde_json::json!({"parent": "/a", "name": "b"}));
        assert!(
            serde_json::from_value::<FsCreateDirParams>(serde_json::json!({"parent": "/a"}))
                .is_err(),
            "缺少 name 必须拒绝"
        );
        assert!(
            serde_json::from_value::<FsCreateDirParams>(
                serde_json::json!({"parent": "/a", "name": "b", "extra": 1})
            )
            .is_err(),
            "未知字段必须拒绝"
        );
        let result = serde_json::to_value(FsCreateDirResult {
            path: "/a/b".into(),
        })
        .expect("结果可序列化");
        assert_eq!(result, serde_json::json!({"path": "/a/b"}));
    }

    #[test]
    fn read_file_wire_shape_is_stable() {
        assert_eq!(<FsReadFile as Method>::NAME, "fs/read_file");
        let params = serde_json::to_value(FsReadFileParams {
            path: "/a".into(),
            max_bytes: None,
        })
        .expect("参数可序列化");
        assert_eq!(params, serde_json::json!({"path": "/a"}));
        assert!(
            serde_json::from_value::<FsReadFileParams>(serde_json::json!({})).is_err(),
            "缺少 path 必须拒绝"
        );
        let result = serde_json::to_value(FsReadFileResult {
            path: "/a".into(),
            size: 5,
            modified_ms: Some(1),
            encoding: FsFileEncoding::Utf8,
            content: Some("hello".into()),
            truncated: true,
        })
        .expect("结果可序列化");
        assert_eq!(
            result,
            serde_json::json!({
                "path": "/a",
                "size": 5,
                "modified_ms": 1,
                "encoding": "utf8",
                "content": "hello",
                "truncated": true
            })
        );
        let binary = serde_json::to_value(FsReadFileResult {
            path: "/a".into(),
            size: 5,
            modified_ms: None,
            encoding: FsFileEncoding::Binary,
            content: None,
            truncated: false,
        })
        .expect("结果可序列化");
        assert_eq!(
            binary,
            serde_json::json!({"path": "/a", "size": 5, "encoding": "binary", "truncated": false})
        );
    }

    #[test]
    fn write_file_wire_shape_is_stable() {
        assert_eq!(<FsWriteFile as Method>::NAME, "fs/write_file");
        let params = serde_json::to_value(FsWriteFileParams {
            path: "/a".into(),
            content: "x".into(),
            expected_modified_ms: None,
        })
        .expect("参数可序列化");
        assert_eq!(params, serde_json::json!({"path": "/a", "content": "x"}));
        assert!(
            serde_json::from_value::<FsWriteFileParams>(
                serde_json::json!({"path": "/a", "content": "x", "extra": 1})
            )
            .is_err()
        );
        let result = serde_json::to_value(FsWriteFileResult {
            path: "/a".into(),
            size: 1,
            modified_ms: Some(2),
        })
        .expect("结果可序列化");
        assert_eq!(
            result,
            serde_json::json!({"path": "/a", "size": 1, "modified_ms": 2})
        );
    }

    #[test]
    fn create_rename_delete_wire_shapes_are_stable() {
        assert_eq!(<FsCreateFile as Method>::NAME, "fs/create_file");
        assert_eq!(
            serde_json::to_value(FsCreateFileParams {
                parent: "/a".into(),
                name: "b".into(),
            })
            .expect("参数可序列化"),
            serde_json::json!({"parent": "/a", "name": "b"})
        );
        assert_eq!(
            serde_json::to_value(FsCreateFileResult {
                path: "/a/b".into(),
            })
            .expect("结果可序列化"),
            serde_json::json!({"path": "/a/b"})
        );

        assert_eq!(<FsRename as Method>::NAME, "fs/rename");
        assert_eq!(
            serde_json::to_value(FsRenameParams {
                from: "/a".into(),
                to: "/b".into(),
            })
            .expect("参数可序列化"),
            serde_json::json!({"from": "/a", "to": "/b"})
        );
        assert_eq!(
            serde_json::to_value(FsRenameResult { path: "/b".into() }).expect("结果可序列化"),
            serde_json::json!({"path": "/b"})
        );

        assert_eq!(<FsDelete as Method>::NAME, "fs/delete");
        assert_eq!(
            serde_json::to_value(FsDeleteParams { path: "/a".into() }).expect("参数可序列化"),
            serde_json::json!({"path": "/a"})
        );
        assert_eq!(
            serde_json::to_value(<FsDelete as Method>::Result::default()).expect("结果可序列化"),
            serde_json::json!({})
        );
    }
}
