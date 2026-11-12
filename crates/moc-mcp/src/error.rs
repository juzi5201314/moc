//! MCP 客户端的统一错误类型。
//!
//! 连接可自动重建，但已发送的调用不能重放：传输错误、超时与取消都不能证明未执行。

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// 配置非法（来源解析、传输字段缺失、server 名不合法等）。
    #[error("MCP configuration error: {0}")]
    Config(String),
    /// 传输层或连接层失败。
    #[error("MCP connection error: {0}")]
    Transport(String),
    /// 调用已发出但没有最终答复；禁止自动重试有副作用的工具。
    #[error(
        "MCP execution outcome is unknown: {0}; the tool may have executed. Verify its effects before retrying"
    )]
    OutcomeUnknown(String),
    /// 调用超出时限；不重试。
    #[error(
        "MCP call timed out after {0}s; execution outcome is unknown. Verify its effects before retrying"
    )]
    Timeout(u64),
    /// 服务端返回的 JSON-RPC 错误或 `isError` 结果；不重试。
    #[error("{0}")]
    Tool(String),
    /// 调用方取消；不重试。
    #[error("MCP call cancelled")]
    Cancelled,
    /// 会话关闭后仍发起调用。
    #[error("MCP manager is closed")]
    Shutdown,
}
