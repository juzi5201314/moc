//! 斜杠命令与资源 URI 补全菜单状态。

use std::ops::Range;

use moc_protocol::ResourceCandidate;

/// 一条斜杠命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

/// 内置斜杠命令表。
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/config",
        description: "打开设置：模型、供应商、界面等",
    },
    SlashCommand {
        name: "/help",
        description: "快捷键与命令帮助",
    },
    SlashCommand {
        name: "/new",
        description: "新建会话",
    },
    SlashCommand {
        name: "/model",
        description: "切换模型",
    },
    SlashCommand {
        name: "/resume",
        description: "恢复历史会话",
    },
    SlashCommand {
        name: "/mode",
        description: "切换会话模式：标准／计划／PTC",
    },
    SlashCommand {
        name: "/ask",
        description: "本条消息按问答模式处理（只读）",
    },
    SlashCommand {
        name: "/memories",
        description: "浏览工作区记忆",
    },
    SlashCommand {
        name: "/compact",
        description: "手动压缩上下文",
    },
    SlashCommand {
        name: "/context",
        description: "查看上下文占用明细",
    },
    SlashCommand {
        name: "/quit",
        description: "退出 moc",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSource {
    Slash,
    Uri,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub value: String,
    pub label: String,
    pub description: String,
    pub append_space: bool,
    pub source: CompletionSource,
}

impl CompletionCandidate {
    fn slash(command: &SlashCommand) -> Self {
        Self {
            value: command.name.into(),
            label: command.name.into(),
            description: command.description.into(),
            append_space: true,
            source: CompletionSource::Slash,
        }
    }

    pub fn resource(candidate: ResourceCandidate) -> Self {
        Self {
            value: candidate.uri,
            label: candidate.label,
            description: candidate.description,
            append_space: candidate.append_space,
            source: CompletionSource::Uri,
        }
    }
}

/// 补全菜单状态：`None` = 未打开。
#[derive(Debug, Clone)]
pub struct CompletionState {
    pub candidates: Vec<CompletionCandidate>,
    pub selected: usize,
    pub replace_range: Range<usize>,
}

impl CompletionState {
    /// 依据首行 `/` 前缀计算候选；无候选时返回 `None`。
    pub fn derive(text: &str) -> Option<Self> {
        let first_line = text.lines().next().unwrap_or_default();
        let query = first_line.strip_prefix('/')?;
        if query.contains(char::is_whitespace) {
            return None;
        }
        let candidates: Vec<_> = SLASH_COMMANDS
            .iter()
            .filter(|command| command.name[1..].starts_with(query))
            .map(CompletionCandidate::slash)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            candidates,
            selected: 0,
            replace_range: 0..first_line.len(),
        })
    }

    pub fn from_resources(
        candidates: Vec<ResourceCandidate>,
        replace_range: Range<usize>,
    ) -> Option<Self> {
        let candidates = candidates
            .into_iter()
            .map(CompletionCandidate::resource)
            .collect::<Vec<_>>();
        (!candidates.is_empty()).then_some(Self {
            candidates,
            selected: 0,
            replace_range,
        })
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.candidates.len() - 1);
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn chosen(&self) -> &CompletionCandidate {
        &self.candidates[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_filters_and_keeps_order() {
        let state = CompletionState::derive("/mo").expect("有候选");
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.candidates[0].value, "/model");
        assert_eq!(state.candidates[1].value, "/mode");
    }

    #[test]
    fn closes_when_args_started() {
        assert!(CompletionState::derive("/model ").is_none());
        assert!(CompletionState::derive("聊天文本").is_none());
        assert!(CompletionState::derive("/zzz").is_none());
    }
    #[test]
    fn resource_candidate_keeps_namespace_without_space() {
        let state = CompletionState::from_resources(
            vec![ResourceCandidate {
                uri: "local://ses_1/plans/".into(),
                label: "plans/".into(),
                description: "计划目录".into(),
                kind: "namespace".into(),
                append_space: false,
            }],
            0..7,
        )
        .expect("资源候选");
        assert_eq!(state.chosen().value, "local://ses_1/plans/");
        assert!(!state.chosen().append_space);
    }
}
