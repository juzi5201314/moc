//! 用户侧会话信息与 Markdown 导出；正文由 core 生成，不加入模型历史。

use crate::method::{ClientRequest, Method};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `/debuginfo`：工具使用统计和上下文压缩次数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDebugInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionDebugInfoParams {
    pub session_id: String,
}

impl Method for SessionDebugInfo {
    const NAME: &'static str = "session/debug_info";
    type Params = SessionDebugInfoParams;
    type Result = String;
    crate::method::json_codec!();
}
impl ClientRequest for SessionDebugInfo {}

/// `/session`：当前会话身份、文件与分类存储大小。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionInspect;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectParams {
    pub session_id: String,
}

impl Method for SessionInspect {
    const NAME: &'static str = "session/inspect";
    type Params = SessionInspectParams;
    type Result = String;
    crate::method::json_codec!();
}
impl ClientRequest for SessionInspect {}

/// `/export`：将会话历史脱敏后导出为 Markdown 文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionExport;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionExportParams {
    pub session_id: String,
    /// 绝对路径或相对于会话工作区的路径；省略时自动命名，已有文件不覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionExportResult {
    pub path: String,
    /// 正文的 JSON 编码超过 8 MiB 时省略，文件仍完整保存。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

impl Method for SessionExport {
    const NAME: &'static str = "session/export";
    type Params = SessionExportParams;
    type Result = SessionExportResult;
    crate::method::json_codec!();
}
impl ClientRequest for SessionExport {}

#[cfg(test)]
mod tests {
    use super::SessionExportResult;

    #[test]
    fn export_content_is_optional_but_empty_content_is_preserved() {
        let result: SessionExportResult = serde_json::from_str(r#"{"path":"export.md"}"#).unwrap();
        assert!(result.content.is_none());
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({"path":"export.md"})
        );
        let result: SessionExportResult =
            serde_json::from_str(r#"{"path":"export.md","content":""}"#).unwrap();
        assert_eq!(result.content.as_deref(), Some(""));
        assert_eq!(serde_json::to_value(&result).unwrap()["content"], "");
        assert!(
            serde_json::from_str::<SessionExportResult>(r#"{"path":"export.md","content":123}"#)
                .is_err()
        );
    }
}
