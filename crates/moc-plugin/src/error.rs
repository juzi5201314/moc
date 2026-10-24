//! 插件错误的统一形状：稳定错误码 + 给人看的消息。

/// 宿主认识的错误码。插件自己抛出的异常若带 `code`，归一化到这一组里的某一个，
/// 否则落到 `E_PLUGIN`——错误码是给上层分支判断用的，不能是任意字符串。
///
/// `E_PLUGIN_DISABLED` 由宿主产生：插件已在本进程内停用后，后续调用统一返回它，
/// 而不是复读首次失败的原因。
pub const CODES: [&str; 20] = [
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
    "E_PLUGIN_DISABLED",
];

/// 插件错误。`code` 是稳定标识（会进模型可见文本与前端），`message` 面向人和模型。
///
/// `detail` 只给用户看（目前是 JS 调用栈）：它不进 `Display`，避免把多行栈塞进
/// 模型可见的工具结果；`/plugins` 与失败通知会带上它。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct PluginError {
    pub code: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

impl PluginError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// 附一段诊断信息（JS 调用栈）；已有内容时换行追加。
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let detail = detail.trim_end();
        if detail.is_empty() {
            return self;
        }
        match &mut self.detail {
            Some(existing) => {
                existing.push('\n');
                existing.push_str(detail);
            }
            None => self.detail = Some(detail.to_owned()),
        }
        self
    }

    /// 插件已在本进程内禁用后的复读错误：带上首次失败原因与恢复方式。
    pub fn disabled(plugin: &str, first: &PluginError) -> Self {
        Self::new(
            "E_PLUGIN_DISABLED",
            format!(
                "插件 `{plugin}` 已在本进程内禁用（首次失败：{first}）；修复后运行 /plugins reload 或重启 moc"
            ),
        )
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
