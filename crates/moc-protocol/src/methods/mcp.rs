//! MCP 服务器运行态查询、手动重连与实时推送。
//!
//! 独立于会话提示生命周期：前端在打开工作区、发送首条消息之前即可看到每个 MCP 的
//! 连接进度、失败原因与 stderr 尾部，同时不阻塞正常会话。

use crate::method::{ClientRequest, Method, ServerNotification};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 单个 MCP server 的连接阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpServerPhase {
    Connecting,
    Connected,
    Reconnecting,
    Failed,
    Disabled,
}

/// 单个 MCP server 的对外状态摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerInfo {
    pub name: String,
    /// 传输方式：`stdio` 或 `http`。
    pub transport: String,
    /// 命令摘要或端点 URL。
    pub target: String,
    pub phase: McpServerPhase,
    /// 已发现的工具数（仅 `connected` 时有值）。
    #[serde(default)]
    pub tool_count: usize,
    /// 当前中断内的连续失败次数（仅 `reconnecting` 时非零）。
    #[serde(default)]
    pub attempts: u32,
    /// 当前握手尝试开始的 UNIX 毫秒时间戳；前端据此本地计算已耗时。
    #[serde(default)]
    pub started_at_ms: Option<u64>,
    /// 最近一次连接失败、超时或协议错误原因。
    #[serde(default)]
    pub error: Option<String>,
    /// stdio 子进程最近的 stderr 行（有界环形缓冲）。
    #[serde(default)]
    pub recent_stderr: Vec<String>,
}

/// 一个工作目录的 MCP 全量状态快照；`mcp/list`、`mcp/reconnect` 与 `mcp/status_updated` 共用。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpStatusSnapshot {
    /// 规范化后的项目目录。
    pub cwd: String,
    /// 各 server 的状态（按名称排序）。
    #[serde(default)]
    pub servers: Vec<McpServerInfo>,
    /// 配置文件解析失败时的诊断；非空时不阻断其它会话能力。
    #[serde(default)]
    pub config_error: Option<String>,
}

/// `mcp/list`：查询指定工作目录（或会话所属目录）的 MCP 服务器运行态快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpList;

impl Method for McpList {
    const NAME: &'static str = "mcp/list";
    type Params = McpListParams;
    type Result = McpStatusSnapshot;
    crate::method::json_codec!();
}

impl ClientRequest for McpList {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpListParams {
    /// 会话 ID；非空时使用该会话的 cwd。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 显式工作目录（草稿会话或会话前查询）；与 `session_id` 都缺省时使用 core 启动目录。
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `mcp/reconnect`：手动重连指定或全部已启用的 MCP server，重置失败预算并返回最新快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpReconnect;

impl Method for McpReconnect {
    const NAME: &'static str = "mcp/reconnect";
    type Params = McpReconnectParams;
    type Result = McpStatusSnapshot;
    crate::method::json_codec!();
}

impl ClientRequest for McpReconnect {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpReconnectParams {
    /// 会话 ID；非空时使用该会话的 cwd。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 显式工作目录；与 `session_id` 都缺省时使用 core 启动目录。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 指定重连的 server 名；`None` 重连该目录全部已启用的 server。
    #[serde(default)]
    pub server: Option<String>,
}

/// `mcp/status_updated`：项目 MCP 状态变化时由 core 主动扇出给已连接前端。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpStatusUpdated;

impl Method for McpStatusUpdated {
    const NAME: &'static str = "mcp/status_updated";
    type Params = McpStatusSnapshot;
    type Result = crate::methods::Empty;
    crate::method::json_codec!();
}

impl ServerNotification for McpStatusUpdated {}
