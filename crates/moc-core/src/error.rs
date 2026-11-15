//! moc-core 的统一错误类型。

use rust_i18n::t;
use std::io;

/// core 内可映射到协议错误或诊断输出的错误。
///
/// `Display` 手写而不用 `#[error]`：分类前缀要走 rust-i18n，attribute 只接受字面量。
#[derive(Debug)]
pub enum CoreError {
    /// 配置无效或无法解析。
    Config(String),
    /// 会话状态或持久化操作失败。
    Session(String),
    /// 本地 I/O 失败。
    Io(io::Error),
    /// 上游模型调用失败。
    Llm(moc_llm::LlmError),
    /// 工具调用失败。
    Tool(String),
    /// 协议边界错误。
    Protocol(String),
    /// 无法在模型上下文窗口内安全继续。
    ContextOverflow(String),
    /// 操作已取消。
    Cancelled,
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(text) => write!(f, "{}{text}", t!("core.err.prefix_config")),
            Self::Session(text) => write!(f, "{}{text}", t!("core.err.prefix_session")),
            Self::Io(error) => error.fmt(f),
            Self::Llm(error) => error.fmt(f),
            Self::Tool(text) => write!(f, "{}{text}", t!("core.err.prefix_tool")),
            Self::Protocol(text) => write!(f, "{}{text}", t!("core.err.prefix_protocol")),
            Self::ContextOverflow(text) => {
                write!(f, "{}{text}", t!("core.err.prefix_overflow"))
            }
            Self::Cancelled => f.write_str(t!("core.err.cancelled").as_ref()),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Llm(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<moc_llm::LlmError> for CoreError {
    fn from(error: moc_llm::LlmError) -> Self {
        Self::Llm(error)
    }
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
