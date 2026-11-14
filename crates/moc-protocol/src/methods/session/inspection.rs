//! 用户侧只读便签；正文由 core 生成，各前端直接展示，不加入模型历史。

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
