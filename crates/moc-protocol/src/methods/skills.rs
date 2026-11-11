//! 技能摘要维护：独立请求，不创建会话或追加聊天历史。

use crate::method::{ClientRequest, Method, ServerNotification};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillsCompactAction {
    #[default]
    Missing,
    Force,
    Cleanup,
    Clear,
}

impl SkillsCompactAction {
    pub fn parse_args(args: &str) -> Result<Self, &'static str> {
        let mut words = args.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (None, None, None) | (Some("compact"), None, None) => Ok(Self::Missing),
            (Some("force"), None, None) => Ok(Self::Force),
            (Some("cleanup"), None, None) => Ok(Self::Cleanup),
            (Some("cleanup"), Some("all"), None) => Ok(Self::Clear),
            _ => Err("用法：/compact-skills [compact | force | cleanup [all]]"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsCompactParams {
    /// 已有会话使用其模型与目录；留空时只使用草稿参数，不创建会话。
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub action: SkillsCompactAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillsCompactResult {
    pub compressed: usize,
    pub skipped: usize,
    pub removed: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// 只用于临时命令结果，不追加到 transcript。
    pub message: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SkillsCompact;
impl Method for SkillsCompact {
    const NAME: &'static str = "skills/compact";
    type Params = SkillsCompactParams;
    type Result = SkillsCompactResult;
    crate::method::json_codec!();
}
impl ClientRequest for SkillsCompact {}

#[derive(Debug, Clone, Copy)]
pub struct SkillsCancel;
impl Method for SkillsCancel {
    const NAME: &'static str = "skills/cancel";
    type Params = super::EmptyParams;
    type Result = super::SessionCancelResult;
    crate::method::json_codec!();
}
impl ClientRequest for SkillsCancel {}

/// 仅发送到发起连接的临时进度，不属于 session/update，不写入会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillsProgressPhase {
    Generating,
    Committing,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsProgressParams {
    pub phase: SkillsProgressPhase,
    /// 已完成生成并通过校验的数量，尚未发布缓存。
    pub completed: usize,
    pub total: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SkillsProgress;
impl Method for SkillsProgress {
    const NAME: &'static str = "skills/progress";
    type Params = SkillsProgressParams;
    type Result = ();
    crate::method::json_codec!();
}
impl ServerNotification for SkillsProgress {}

#[cfg(test)]
mod tests {
    use super::SkillsCompactAction;

    #[test]
    fn compact_subcommand_matches_default_and_rejects_extra_arguments() {
        for args in ["", "compact", " compact \n"] {
            assert_eq!(
                SkillsCompactAction::parse_args(args),
                Ok(SkillsCompactAction::Missing)
            );
        }
        for args in ["compact force", "compact all", "cleanup all extra"] {
            assert!(SkillsCompactAction::parse_args(args).is_err());
        }
    }
}
