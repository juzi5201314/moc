//! 用户主动执行 Bash；与模型提示共用会话取消和繁忙边界，不调用模型。

use serde::{Deserialize, Serialize};

use super::session::ToolStatus;
use crate::ToolResult;
use crate::method::{ClientRequest, Method};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionBash;

impl Method for SessionBash {
    const NAME: &'static str = "session/bash";
    type Params = BashParams;
    type Result = BashResult;
}

impl ClientRequest for SessionBash {}

/// 命令来源及上下文可见性；随执行结果持久化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashCommand {
    pub command: String,
    #[serde(default)]
    pub exclude_from_context: bool,
}

impl BashCommand {
    /// 只识别输入开头的感叹号；先消费 `!!`，其后的内容完整交给 Bash。
    pub fn parse(input: &str) -> Option<Self> {
        let rest = input.strip_prefix('!')?;
        let (command, exclude_from_context) = match rest.strip_prefix('!') {
            Some(command) => (command, true),
            None => (rest, false),
        };
        Some(Self {
            command: command.to_owned(),
            exclude_from_context,
        })
    }

    /// 转成带有用户执行来源的文本，不伪造 assistant 的工具调用。
    ///
    /// 文本进入模型上下文：遵循 moc-core `system_prompt` 模块的提示词编写规范（英文、紧凑）。
    pub fn context_message(&self, output: &crate::Message) -> crate::Message {
        crate::Message::user_text(format!(
            "The user ran this command manually (not a request to you):\n```bash\n{}\n```\nOutput:\n{}",
            self.command,
            output.text()
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashParams {
    pub session_id: String,
    pub command: BashCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashResult {
    pub result: ToolResult,
    pub status: ToolStatus,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::BashCommand;

    #[test]
    fn parse_distinguishes_context_visibility_without_trimming_shell_input() {
        assert_eq!(
            BashCommand::parse("!printf 'a b'"),
            Some(BashCommand {
                command: "printf 'a b'".into(),
                exclude_from_context: false
            })
        );
        assert_eq!(
            BashCommand::parse("!!printf 'a b'"),
            Some(BashCommand {
                command: "printf 'a b'".into(),
                exclude_from_context: true
            })
        );
        assert_eq!(BashCommand::parse("echo !"), None);
        assert_eq!(BashCommand::parse(" !!! false\n"), None);
        assert_eq!(
            BashCommand::parse("!!! false\n"),
            Some(BashCommand {
                command: "! false\n".into(),
                exclude_from_context: true
            })
        );
    }
}
