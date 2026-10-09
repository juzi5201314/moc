//! MCP 客户端的统一错误类型。
//!
//! [`McpError::Transport`] 是唯一会触发「等待重连后单次重试」的类别；
//! 服务端业务错误与超时都不重试，避免副作用型工具被重复执行。

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// 配置非法（来源解析、传输字段缺失、server 名不合法等）。
    #[error("MCP configuration error: {0}")]
    Config(String),
    /// 传输层或连接层失败；等待重连成功后可重试一次。
    #[error("MCP connection error: {0}")]
    Transport(String),
    /// 调用超出时限；不重试。
    #[error("MCP call timed out after {0}s")]
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

impl McpError {
    /// 是否属于「连接层故障」，允许等待重连后重试一次。
    pub fn is_retriable(&self) -> bool {
        matches!(self, McpError::Transport(_))
    }
}
