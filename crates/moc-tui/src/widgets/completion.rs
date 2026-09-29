//! 斜杠命令与补全菜单状态。

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
        description: "切换会话模式：标准／问答／计划",
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

/// 补全菜单状态：`None` = 未打开。
#[derive(Debug, Clone)]
pub struct CompletionState {
    pub candidates: Vec<&'static SlashCommand>,
    pub selected: usize,
}

impl CompletionState {
    /// 依据首行 `/` 前缀计算候选；无候选时返回 `None`。
    pub fn derive(text: &str) -> Option<Self> {
        let first_line = text.lines().next().unwrap_or_default();
        let query = first_line.strip_prefix('/')?;
        if query.contains(' ') {
            // 已进入参数阶段：菜单关闭
            return None;
        }
        let candidates: Vec<&'static SlashCommand> = SLASH_COMMANDS
            .iter()
            .filter(|command| command.name[1..].starts_with(query))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            candidates,
            selected: 0,
        })
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.candidates.len() - 1);
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// 选中命令的名字（含 `/`）。
    pub fn chosen(&self) -> &'static str {
        self.candidates[self.selected].name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_filters_and_keeps_order() {
        let state = CompletionState::derive("/mo").expect("有候选");
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.candidates[0].name, "/model");
        assert_eq!(state.candidates[1].name, "/mode");
    }

    #[test]
    fn closes_when_args_started() {
        assert!(CompletionState::derive("/model ").is_none());
        assert!(CompletionState::derive("聊天文本").is_none());
        assert!(CompletionState::derive("/zzz").is_none());
    }
}
