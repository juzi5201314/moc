//! `fs/list_dir` / `fs/create_dir`：让前端浏览目录、新建子目录来选工作目录。
//!
//! 只列目录，不列文件；路径必须是绝对路径或以 `~` 开头。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// 单次返回的目录项上限；超出时置 [`FsListDirResult::truncated`]。
pub const FS_LIST_DIR_MAX_ENTRIES: usize = 1000;

/// `fs/list_dir`：列出目录下的子目录。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsListDir;

impl Method for FsListDir {
    const NAME: &'static str = "fs/list_dir";
    type Params = FsListDirParams;
    type Result = FsListDirResult;
}

impl ClientRequest for FsListDir {}

/// `fs/list_dir` 的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsListDirParams {
    /// 绝对路径或以 `~` 开头；`None` 取 core 的当前目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 是否包含以 `.` 开头的目录。
    #[serde(default)]
    pub show_hidden: bool,
}

/// `fs/list_dir` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 子目录，按名称（不区分大小写）排序。
    pub entries: Vec<FsDirEntry>,
    /// 超过 [`FS_LIST_DIR_MAX_ENTRIES`] 被截断。
    pub truncated: bool,
}

/// 一个子目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsDirEntry {
    /// 目录名。
    pub name: String,
    /// 绝对路径。
    pub path: String,
    /// 目录下有 `.git`。
    pub git: bool,
}

/// `fs/create_dir`：在指定目录下新建一个子目录。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FsCreateDir;

impl Method for FsCreateDir {
    const NAME: &'static str = "fs/create_dir";
    type Params = FsCreateDirParams;
    type Result = FsCreateDirResult;
}

impl ClientRequest for FsCreateDir {}

/// `fs/create_dir` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsCreateDirParams {
    /// 父目录：绝对路径或以 `~` 开头，必须已存在。
    pub parent: String,
    /// 新目录名：单段名字，不接受路径分隔符与 `..`。
    pub name: String,
}

/// `fs/create_dir` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsCreateDirResult {
    /// 新目录的绝对路径。
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_name_and_wire_shape_are_stable() {
        assert_eq!(<FsListDir as Method>::NAME, "fs/list_dir");
        let params = serde_json::to_value(FsListDirParams::default()).expect("参数可序列化");
        assert_eq!(params, serde_json::json!({"show_hidden": false}));
        let parsed: FsListDirParams =
            serde_json::from_value(serde_json::json!({"path": "~/x"})).expect("可反序列化");
        assert_eq!(parsed.path.as_deref(), Some("~/x"));
        assert!(!parsed.show_hidden);
        assert!(
            serde_json::from_value::<FsListDirParams>(serde_json::json!({"extra": true})).is_err()
        );
        let result = serde_json::to_value(FsListDirResult {
            path: "/a".into(),
            parent: None,
            home: None,
            entries: vec![FsDirEntry {
                name: "b".into(),
                path: "/a/b".into(),
                git: true,
            }],
            truncated: false,
        })
        .expect("结果可序列化");
        assert_eq!(
            result,
            serde_json::json!({
                "path": "/a",
                "entries": [{"name": "b", "path": "/a/b", "git": true}],
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
}
