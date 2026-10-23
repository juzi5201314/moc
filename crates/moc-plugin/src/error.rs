//! 插件错误的统一形状：稳定错误码 + 给人看的消息。

/// 宿主认识的错误码。插件自己抛出的异常若带 `code`，归一化到这一组里的某一个，
/// 否则落到 `E_PLUGIN`——错误码是给上层分支判断用的，不能是任意字符串。
pub const CODES: [&str; 19] = [
    "E_PLUGIN_CONFIG",
    "E_PLUGIN",
    "E_PLUGIN_LOAD",
    "E_PLUGIN_RUNTIME",
    "E_PLUGIN_THREAD",
    "E_PLUGIN_IMPORT",
    "E_PLUGIN_DESCRIPTOR",
    "E_PLUGIN_VALUE",
    "E_PLUGIN_STATE",
    "E_PLUGIN_MEMORY",
    "E_PLUGIN_INTERRUPTED",
    "E_PLUGIN_TIMEOUT",
    "E_PLUGIN_CRASH",
    "E_PLUGIN_TOOL",
    "E_PLUGIN_COMMAND",
    "E_PLUGIN_DENIED",
    "E_PLUGIN_TOOLS_UNAVAILABLE",
    "E_PLUGIN_ARGUMENTS",
    "E_PLUGIN_RECURSION",
];

/// 插件错误。`code` 是稳定标识（会进模型可见文本与前端），`message` 面向人和模型。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct PluginError {
    pub code: &'static str,
    pub message: String,
}

impl PluginError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 把可能来自插件的错误码归一化成宿主认识的那一组。
    pub fn known_code(code: &str) -> &'static str {
        CODES
            .iter()
            .find(|candidate| **candidate == code)
            .copied()
            .unwrap_or("E_PLUGIN")
    }
}

pub type Result<T> = std::result::Result<T, PluginError>;
