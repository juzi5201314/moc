//! 斜杠命令与资源 URI 补全菜单状态。
//!
//! 命令名与参数的候选数据来自 `crate::commands::COMMANDS` 注册表（含插件命令
//! 这一动态来源）；本文件只负责把注册表候选翻译成菜单条目与命中区域。

use std::ops::Range;

use moc_protocol::ResourceCandidate;
use moc_protocol::commands::{ArgCompletion, CommandCandidate, CommandSpec, DynamicArg};
use ratatui::layout::Rect;

use crate::app::App;
use crate::commands::CommandId;

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

/// 当前上下文下这条内置命令是否可提示（与 `execute_command` 的门控一致）。
fn command_visible(app: &App, spec: &CommandSpec<CommandId>) -> bool {
    spec.flag.is_none_or(|flag| app.features.enabled(flag))
        && (spec.available_in_side || !app.focused_agent())
        && (spec.available_during_task || !app.busy())
        && (!spec.needs_session || app.session.is_some())
}

/// 注册表候选 → 菜单条目。
fn command_candidate(candidate: CommandCandidate) -> CompletionCandidate {
    CompletionCandidate {
        value: candidate.value,
        label: candidate.label,
        description: candidate.description,
        append_space: candidate.append_space,
        source: CompletionSource::Slash,
    }
}

/// 参数段候选：按命令 spec 的 args 声明供给；插件命令与自由文本参数不出候选。
fn arg_candidates(app: &App, name: &str, args: &str) -> Option<Vec<CompletionCandidate>> {
    // 只补全第一个参数；`args` 里再出现空白说明参数已完整。
    if args.contains(char::is_whitespace) {
        return None;
    }
    let spec = app.commands.builtin(name)?;
    let values: Vec<String> = match &spec.args {
        ArgCompletion::None => return None,
        ArgCompletion::Choices(choices) => {
            choices.iter().map(|choice| (*choice).to_owned()).collect()
        }
        ArgCompletion::Dynamic(kind) => dynamic_args(app, *kind),
    };
    Some(
        values
            .into_iter()
            .filter(|value| value.starts_with(args))
            .map(|value| CompletionCandidate {
                label: value.clone(),
                value,
                description: format!("/{} 参数", spec.name),
                append_space: false,
                source: CompletionSource::Slash,
            })
            .collect(),
    )
}

/// 动态参数候选的实际数据；每种来源从 app 状态取当前可用集合。
fn dynamic_args(app: &App, kind: DynamicArg) -> Vec<String> {
    match kind {
        // `clear` 清空本会话授权，与各权限模式并列。
        DynamicArg::SandboxMode => app
            .sandbox_modes
            .iter()
            .map(|mode| mode.as_str().to_owned())
            .chain(std::iter::once("clear".to_owned()))
            .collect(),
    }
}

impl CompletionCandidate {
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
    /// 命令名候选经注册表过滤：hidden、flag 关闭、聚焦旁支不可用、运行中不可用、
    /// 无会话的命令都不提示。空格之后进入参数段，按命令 spec 的 args 声明补全。
    pub fn derive(app: &App) -> Option<Self> {
        let first_line = app.prompt.text.lines().next().unwrap_or_default();
        let query = first_line.strip_prefix('/')?;
        let (candidates, replace_start) = match query.split_once(char::is_whitespace) {
            None => (
                app.commands
                    .complete(query, |spec| command_visible(app, spec))
                    .into_iter()
                    .map(command_candidate)
                    .collect::<Vec<_>>(),
                0,
            ),
            Some((name, args)) => (arg_candidates(app, name, args)?, name.len() + 2),
        };
        if candidates.is_empty() {
            return None;
        }
        Some(Self {
            candidates,
            selected: 0,
            replace_range: replace_start..first_line.len(),
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

    fn app(text: &str) -> App {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.prompt.text = text.to_owned();
        app
    }

    #[test]
    fn derive_table_filters_closes_and_keeps_order() {
        let state = CompletionState::derive(&app("/r")).expect("有候选");
        // `/resume` 不需会话而 `/retry` 需要：无会话时只剩前者。
        assert_eq!(
            state
                .candidates
                .iter()
                .map(|candidate| candidate.value.as_str())
                .collect::<Vec<_>>(),
            ["/resume"]
        );

        // 非命令文本或无匹配前缀都不提供候选。
        assert!(CompletionState::derive(&app("聊天文本")).is_none());
        assert!(CompletionState::derive(&app("/zzz")).is_none());
    }

    #[test]
    fn argument_candidates_follow_spec() {
        // `/autonomy` 的固定取值进候选，命中前缀才出现。
        let state = CompletionState::derive(&app("/autonomy un")).expect("参数候选");
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].value, "unattended");
        assert!(!state.candidates[0].append_space);
        assert_eq!(
            state.replace_range,
            "/autonomy ".len().."/autonomy un".len()
        );

        // 完整参数（再跟空格）与无参数命令都没有候选。
        assert!(CompletionState::derive(&app("/autonomy default x")).is_none());
        assert!(CompletionState::derive(&app("/new ")).is_none());
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
        let mut state = CompletionState::derive(&app("/")).expect("有候选");
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
