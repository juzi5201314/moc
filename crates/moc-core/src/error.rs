//! moc-core 的统一错误类型。

use std::io;

/// core 内可映射到协议错误或诊断输出的错误。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 配置无效或无法解析。
    #[error("配置错误：{0}")]
    Config(String),
    /// 会话状态或持久化操作失败。
    #[error("会话错误：{0}")]
    Session(String),
    /// 本地 I/O 失败。
    #[error(transparent)]
    Io(#[from] io::Error),
    /// 上游模型调用失败。
    #[error(transparent)]
    Llm(#[from] moc_llm::LlmError),
    /// 工具调用失败。
    #[error("工具错误：{0}")]
    Tool(String),
    /// 协议边界错误。
    #[error("协议错误：{0}")]
    Protocol(String),
    /// 无法在模型上下文窗口内安全继续。
    #[error("上下文溢出：{0}")]
    ContextOverflow(String),
    /// 操作已取消。
    #[error("操作已取消")]
    Cancelled,
}

impl CoreError {
    /// 错误原文，不含「配置错误：」这类分类前缀；再包一层错误时用它避免前缀重复。
    pub fn detail(&self) -> String {
        match self {
            CoreError::Config(text)
            | CoreError::Session(text)
            | CoreError::Tool(text)
            | CoreError::Protocol(text)
            | CoreError::ContextOverflow(text) => text.clone(),
            other => other.to_string(),
        }
    }
}
