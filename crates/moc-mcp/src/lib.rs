//! moc 的 MCP（Model Context Protocol）客户端 crate。
//!
//! 职责边界：本 crate 只做「协议层」的事——配置发现与归一化、
//! 连接生命周期（握手 / 重连 / `tools/list_changed`）、工具列表快照与 `tools/call`。
//! 到 moc 工具体系（`Tool` trait、审批、模式）的桥接在 `moc-core` 侧完成。

pub mod client;
pub mod config;
pub mod content;
pub mod error;
pub mod manager;

pub use config::{
    JsonSource, McpDocument, RawServerSpec, ServerConfig, ToolFilter, Transport,
    discover_project_json, expand_env, parse_mcp_json, resolve_all, resolve_server, user_json_path,
};
pub use content::{ImageData, ToolOutput};
pub use error::McpError;
pub use manager::{McpManager, McpNotice, McpSnapshot, McpToolInfo, ServerStatus};
