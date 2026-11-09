//! `resource/*`：会话虚拟空间的输入补全、结构化列举与读取。
//!
//! `resource/complete` 服务输入框补全；`resource/list` / `resource/read`
//! 服务 WebUI 文件管理器的虚拟空间分页，URI 命名空间与 core 的
//! `ResourceRouter` 一致（`local://` / `artifact://` / `history://` / `skill://`）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};
use crate::methods::fs::{FsEntryKind, FsFileEncoding};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceComplete;

impl Method for ResourceComplete {
    const NAME: &'static str = "resource/complete";
    type Params = ResourceCompleteParams;
    type Result = ResourceCompleteResult;
    crate::method::json_codec!();
}

impl ClientRequest for ResourceComplete {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceCompleteParams {
    pub session_id: String,
    pub prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceCandidate {
    pub uri: String,
    pub label: String,
    pub description: String,
    pub kind: String,
    pub append_space: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceCompleteResult {
    pub candidates: Vec<ResourceCandidate>,
}

/// `resource/list`：结构化列举会话虚拟空间的一层。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceList;

impl Method for ResourceList {
    const NAME: &'static str = "resource/list";
    type Params = ResourceListParams;
    type Result = ResourceListResult;
    crate::method::json_codec!();
}

impl ClientRequest for ResourceList {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceListParams {
    pub session_id: String,
    /// 要列举的 URI；`""` 是根（列出各命名空间入口）。
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceListEntry {
    /// 子项 URI：`kind == "dir"` 可继续 `resource/list`，`"file"` 可 `resource/read`。
    pub uri: String,
    /// 展示名。
    pub name: String,
    pub kind: FsEntryKind,
    /// 字节数；仅文件有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 修改时间（epoch 毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
    /// 补充说明（如 skill 的 description）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceListResult {
    /// 回显请求的 URI。
    pub uri: String,
    pub entries: Vec<ResourceListEntry>,
    pub truncated: bool,
}

/// `resource/read`：按 URI 读取资源内容；命名空间不可读，用 `resource/list`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceRead;

impl Method for ResourceRead {
    const NAME: &'static str = "resource/read";
    type Params = ResourceReadParams;
    type Result = ResourceReadResult;
    crate::method::json_codec!();
}

impl ClientRequest for ResourceRead {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceReadParams {
    pub session_id: String,
    pub uri: String,
    /// 最多读取的字节数；缺省与上限同 `fs/read_file`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceReadResult {
    /// 回显请求的 URI。
    pub uri: String,
    /// 资源实际字节数（不受 `max_bytes` 截断影响）。
    pub size: u64,
    /// 修改时间（epoch 毫秒）；取不到时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
    /// 内容编码判定。
    pub encoding: FsFileEncoding,
    /// UTF-8 正文；`encoding == "binary"` 时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 资源超过 `max_bytes`，只回了头部（在字符边界截断）。
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;

    #[test]
    fn method_name_and_wire_shape_are_stable() {
        assert_eq!(<ResourceComplete as Method>::NAME, "resource/complete");
        let params = serde_json::to_value(ResourceCompleteParams {
            session_id: "ses_1".into(),
            prefix: "local:/".into(),
        })
        .expect("参数可序列化");
        assert_eq!(
            params,
            serde_json::json!({"session_id":"ses_1","prefix":"local:/"})
        );
        assert!(
            serde_json::from_value::<ResourceCompleteParams>(
                serde_json::json!({"session_id":"ses_1","prefix":"local:/","extra":true})
            )
            .is_err()
        );
    }

    #[test]
    fn list_wire_shape_is_stable() {
        assert_eq!(<ResourceList as Method>::NAME, "resource/list");
        assert_eq!(
            serde_json::to_value(ResourceListParams {
                session_id: "ses_1".into(),
                uri: String::new(),
            })
            .expect("参数可序列化"),
            serde_json::json!({"session_id": "ses_1", "uri": ""})
        );
        let result = serde_json::to_value(ResourceListResult {
            uri: "skill://demo".into(),
            entries: vec![ResourceListEntry {
                uri: "skill://demo/SKILL.md".into(),
                name: "SKILL.md".into(),
                kind: FsEntryKind::File,
                size: Some(4),
                modified_ms: None,
                description: None,
            }],
            truncated: false,
        })
        .expect("结果可序列化");
        assert_eq!(
            result,
            serde_json::json!({
                "uri": "skill://demo",
                "entries": [{"uri": "skill://demo/SKILL.md", "name": "SKILL.md", "kind": "file", "size": 4}],
                "truncated": false
            })
        );
    }

    #[test]
    fn read_wire_shape_is_stable() {
        assert_eq!(<ResourceRead as Method>::NAME, "resource/read");
        assert_eq!(
            serde_json::to_value(ResourceReadParams {
                session_id: "ses_1".into(),
                uri: "local://p.md".into(),
                max_bytes: None,
            })
            .expect("参数可序列化"),
            serde_json::json!({"session_id": "ses_1", "uri": "local://p.md"})
        );
        assert!(
            serde_json::from_value::<ResourceReadParams>(
                serde_json::json!({"session_id": "ses_1"})
            )
            .is_err(),
            "缺少 uri 必须拒绝"
        );
        assert_eq!(
            serde_json::to_value(ResourceReadResult {
                uri: "local://p.md".into(),
                size: 7,
                modified_ms: Some(1),
                encoding: FsFileEncoding::Utf8,
                content: Some("# plan".into()),
                truncated: false,
            })
            .expect("结果可序列化"),
            serde_json::json!({
                "uri": "local://p.md",
                "size": 7,
                "modified_ms": 1,
                "encoding": "utf8",
                "content": "# plan",
                "truncated": false
            })
        );
    }
}
