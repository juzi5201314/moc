//! 斜杠命令与资源 URI 补全菜单状态。

use std::ops::Range;

use moc_protocol::ResourceCandidate;
use ratatui::layout::Rect;

/// 一条斜杠命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

/// 插件命令在补全里的名字：`/` 加插件命令名。
fn plugin_candidate(command: &moc_protocol::methods::PluginCommandInfo) -> CompletionCandidate {
    CompletionCandidate {
        value: format!("/{}", command.name),
        label: format!("/{}", command.name),
        description: if command.description.is_empty() {
            "插件命令".to_owned()
        } else {
            command.description.clone()
        },
        append_space: true,
        source: CompletionSource::Slash,
    }
}

/// 内置斜杠命令表。
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/btw",
        description: "旁路问答：带问题继续线程；不带问题重开面板",
    },
    SlashCommand {
        name: "/config",
        description: "打开设置：模型、供应商、界面等",
    },
    SlashCommand {
        name: "/settings",
        description: "打开设置（/config 别名）",
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
        description: "切换会话模式：标准／计划／PTC／CLM实验",
    },
    SlashCommand {
        name: "/clm-experimental",
        description: "切换 CLM 实验模式；启用后本会话固定使用 CLM",
    },
    SlashCommand {
        name: "/permission",
        description: "切换 full / workspace / workspace-only 权限，或清空会话授权",
    },
    SlashCommand {
        name: "/ask",
        description: "本条消息按问答模式处理（只读）",
    },
    SlashCommand {
        name: "/retry",
        description: "接着现有上下文继续跑（不追加用户消息）",
    },
    SlashCommand {
        name: "/compact",
        description: "手动压缩上下文",
    },
    SlashCommand {
        name: "/context",
        description: "查看上下文内容",
    },
    SlashCommand {
        name: "/usage",
        description: "查看当前会话模型用量",
    },
    SlashCommand {
        name: "/quit",
        description: "退出 moc",
    },
    SlashCommand {
        name: "/confirm-plan",
        description: "计划模式下重新审阅已写入的待审计划",
    },
    SlashCommand {
        name: "/plugins",
        description: "查看插件：来源、工具与斜杠命令；/plugins reload 重载",
    },
    SlashCommand {
        name: "/close",
        description: "卸载当前会话并释放跨进程写租约",
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
    /// 上一帧绘制的菜单区域；鼠标命中使用。
    pub area: Rect,
    /// 视口滚动偏移量（跳过的前部候选数）。
    pub offset: usize,
    /// 上一帧渲染的可见候选行数。
    pub visible_count: usize,
}

impl CompletionState {
    /// 依据首行 `/` 前缀计算候选；无候选时返回 `None`。
    ///
    /// `plugins` 是当前工作区插件注册的命令，与内置命令一起参与前缀匹配；
    /// 同名时内置命令在前，分派也以内置为准。
    pub fn derive(
        text: &str,
        plugins: &[moc_protocol::methods::PluginCommandInfo],
    ) -> Option<Self> {
        let first_line = text.lines().next().unwrap_or_default();
        let query = first_line.strip_prefix('/')?;
        if query.contains(char::is_whitespace) {
            return None;
        }
        let candidates: Vec<_> = SLASH_COMMANDS
            .iter()
            .filter(|command| command.name[1..].starts_with(query))
            .map(CompletionCandidate::slash)
            .chain(
                plugins
                    .iter()
                    .filter(|command| command.name.starts_with(query))
                    .map(plugin_candidate),
            )
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            candidates,
            selected: 0,
            replace_range: 0..first_line.len(),
            area: Rect::default(),
            offset: 0,
            visible_count: 0,
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
            area: Rect::default(),
            offset: 0,
            visible_count: 0,
        })
    }

    pub fn select_next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
        self.adjust_scroll();
    }

    pub fn select_previous(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
        self.adjust_scroll();
    }

    pub fn adjust_scroll(&mut self) {
        let page = if self.visible_count > 0 {
            self.visible_count
        } else {
            20
        };
        let count = self.candidates.len();
        if count == 0 {
            self.offset = 0;
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(count - 1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + page {
            self.offset = self.selected + 1 - page;
        }
        self.offset = self.offset.min(count.saturating_sub(page));
    }

    /// 命中的候选条目下标；点在边框、说明行或菜单外返回 `None`。
    pub fn entry_at(&self, column: u16, row: u16) -> Option<usize> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        if column == self.area.x
            || column
                == self
                    .area
                    .x
                    .saturating_add(self.area.width)
                    .saturating_sub(1)
        {
            return None;
        }
        let rel_y = usize::from(row.checked_sub(self.area.y.saturating_add(1))?);
        let count = if self.visible_count > 0 {
            self.visible_count
        } else {
            self.candidates.len()
        };
        if rel_y >= count {
            return None;
        }
        let index = self.offset + rel_y;
        (index < self.candidates.len()).then_some(index)
    }

    pub fn chosen(&self) -> &CompletionCandidate {
        &self.candidates[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_table_filters_closes_and_keeps_order() {
        let state = CompletionState::derive("/mo", &[]).expect("有候选");
        assert_eq!(state.candidates.len(), 2);
        assert_eq!(state.candidates[0].value, "/model");
        assert_eq!(state.candidates[1].value, "/mode");

        // 参数一旦开始、非命令文本或无匹配前缀都不提供候选。
        assert!(CompletionState::derive("/model ", &[]).is_none());
        assert!(CompletionState::derive("聊天文本", &[]).is_none());
        assert!(CompletionState::derive("/zzz", &[]).is_none());
    }
    #[test]
    fn resource_candidate_keeps_namespace_without_space() {
        let state = CompletionState::from_resources(
            vec![ResourceCandidate {
                uri: "local://".into(),
                label: "plans/".into(),
                description: "计划目录".into(),
                kind: "namespace".into(),
                append_space: false,
            }],
            0..7,
        )
        .expect("资源候选");
        assert_eq!(state.chosen().value, "local://");
        assert!(!state.chosen().append_space);
    }

    #[test]
    fn entry_at_detects_candidate_rows_and_ignores_borders() {
        let mut state = CompletionState::derive("/mo", &[]).expect("有候选");
        state.area = Rect::new(10, 5, 30, 5);
        state.visible_count = 2;

        assert_eq!(state.entry_at(10, 6), None, "左边框忽略");
        assert_eq!(state.entry_at(39, 6), None, "右边框忽略");
        assert_eq!(state.entry_at(15, 5), None, "上边框忽略");
        assert_eq!(state.entry_at(15, 6), Some(0), "第 1 条候选命中");
        assert_eq!(state.entry_at(15, 7), Some(1), "第 2 条候选命中");
        assert_eq!(
            state.entry_at(15, 8),
            None,
            "超出候选行（说明文字/下边框）忽略"
        );
        assert_eq!(state.entry_at(5, 6), None, "区域外忽略");
    }
}
