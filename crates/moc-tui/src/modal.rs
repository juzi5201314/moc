//! 模态卡片：权限、提问、模型选择、会话恢复、计划审批、帮助。
//!
//! 每种模态自带键盘状态（选中项等）与答复通道（oneshot sender）；键盘
//! 事件优先路由给打开的模态（见 `crate::event`），渲染浮层在 `crate::render`。

use std::ops::Range;

use moc_protocol::Role;
use moc_protocol::methods::{
    AskParams, AskResult, ContextContentResult, ContextContentSection, ContextUsageResult,
    ModelSwitchOverflow, PermissionRequestParams, PermissionResult, SessionInfo, TranscriptEntry,
};

use ratatui::layout::Rect;
use ratatui::text::Line;

/// 可搜索列表模态的状态。
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    /// 当前过滤字符串。
    pub query: String,
    /// 过滤结果中的选中项下标。
    pub selected: usize,
    /// 列表 viewport 的首项下标。
    pub offset: usize,
    /// 是否正在请求后端数据。
    pub loading: bool,
    /// 最近一次请求错误。
    pub error: Option<String>,
}

/// 选择器里的一行：真实模型，或模型角色字段的伪值（沿用主模型 / 沿用当前会话模型）。
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// 写回配置或会话的 selector，不含思考等级；伪值就是 `inherit` / `none`。
    pub selector: String,
    /// 展示名：模型是 `供应商/模型`，伪值是中文标签。
    pub label: String,
    /// 搜索别名（模型的展示名），不参与展示；伪值为空。
    pub display_name: String,
    /// 该模型允许选择的思考等级；伪值没有等级，这里是空列表。
    pub levels: Vec<String>,
    /// 上下文窗口 token 数；未配置或伪值为 `None`。
    pub context_window: Option<u64>,
    /// 单次输出上限 token 数；未配置或伪值为 `None`。
    pub max_output_tokens: Option<u32>,
    /// 接受的输入模态；伪值为空。
    pub inputs: Vec<moc_protocol::methods::ModelInput>,
    /// Responses 原生服务端压缩是否启用。
    pub remote_compaction: bool,
    /// 每百万 token 定价；未配置为 `None`。
    pub pricing: Option<moc_protocol::methods::ModelPricing>,
}

impl ModelEntry {
    /// 选中后写回的 selector；伪值原样返回。
    pub fn selector_with(&self, level: Option<&str>) -> String {
        match level.filter(|_| !self.levels.is_empty()) {
            Some(level) => format!("{}:{level}", self.selector),
            None => self.selector.clone(),
        }
    }
}

/// 模型未声明可选等级时前端使用的集合，与 core 的默认 `thinking_levels` 一致。
pub const DEFAULT_LEVELS: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];

/// 思考等级的强弱次序；名字不可识别时比 `off` 还弱，不会成为默认等级。
fn level_rank(level: &str) -> usize {
    DEFAULT_LEVELS
        .iter()
        .position(|known| *known == level)
        .map_or(0, |index| index + 1)
}

/// 思考等级条的键盘状态：手动选中的等级。
///
/// `manual` 为空表示用高亮模型支持的最高等级；高亮换行时清空，回到「一律用最高等级」。
#[derive(Debug, Clone, Default)]
pub struct LevelState {
    pub manual: Option<String>,
}

impl LevelState {
    /// 高亮换行：等级回到该模型的最高等级。
    pub fn clear_manual(&mut self) {
        self.manual = None;
    }

    /// 当前生效的等级：手动值仍在候选内就用它，否则用候选里最高的等级。
    pub fn resolve(&self, levels: &[String]) -> Option<String> {
        if let Some(manual) = self.manual.as_ref()
            && levels.contains(manual)
        {
            return Some(manual.clone());
        }
        levels.iter().max_by_key(|level| level_rank(level)).cloned()
    }

    /// 在候选等级上循环切档；候选为空时不动。
    pub fn move_level(&mut self, levels: &[String], forward: bool) {
        if levels.is_empty() {
            return;
        }
        let current = self
            .resolve(levels)
            .and_then(|level| levels.iter().position(|candidate| *candidate == level))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % levels.len()
        } else {
            current.checked_sub(1).unwrap_or(levels.len() - 1)
        };
        self.manual = Some(levels[next].clone());
    }
}

/// 计划审阅窗口的视图：渲染后的正文（阅读）或带行号的源码（批注）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlanPane {
    /// 渲染后的计划正文。
    #[default]
    Read,
    /// 带行号的源码视图；批注在这里按行锚定。
    Annotate,
}

/// 还没提交给 core 的一条计划批注。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanAnnotationDraft {
    /// 前端 id，提交时随批注一起带上。
    pub id: String,
    /// 起始行（1-based，含）。
    pub start_line: usize,
    /// 结束行（1-based，含）。
    pub end_line: usize,
    /// 锚定的源码文本；只用于界面回显。
    pub quote: String,
    /// 批注正文，可含 ```` ```suggestion ```` 围栏。
    pub body: String,
}

impl PlanAnnotationDraft {
    /// 这一行是否落在这条批注的区间内。
    pub fn covers(&self, line: usize) -> bool {
        (self.start_line..=self.end_line).contains(&line)
    }
}

/// 正在编辑的一条批注。
pub struct PlanAnnotationEditor {
    /// 正在编辑的草稿下标；`None` 表示新建。
    pub editing: Option<usize>,
    pub start_line: usize,
    pub end_line: usize,
    pub quote: String,
    pub body: crate::widgets::prompt::PromptState,
}

/// 计划审阅窗口的完整状态。
pub struct PlanReviewState {
    pub plan_id: String,
    pub title: String,
    pub path: String,
    pub markdown: String,
    pub status: moc_protocol::methods::PlanStatus,
    /// 审阅版本号（从 1 开始）。
    pub version: u32,
    /// 上一版计划；有值时默认展示「本版改动」。
    pub previous_plan_id: Option<String>,
    /// core 里已经提交过的批注（只读）。
    pub submitted: Vec<moc_protocol::methods::PlanAnnotation>,
    /// 本次窗口里还没提交的草稿。
    pub drafts: Vec<PlanAnnotationDraft>,
    /// 下一次新建批注使用的 id 序号。
    pub next_annotation_id: u64,
    /// 当前视图。
    pub pane: PlanPane,
    /// 源码视图光标（0-based 行下标）。
    pub cursor: usize,
    /// 选区锚点；`Some` 时选区为 `anchor..=cursor`。
    pub anchor: Option<usize>,
    /// 正在编辑的批注。
    pub editor: Option<PlanAnnotationEditor>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub execution_model: String,
    pub submitting: bool,
    pub error: Option<String>,
    /// 与上一版的差异；`None` 表示没有上一版或还没加载。
    pub diff: Option<moc_protocol::methods::ToolDiff>,
    /// 差异请求进行中。
    pub diff_loading: bool,
    /// 差异请求失败原因。
    pub diff_error: Option<String>,
    /// 阅读视图当前展示差异而不是计划正文。
    pub show_diff: bool,
}

impl PlanReviewState {
    pub fn new(plan: moc_protocol::methods::PlanInfo, execution_model: String) -> Self {
        // 有上一版时优先让用户看「改了什么」，没有才看正文。
        let show_diff = plan.previous_plan_id.is_some();
        Self {
            plan_id: plan.plan_id,
            title: plan.title,
            path: plan.path,
            markdown: plan.markdown,
            status: plan.status,
            version: plan.version,
            previous_plan_id: plan.previous_plan_id,
            submitted: plan.annotations,
            drafts: Vec::new(),
            next_annotation_id: 1,
            pane: PlanPane::Read,
            cursor: 0,
            anchor: None,
            editor: None,
            selected: 0,
            scroll_offset: 0,
            execution_model,
            submitting: false,
            error: None,
            diff: None,
            diff_loading: false,
            diff_error: None,
            show_diff,
        }
    }

    /// 计划源码行（0-based）。
    pub fn source_lines(&self) -> Vec<&str> {
        self.markdown.lines().collect()
    }

    /// 当前选区（0-based，含端点）；没有选区时是光标所在单行。
    pub fn selection(&self) -> (usize, usize) {
        let cursor = self.cursor;
        let anchor = self.anchor.unwrap_or(cursor);
        (cursor.min(anchor), cursor.max(anchor))
    }

    /// 当前版本是否还能批注（只有待审阅的计划可以）。
    pub fn can_annotate(&self) -> bool {
        self.status == moc_protocol::methods::PlanStatus::Submitted
    }

    /// 与选区重叠的第一条草稿。
    pub fn draft_at_cursor(&self) -> Option<usize> {
        let (start, end) = self.selection();
        let (start, end) = (start + 1, end + 1);
        self.drafts
            .iter()
            .position(|draft| draft.start_line <= end && draft.end_line >= start)
    }

    /// 打开编辑器：选区落在已有草稿上就编辑它，否则新建一条。
    pub fn open_editor(&mut self) {
        if !self.can_annotate() {
            self.error = Some("当前版本不可批注".into());
            return;
        }
        let lines = self.source_lines();
        if lines.is_empty() {
            return;
        }
        let (start, end) = self.selection();
        let editing = self.draft_at_cursor();
        let (start_line, end_line, quote, body) = match editing {
            Some(index) => {
                let draft = &self.drafts[index];
                (
                    draft.start_line,
                    draft.end_line,
                    draft.quote.clone(),
                    draft.body.clone(),
                )
            }
            None => (
                start + 1,
                end + 1,
                lines[start..=end].join("\n"),
                String::new(),
            ),
        };
        self.error = None;
        self.editor = Some(PlanAnnotationEditor {
            editing,
            start_line,
            end_line,
            quote,
            body: crate::widgets::prompt::PromptState::from_text(body),
        });
    }

    /// 保存编辑器里的批注；正文为空时不落草稿。
    pub fn save_editor(&mut self) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let body = editor.body.text.trim_end().to_owned();
        if body.trim().is_empty() {
            self.error = Some("批注正文不能为空".into());
            return;
        }
        let draft = PlanAnnotationDraft {
            id: format!("a{}", self.next_annotation_id),
            start_line: editor.start_line,
            end_line: editor.end_line,
            quote: truncate_chars(&editor.quote, 4096),
            body,
        };
        let editing = editor.editing;
        self.next_annotation_id += 1;
        self.editor = None;
        self.error = None;
        match editing {
            Some(index) if index < self.drafts.len() => self.drafts[index] = draft,
            _ => self.drafts.push(draft),
        }
    }

    /// 删除光标/选区上的草稿。
    pub fn remove_draft_at_cursor(&mut self) {
        if let Some(index) = self.draft_at_cursor() {
            self.drafts.remove(index);
        }
    }

    /// 移动光标；`select` 为真时同步推进选区端点。
    pub fn move_cursor(&mut self, delta: isize, select: bool) {
        let lines = self.source_lines().len();
        if lines == 0 {
            return;
        }
        let next = self
            .cursor
            .saturating_add_signed(delta)
            .min(lines.saturating_sub(1));
        self.cursor = next;
        if !select {
            self.anchor = None;
        } else if self.anchor.is_none() {
            self.anchor = Some(next);
        }
    }

    /// 让光标落在给定视口高度内。
    pub fn ensure_cursor_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor < self.scroll_offset {
            self.scroll_offset = self.cursor;
        } else if self.cursor >= self.scroll_offset.saturating_add(height) {
            self.scroll_offset = self.cursor + 1 - height;
        }
    }

    /// 提交用的批注列表。
    pub fn annotations(&self) -> Vec<moc_protocol::methods::PlanAnnotation> {
        self.drafts
            .iter()
            .map(|draft| moc_protocol::methods::PlanAnnotation {
                id: draft.id.clone(),
                start_line: u32::try_from(draft.start_line).unwrap_or(u32::MAX),
                end_line: u32::try_from(draft.end_line).unwrap_or(u32::MAX),
                quote: draft.quote.clone(),
                body: draft.body.clone(),
            })
            .collect()
    }
}

/// 按字符数截断，避免把多字节字符切断。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

/// 同一模型选择器可切换会话模型，也可只选择计划执行模型。
pub enum ModelTarget {
    Session,
    PlanReview(PlanReviewState),
    /// `/config` 的模型角色字段：选中后写回该路径的配置值。
    Config {
        path: Vec<String>,
        /// 字段标签，用作改动提示的前缀。
        label: String,
    },
}

impl PickerState {
    pub fn reset(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.offset = 0;
        self.error = None;
    }

    pub fn move_selection(&mut self, delta: i32, count: usize, page: usize) {
        if count == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }
        let max = count - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected.saturating_add(delta as usize).min(max)
        };
        self.keep_visible(page.max(1), count);
    }

    pub fn keep_visible(&mut self, page: usize, count: usize) {
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
}
/// 单个回退目标条目（Turn / Entry）
#[derive(Debug, Clone)]
pub struct RewindItem {
    pub entry_id: String,
    pub role: Role,
    pub is_user: bool,
    pub at_ms: u64,
    pub preview: String,
    pub full_text: String,
    pub tool_calls: Vec<String>,
    pub prompt_index: usize,
    pub item_index: usize,
    pub entry: TranscriptEntry,
}

impl RewindItem {
    pub fn role_label(&self) -> &'static str {
        match self.role {
            Role::User => "用户提问",
            Role::Assistant => "模型回复",
            Role::Tool => "工具结果",
            Role::System | Role::Developer => "系统消息",
        }
    }
}

/// 消息回退选择器状态；支持 omp 风格的线性时间线、提问快速跳转与关键词搜索过滤。
#[derive(Debug, Clone)]
pub struct RewindState {
    pub prompts: Vec<Vec<TranscriptEntry>>,
    pub selected_prompt: usize,
    pub selected_item: usize,
    pub loading: bool,
    pub error: Option<String>,

    /// 扁平化的时间线目标序列
    pub items: Vec<RewindItem>,
    /// 当前在 items 中的选中项下标
    pub selected: usize,
    /// 是否处于搜索过滤态
    pub filtering: bool,
    /// 搜索过滤关键词
    pub query: String,
    /// 列表滚动偏移
    pub list_scroll: usize,
    /// 详情预览滚动偏移
    pub detail_scroll: usize,
}

impl Default for RewindState {
    fn default() -> Self {
        Self {
            prompts: Vec::new(),
            selected_prompt: 0,
            selected_item: 0,
            loading: true,
            error: None,
            items: Vec::new(),
            selected: 0,
            filtering: false,
            query: String::new(),
            list_scroll: 0,
            detail_scroll: 0,
        }
    }
}

impl RewindState {
    pub fn set_entries(&mut self, entries: Vec<TranscriptEntry>) {
        self.prompts.clear();
        self.items.clear();
        for entry in entries {
            // `!!` 命令与代理消息都不是人类提问，不作为回退边界。
            if entry.bash.is_some() || entry.agent.is_some() {
                continue;
            }
            if entry.message.role == Role::User {
                self.prompts.push(vec![entry]);
            } else if let Some(prompt) = self.prompts.last_mut() {
                prompt.push(entry);
            }
        }

        for (p_idx, prompt) in self.prompts.iter().enumerate() {
            for (i_idx, entry) in prompt.iter().enumerate() {
                let is_user = entry.message.role == Role::User;
                let text = entry.message.text();
                let tool_calls: Vec<String> = entry
                    .message
                    .tool_calls()
                    .map(|call| call.name.clone())
                    .collect();

                let preview = if is_user {
                    let first_line = text
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    if first_line.is_empty() {
                        "（用户提问）".to_string()
                    } else {
                        first_line.to_string()
                    }
                } else if !tool_calls.is_empty() {
                    format!("调用工具：{}", tool_calls.join("、"))
                } else {
                    let first_line = text
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    if first_line.is_empty() {
                        if entry.message.role == Role::Tool {
                            "工具结果返回".to_string()
                        } else {
                            "（模型回复）".to_string()
                        }
                    } else {
                        first_line.to_string()
                    }
                };

                let full_text = if !text.trim().is_empty() {
                    text
                } else if !tool_calls.is_empty() {
                    let mut s = format!("调用工具：{}\n", tool_calls.join("、"));
                    for call in entry.message.tool_calls() {
                        s.push_str(&format!(
                            "\n• 工具 [{}] 参数：\n{}\n",
                            call.name, call.arguments
                        ));
                    }
                    s
                } else {
                    entry
                        .message
                        .tool_results()
                        .map(|res| res.content.clone())
                        .collect::<Vec<_>>()
                        .join("\n")
                };

                self.items.push(RewindItem {
                    entry_id: entry.id.clone(),
                    role: entry.message.role,
                    is_user,
                    at_ms: entry.at_ms,
                    preview,
                    full_text,
                    tool_calls,
                    prompt_index: p_idx,
                    item_index: i_idx,
                    entry: entry.clone(),
                });
            }
        }

        self.selected_prompt = self.prompts.len().saturating_sub(1);
        self.selected_item = self
            .prompts
            .last()
            .map_or(0, |prompt| prompt.len().saturating_sub(1));
        self.sync_selected_from_prompt_item();
        self.loading = false;
        self.error = None;
        self.filtering = false;
        self.query.clear();
        self.list_scroll = 0;
        self.detail_scroll = 0;
    }

    pub fn sync_selected_from_prompt_item(&mut self) {
        if let Some(pos) = self.items.iter().position(|it| {
            it.prompt_index == self.selected_prompt && it.item_index == self.selected_item
        }) {
            self.selected = pos;
        } else {
            self.selected = self.items.len().saturating_sub(1);
        }
        self.detail_scroll = 0;
    }

    pub fn sync_prompt_item_from_selected(&mut self) {
        if let Some(item) = self.items.get(self.selected) {
            self.selected_prompt = item.prompt_index;
            self.selected_item = item.item_index;
        }
        self.detail_scroll = 0;
    }

    pub fn selected(&self) -> Option<&TranscriptEntry> {
        if let Some(item) = self.items.get(self.selected) {
            Some(&item.entry)
        } else {
            self.prompts
                .get(self.selected_prompt)
                .and_then(|prompt| prompt.get(self.selected_item))
        }
    }

    pub fn selected_item_ref(&self) -> Option<&RewindItem> {
        self.items.get(self.selected)
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        let words: Vec<&str> = q.split_whitespace().collect();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                let target_str = format!("{} {}", it.preview, it.full_text).to_lowercase();
                words.iter().all(|w| target_str.contains(w))
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn step(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let matches = self.filtered_indices();
        if matches.is_empty() {
            return;
        }
        let curr_pos = matches.iter().position(|&idx| idx == self.selected);
        let new_pos = match curr_pos {
            Some(p) => (p as i32 + delta).clamp(0, matches.len() as i32 - 1) as usize,
            None => {
                if delta < 0 {
                    matches
                        .iter()
                        .rposition(|&idx| idx < self.selected)
                        .unwrap_or(0)
                } else {
                    matches
                        .iter()
                        .position(|&idx| idx > self.selected)
                        .unwrap_or(matches.len() - 1)
                }
            }
        };
        self.selected = matches[new_pos];
        self.sync_prompt_item_from_selected();
    }

    pub fn jump_user(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let matches = self.filtered_indices();
        if matches.is_empty() {
            return;
        }
        if delta < 0 {
            if let Some(&prev_idx) = matches
                .iter()
                .rfind(|&&idx| idx < self.selected && self.items[idx].is_user)
            {
                self.selected = prev_idx;
                self.sync_prompt_item_from_selected();
            }
        } else if delta > 0
            && let Some(&next_idx) = matches
                .iter()
                .find(|&&idx| idx > self.selected && self.items[idx].is_user)
        {
            self.selected = next_idx;
            self.sync_prompt_item_from_selected();
        }
    }

    pub fn move_vertical(&mut self, delta: i32) {
        let Some(prompt) = self.prompts.get(self.selected_prompt) else {
            return;
        };
        let max = prompt.len().saturating_sub(1);
        self.selected_item = if delta < 0 {
            self.selected_item
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_item.saturating_add(delta as usize).min(max)
        };
        self.sync_selected_from_prompt_item();
    }

    pub fn move_horizontal(&mut self, delta: i32) {
        if self.prompts.is_empty() {
            return;
        }
        let max = self.prompts.len().saturating_sub(1);
        self.selected_prompt = if delta < 0 {
            self.selected_prompt
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_prompt.saturating_add(delta as usize).min(max)
        };
        self.selected_item = self
            .selected_item
            .min(self.prompts[self.selected_prompt].len().saturating_sub(1));
        self.sync_selected_from_prompt_item();
    }

    pub fn start_filter(&mut self) {
        self.filtering = true;
    }

    pub fn input_char(&mut self, c: char) {
        self.query.push(c);
        let matches = self.filtered_indices();
        if !matches.is_empty() && !matches.contains(&self.selected) {
            self.selected = matches[0];
            self.sync_prompt_item_from_selected();
        }
    }

    pub fn backspace(&mut self) {
        if !self.query.is_empty() {
            self.query.pop();
            let matches = self.filtered_indices();
            if !matches.is_empty() && !matches.contains(&self.selected) {
                self.selected = matches[0];
                self.sync_prompt_item_from_selected();
            }
        } else {
            self.filtering = false;
        }
    }

    pub fn clear_filter(&mut self) {
        self.query.clear();
        self.filtering = false;
    }

    pub fn scroll_detail(&mut self, delta: i32) {
        if delta < 0 {
            self.detail_scroll = self
                .detail_scroll
                .saturating_sub(delta.unsigned_abs() as usize);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as usize);
        }
    }
}

/// 上下文正文窗：分块折叠后的可见行数，超出的部分折叠成「点击展开」提示行。
pub const CONTEXT_COLLAPSED_LINES: usize = 6;

/// 上下文正文窗：单个分块的正文上限（字节）。超长工具结果不至于把整窗拉成
/// 几十兆，缺口在分块末尾标出来。
pub const CONTEXT_SECTION_BYTES: u32 = 256 * 1024;

/// 正文窗的渲染缓存：与分块列表、展开状态和列宽同源。
#[derive(Debug)]
pub struct ContextLayout {
    /// 生成缓存时的正文列宽。
    pub width: usize,
    /// 每块在行表里的行区间（标题行 + 正文行 + 提示行 + 块间空行）。
    pub spans: Vec<Range<usize>>,
    /// 全部渲染行。
    pub lines: Vec<Line<'static>>,
}

impl ContextLayout {
    /// 行数不足一屏时的最大滚动偏移（0 表示内容不溢出）。
    pub fn max_offset(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport)
    }
}

/// 正文窗滚动条上一帧的几何：拖拽与点击都按它换算滚动位置。
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextScrollbar {
    /// 轨道区域（屏幕坐标，一列宽）。
    pub track: Rect,
    /// 滑块行数。
    pub thumb: u16,
    /// 上一帧的最大滚动偏移。
    pub max_offset: usize,
}

impl ContextScrollbar {
    /// 滑块顶端相对轨道顶端的行数；内容不溢出时贴轨道顶端。
    pub fn thumb_top(&self, scroll: usize) -> u16 {
        let travel = self.track.height.saturating_sub(self.thumb);
        if self.max_offset == 0 || travel == 0 {
            return 0;
        }
        let position = scroll.min(self.max_offset) * usize::from(travel) / self.max_offset;
        position as u16
    }

    /// 光标行对应的滚动偏移；`grab` 是拖拽起点到滑块顶端的行差。
    ///
    /// 轨道不足一屏（`max_offset == 0`）时永远回到顶端。
    pub fn offset_at(&self, row: u16, grab: i32) -> usize {
        let travel = i32::from(self.track.height.saturating_sub(self.thumb));
        if travel <= 0 || self.max_offset == 0 {
            return 0;
        }
        let target =
            (i32::from(row).saturating_sub(i32::from(self.track.y)) - grab).clamp(0, travel);
        (target as usize * self.max_offset) / travel as usize
    }
}

/// 正文窗的滚动条拖拽状态。
#[derive(Debug, Clone, Copy)]
pub struct ContextDrag {
    /// 按下时光标到滑块顶端的行差（点空白处时取滑块中线）。
    pub grab: i32,
}

/// 上下文正文窗：分块正文、折叠状态与滚动位置。
#[derive(Debug, Default)]
pub struct ContextContentState {
    /// 拉取结果；`None` = 还在读。
    pub content: Option<ContextContentResult>,
    /// 读取失败的原因。
    pub error: Option<String>,
    /// 每块的展开状态，与 `sections` 等长；短块展不展开都全文显示。
    pub expanded: Vec<bool>,
    /// 键盘焦点块；`None` = 未选中（Tab 或点击后出现高亮）。
    pub focus: Option<usize>,
    /// 视口顶端行号。
    pub scroll: usize,
    /// 上一帧的正文视口行数（滚动夹取与命中判定用）。
    pub viewport: usize,
    /// 渲染缓存；分块、展开状态或列宽变化时丢弃重建。
    pub layout: Option<ContextLayout>,
    /// 滚动条几何；上一帧绘制时更新。
    pub scrollbar: ContextScrollbar,
    /// 滚动条拖拽状态；`Some` 时鼠标横向移动都被它吃掉。
    pub drag: Option<ContextDrag>,
}

impl ContextContentState {
    /// 装载结果：全部折叠、焦点清空、视口回到顶部。
    pub fn set_content(&mut self, content: ContextContentResult) {
        self.expanded = vec![false; content.sections.len()];
        self.content = Some(content);
        self.error = None;
        self.focus = None;
        self.scroll = 0;
        self.layout = None;
    }

    /// 装上失败原因。
    pub fn set_error(&mut self, error: String) {
        self.error = Some(error);
        self.content = None;
        self.layout = None;
    }

    /// 分块列表；还没读到结果时为空。
    pub fn sections(&self) -> &[ContextContentSection] {
        self.content
            .as_ref()
            .map_or(&[], |content| content.sections.as_slice())
    }

    /// 当前分块数。
    pub fn count(&self) -> usize {
        self.sections().len()
    }

    /// 切换第 `index` 块的展开状态。
    pub fn toggle(&mut self, index: usize) {
        let Some(flag) = self.expanded.get_mut(index) else {
            return;
        };
        *flag = !*flag;
        self.layout = None;
    }

    /// 全部展开或全部收起。
    pub fn set_all_expanded(&mut self, expanded: bool) {
        self.expanded.fill(expanded);
        self.layout = None;
    }

    /// 滚动 `delta` 行（正数向下）；按上一帧的缓存夹住范围。
    pub fn scroll_by(&mut self, delta: i64) {
        let Some(layout) = &self.layout else {
            return;
        };
        let max = layout.max_offset(self.viewport);
        self.scroll = if delta < 0 {
            self.scroll.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.scroll.saturating_add(delta as usize)
        }
        .min(max);
    }

    /// 跳到顶部或底部。
    pub fn scroll_to_edge(&mut self, bottom: bool) {
        let Some(layout) = &self.layout else {
            return;
        };
        self.scroll = if bottom {
            layout.max_offset(self.viewport)
        } else {
            0
        };
    }

    /// 焦点移到相邻块，并把该块滚进视口；未选中时从首块开始。
    pub fn focus_step(&mut self, delta: isize) {
        let count = self.count();
        if count == 0 {
            return;
        }
        let current = match self.focus {
            None => 0,
            Some(index) if delta < 0 => index.saturating_sub(delta.unsigned_abs()),
            Some(index) => index.saturating_add(delta as usize).min(count - 1),
        };
        self.focus = Some(current);
        self.reveal_focused();
    }

    /// 把焦点块滚进视口（块比视口高时对齐「更近」的那一端）。
    pub fn reveal_focused(&mut self) {
        let Some(index) = self.focus else {
            return;
        };
        let Some(layout) = &self.layout else {
            return;
        };
        let Some(span) = layout.spans.get(index) else {
            return;
        };
        let viewport = self.viewport;
        let max = layout.max_offset(viewport);
        if span.start < self.scroll {
            self.scroll = span.start.min(max);
        } else if span.end > self.scroll + viewport {
            self.scroll = span.end.saturating_sub(viewport).min(max);
        }
    }

    /// 命中屏幕行所在的分块；正文之外返回 `None`。
    pub fn block_at(&self, row: u16) -> Option<usize> {
        let track = self.scrollbar.track;
        if row < track.y || row >= track.y.saturating_add(track.height) {
            return None;
        }
        let line = self.scroll + usize::from(row - track.y);
        let layout = self.layout.as_ref()?;
        layout
            .spans
            .iter()
            .position(|span| line >= span.start && line < span.end)
    }
}

/// 会话恢复列表的范围：默认只看当前目录，`Tab` 切到全部目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionScope {
    #[default]
    Folder,
    All,
}

impl SessionScope {
    /// 在「当前目录」与「全部目录」之间切换。
    pub fn toggled(self) -> Self {
        match self {
            Self::Folder => Self::All,
            Self::All => Self::Folder,
        }
    }
}

/// 打开的模态。
pub enum Modal {
    /// 工具授权：展示 diff 预览（若有）与选项列表。
    Permission {
        params: PermissionRequestParams,
        selected: usize,
        scroll_offset: usize,
        reply: Option<tokio::sync::oneshot::Sender<Result<PermissionResult, RpcErrorOf>>>,
    },
    /// `ask` 工具的用户提问（可多问、可多选、可自定义输入）。
    Ask {
        params: AskParams,
        question: usize,
        /// 每问当前高亮的选项下标（== options.len() 表示自定义输入行）。
        highlight: Vec<usize>,
        /// 每问已选中的 option label 集合。
        selected: Vec<Vec<String>>,
        /// 每问的自定义答案输入，与 `selected` 一一对应（切问不串答案）。
        custom: Vec<CustomAnswer>,
        /// 当前聚焦项预览的滚动行数（切换选项/问题时归零）。
        preview_scroll: usize,
        /// 键盘停泊：卡片保留，按键交给转写区滚动，供用户查阅提问的上下文。
        parked: bool,
        reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, RpcErrorOf>>>,
    },
    /// 模型选择（`session/set_model`）。
    Model {
        entries: Vec<ModelEntry>,
        current: String,
        picker: PickerState,
        level: LevelState,
        target: ModelTarget,
    },
    /// 消息回退选择器。
    Rewind { state: RewindState },
    /// 会话恢复（`session/list` + `session/load`）。
    Sessions {
        sessions: Vec<SessionInfo>,
        /// 当前目录；`SessionScope::Folder` 只列与它一致的会话。
        cwd: String,
        scope: SessionScope,
        picker: PickerState,
    },
    /// 当前会话的子代理列表。
    Subagents {
        agents: Vec<moc_protocol::methods::AgentInfo>,
        picker: PickerState,
    },
    /// 计划审批（`session/plan_approve`）。
    Plan { state: PlanReviewState },
    /// 快捷键帮助。
    Help,
    /// 上下文组成估算详情。
    Context { usage: Option<ContextUsageResult> },
    /// 上下文正文：分块展示真正送进模型的内容。
    ContextContent { state: ContextContentState },
    /// 切换模型需要压缩上下文时的确认卡片。
    SwitchModel {
        /// 发起切换的会话；卡片期间该会话视为忙，队列不派发。
        session_id: String,
        selector: String,
        overflow: ModelSwitchOverflow,
        /// 0 = 切换并压缩，1 = 取消。
        option: usize,
        loading: bool,
        error: Option<String>,
    },
}
/// 回复通道的错误类型别名（协议层 RpcError 的重导出位置）。
type RpcErrorOf = moc_protocol::RpcError;

/// 一问的自定义答案输入状态。
#[derive(Debug, Clone, Default)]
pub struct CustomAnswer {
    /// 已输入正文。
    pub text: String,
    /// 光标（字节偏移，恒在字符边界）。
    pub cursor: usize,
    /// 是否处于编辑态（编辑态下按键都归输入行）。
    pub editing: bool,
}

impl Modal {
    /// 标题行。
    pub fn title(&self) -> String {
        match self {
            Modal::Permission { params, .. } => params.title.clone(),
            Modal::Ask {
                params,
                question,
                parked,
                ..
            } => {
                let title = params
                    .questions
                    .get(*question)
                    .and_then(|question| question.header.clone())
                    .unwrap_or_else(|| "提问".into());
                if *parked {
                    format!("{title} · 稍后回答")
                } else {
                    title
                }
            }
            Modal::Model { .. } => "选择模型".into(),
            Modal::Rewind { .. } => "消息回退".into(),
            Modal::Sessions { scope, .. } => match scope {
                SessionScope::Folder => "恢复会话（当前目录）".into(),
                SessionScope::All => "恢复会话（全部目录）".into(),
            },
            Modal::Subagents { .. } => "子代理".into(),
            Modal::Plan { state } => format!("计划审批：{}", state.title),
            Modal::Help => "快捷键帮助".into(),
            Modal::Context { .. } => "上下文占用".into(),
            Modal::ContextContent { .. } => "上下文内容".into(),
            Modal::SwitchModel { .. } => "切换模型：需要压缩上下文".into(),
        }
    }

    /// 选项行数（键盘循环的范围）。
    pub fn option_count(&self) -> usize {
        match self {
            Modal::Permission { params, .. } => params.options.len(),
            Modal::Ask {
                params, question, ..
            } => params
                .questions
                .get(*question)
                .map(|question| question.options.len() + 1)
                .unwrap_or(0),
            Modal::Model {
                picker, entries, ..
            } => filtered_entries(entries, &picker.query).len(),
            Modal::Rewind { .. } => 0,
            Modal::Sessions {
                picker,
                sessions,
                cwd,
                scope,
            } => filtered_sessions(sessions, &picker.query, *scope, cwd).len(),
            Modal::Subagents { picker, agents } => agents
                .iter()
                .filter(|agent| {
                    let query = picker.query.to_lowercase();
                    agent.title.to_lowercase().contains(&query) || agent.id.contains(&query)
                })
                .count(),
            Modal::Plan { .. } => 4,
            Modal::Help | Modal::Context { .. } | Modal::ContextContent { .. } => 0,
            Modal::SwitchModel { .. } => 2,
        }
    }

    /// 当前提问是否有选项带预览（为真时卡片放宽，容纳并排的预览栏）。
    pub fn has_option_preview(&self) -> bool {
        match self {
            Modal::Ask {
                params, question, ..
            } => params.questions.get(*question).is_some_and(|question| {
                question.options.iter().any(|option| {
                    option
                        .preview
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                })
            }),
            _ => false,
        }
    }

    /// 移动选中项（循环）。
    pub fn move_selection(&mut self, delta: i32) {
        let count = self.option_count();
        if count == 0 {
            return;
        }
        match self {
            Modal::Permission { selected, .. } => {
                *selected = (*selected as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::Plan { state } => {
                state.selected = state
                    .selected
                    .saturating_add_signed(isize::try_from(delta).expect("方向增量可表示为 isize"))
                    .min(count - 1);
            }
            Modal::Ask { .. }
            | Modal::Rewind { .. }
            | Modal::Help
            | Modal::Context { .. }
            | Modal::ContextContent { .. } => {}
            Modal::SwitchModel { option, .. } => {
                *option = (*option as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::Model { picker, .. }
            | Modal::Sessions { picker, .. }
            | Modal::Subagents { picker, .. } => {
                picker.move_selection(delta, count, 8);
            }
        }
    }

    pub fn picker_mut(&mut self) -> Option<&mut PickerState> {
        match self {
            Modal::Model { picker, .. } | Modal::Sessions { picker, .. } => Some(picker),
            _ => None,
        }
    }

    /// 模型选择器过滤后列表里当前高亮的条目。
    pub fn highlighted_entry(&self) -> Option<&ModelEntry> {
        let Modal::Model {
            entries, picker, ..
        } = self
        else {
            return None;
        };
        filtered_entries(entries, &picker.query)
            .get(picker.selected)
            .map(|(_, entry)| *entry)
    }

    /// 高亮条目的最终 selector（已含思考等级）；伪值原样返回。
    pub fn model_choice(&self) -> Option<(&ModelEntry, String)> {
        let entry = self.highlighted_entry()?;
        let Modal::Model { level, .. } = self else {
            return None;
        };
        let level = level.resolve(&entry.levels);
        Some((entry, entry.selector_with(level.as_deref())))
    }

    /// 把错误交给当前模态；模态没有显示位置（帮助、上下文占用等）时返回 `false`，
    /// 由调用方改成通知，避免错误被静默丢弃。
    pub fn set_error(&mut self, message: &str) -> bool {
        match self {
            Modal::SwitchModel { loading, error, .. } => {
                *loading = false;
                *error = Some(message.to_owned());
                true
            }
            Modal::ContextContent { state } => {
                state.set_error(message.to_owned());
                true
            }
            _ => {
                if let Some(picker) = self.picker_mut() {
                    picker.loading = false;
                    picker.error = Some(message.to_owned());
                    true
                } else {
                    false
                }
            }
        }
    }
}

pub fn filtered_entries<'a>(
    entries: &'a [ModelEntry],
    query: &str,
) -> Vec<(usize, &'a ModelEntry)> {
    let query = query.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            query.is_empty()
                || entry.label.to_lowercase().contains(&query)
                || entry.display_name.to_lowercase().contains(&query)
        })
        .collect()
}

pub fn filtered_sessions<'a>(
    sessions: &'a [SessionInfo],
    query: &str,
    scope: SessionScope,
    cwd: &str,
) -> Vec<(usize, &'a SessionInfo)> {
    let query = query.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| scope == SessionScope::All || session.cwd == cwd)
        .filter(|(_, session)| {
            query.is_empty()
                || session.title.to_lowercase().contains(&query)
                || session.cwd.to_lowercase().contains(&query)
        })
        .collect()
}

/// 分叉会话的来源标注：优先用来源会话的标题，来源已从列表消失时退回短 id。
pub fn fork_source_label<'a>(
    sessions: &'a [SessionInfo],
    session: &'a SessionInfo,
) -> Option<&'a str> {
    let source = session.forked_from.as_deref()?;
    Some(
        sessions
            .iter()
            .find(|candidate| candidate.id == source)
            .map_or_else(
                || short_session_id(source),
                |candidate| candidate.title.as_str(),
            ),
    )
}

/// 会话 id 的展示短形式；短于约定长度时原样返回。
pub fn short_session_id(id: &str) -> &str {
    id.get(..16).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::{
        CustomAnswer, LevelState, Modal, ModelEntry, PickerState, RewindState, filtered_entries,
    };

    use moc_protocol::methods::{AskOption, AskParams, AskQuestion, TranscriptEntry};
    use moc_protocol::{Message, Role};

    /// 等级默认用候选里最高的那个；手动值仍在候选内时优先，掉了就回退最高。
    #[test]
    fn level_state_defaults_to_highest_and_keeps_supported_manual_choice() {
        let levels = |list: &[&str]| {
            list.iter()
                .map(|level| (*level).to_owned())
                .collect::<Vec<_>>()
        };
        let full = levels(&["off", "low", "medium", "high", "xhigh", "max"]);
        let narrow = levels(&["off", "low"]);

        let mut state = LevelState::default();
        assert_eq!(state.resolve(&full).as_deref(), Some("max"));
        assert!(state.resolve(&[]).is_none(), "没有候选就没有等级");

        // 手动选的等级在候选内就沿用：切到同样支持 max 的模型不会被降级。
        state.manual = Some("max".into());
        assert_eq!(state.resolve(&full).as_deref(), Some("max"));
        // 目标模型装不下手动值时回退到它自己的最高等级。
        assert_eq!(state.resolve(&narrow).as_deref(), Some("low"));

        // 左右移动按候选顺序环绕，并把结果记为手动值。
        state.manual = None;
        state.move_level(&narrow, true);
        assert_eq!(
            state.manual.as_deref(),
            Some("off"),
            "最高等级再右移回到 off"
        );
        state.move_level(&narrow, false);
        assert_eq!(state.manual.as_deref(), Some("low"), "左移按顺序回退");
        state.move_level(&[], true);
        assert_eq!(state.manual.as_deref(), Some("low"), "空候选不动手动值");

        // 高亮换行清掉手动值，回到「用最高等级」。
        state.clear_manual();
        assert_eq!(state.resolve(&narrow).as_deref(), Some("low"));
    }

    /// 写回的 selector：模型带等级后缀，角色伪值原样。
    #[test]
    fn model_entry_appends_level_only_for_models() {
        let model = ModelEntry {
            selector: "p/m".into(),
            label: "p/m".into(),
            display_name: "模型".into(),
            levels: vec!["off".into(), "high".into()],
            context_window: Some(1000),
            max_output_tokens: Some(512),
            inputs: vec![moc_protocol::methods::ModelInput::Text],
            remote_compaction: false,
            pricing: None,
        };
        assert_eq!(model.selector_with(Some("high")), "p/m:high");
        assert_eq!(model.selector_with(None), "p/m");

        let inherit = ModelEntry {
            selector: "inherit".into(),
            label: "沿用主模型".into(),
            display_name: String::new(),
            levels: Vec::new(),
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            remote_compaction: false,
            pricing: None,
        };
        assert_eq!(
            inherit.selector_with(Some("max")),
            "inherit",
            "伪值不接等级"
        );
    }

    /// 过滤同时匹配选择串与展示名，伪值也能被中文标签搜到。
    #[test]
    fn filtering_matches_selector_display_name_and_label() {
        let entries = vec![
            ModelEntry {
                selector: "inherit".into(),
                label: "沿用主模型".into(),
                display_name: String::new(),
                levels: Vec::new(),
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                remote_compaction: false,
                pricing: None,
            },
            ModelEntry {
                selector: "p/model".into(),
                label: "p/model".into(),
                display_name: "DeepSeek Flash".into(),
                levels: vec!["off".into()],
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                remote_compaction: false,
                pricing: None,
            },
        ];
        let found = |query: &str| {
            filtered_entries(&entries, query)
                .into_iter()
                .map(|(_, entry)| entry.selector.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(found(""), vec!["inherit", "p/model"], "空查询不过滤");
        assert_eq!(found("model"), vec!["p/model"]);
        assert_eq!(found("deepseek"), vec!["p/model"], "展示名参与匹配");
        assert_eq!(found("主模型"), vec!["inherit"], "伪值按标签匹配");
    }

    /// 预览判定：只认非空预览，且只作用于提问模态的当前问题。
    #[test]
    fn option_preview_detection_follows_current_question() {
        let question = |id: &str, previews: [Option<&str>; 2]| AskQuestion {
            id: id.into(),
            question: "选哪个？".into(),
            header: None,
            multi: false,
            recommended: Vec::new(),
            options: previews
                .into_iter()
                .enumerate()
                .map(|(index, preview)| AskOption {
                    label: format!("选项{index}"),
                    description: None,
                    preview: preview.map(str::to_owned),
                })
                .collect(),
        };
        let modal = |questions: Vec<AskQuestion>, question: usize| Modal::Ask {
            params: AskParams {
                session_id: "ses".into(),
                prompt_id: "prompt".into(),
                request_id: "ask".into(),
                questions,
            },
            question,
            highlight: vec![0; 2],
            selected: vec![Vec::new(); 2],
            custom: vec![CustomAnswer::default(); 2],
            preview_scroll: 0,
            parked: false,
            reply: None,
        };

        let questions = vec![
            question("first", [None, Some("   ")]),
            question("second", [None, Some("```\n+--+\n```")]),
        ];
        assert!(!modal(questions.clone(), 0).has_option_preview());
        assert!(modal(questions, 1).has_option_preview());
    }

    #[test]
    fn picker_keeps_selected_item_inside_viewport() {
        let mut picker = PickerState {
            selected: 0,
            offset: 0,
            ..Default::default()
        };
        picker.move_selection(9, 20, 8);
        assert_eq!(picker.selected, 9);
        assert_eq!(picker.offset, 2);
        picker.move_selection(-9, 20, 8);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.offset, 0);
    }
    #[test]
    fn rewind_navigation_moves_within_turns_and_between_users() {
        let entry = |id: &str, message: Message| TranscriptEntry {
            id: id.into(),
            at_ms: 0,
            message,
            tool: None,
            bash: None,
            agent: None,
        };
        let mut state = RewindState::default();
        state.set_entries(vec![
            entry("u1", Message::user_text("第一条")),
            entry("a1", Message::assistant_text("回复")),
            entry("t1", Message::tool_result("call", "结果", false)),
            entry("u2", Message::user_text("第二条")),
        ]);
        assert_eq!(state.prompts.len(), 2);
        assert_eq!(state.prompts[0].len(), 3);
        assert_eq!(state.selected_prompt, 1);
        assert_eq!(state.selected_item, 0);

        state.move_horizontal(-1);
        state.move_vertical(2);
        assert_eq!((state.selected_prompt, state.selected_item), (0, 2));
        state.move_horizontal(1);
        assert_eq!((state.selected_prompt, state.selected_item), (1, 0));
        assert_eq!(state.selected().expect("选中条目").message.role, Role::User);
    }

    #[test]
    fn test_rewind_omp_style_navigation_and_filtering() {
        let entry = |id: &str, message: Message| TranscriptEntry {
            id: id.into(),
            at_ms: 1700000000000,
            message,
            tool: None,
            bash: None,
            agent: None,
        };
        let mut state = RewindState::default();
        state.set_entries(vec![
            entry("u1", Message::user_text("帮我实现一个快排")),
            entry(
                "a1",
                Message::assistant_text("好的，这是快速排序（快排）实现"),
            ),
            entry("u2", Message::user_text("加几个单元测试")),
            entry("a2", Message::assistant_text("测试已增加并全部通过")),
            entry("u3", Message::user_text("改成双轴快排")),
        ]);

        // Total 5 items in timeline
        assert_eq!(state.items.len(), 5);
        assert_eq!(state.selected, 4); // Default to last item
        assert_eq!(state.selected_item_ref().unwrap().preview, "改成双轴快排");

        // 1. Up / Down linear stepping (↑↓ / jk)
        state.step(-1);
        assert_eq!(state.selected, 3);
        assert!(!state.selected_item_ref().unwrap().is_user);

        state.step(-1);
        assert_eq!(state.selected, 2);
        assert!(state.selected_item_ref().unwrap().is_user);

        // 2. Jump between User turns (←→ / hl)
        state.jump_user(-1);
        assert_eq!(state.selected, 0); // Jumps to u1
        assert_eq!(state.selected_item_ref().unwrap().entry_id, "u1");

        state.jump_user(1);
        assert_eq!(state.selected, 2); // Jumps to u2
        assert_eq!(state.selected_item_ref().unwrap().entry_id, "u2");

        state.jump_user(1);
        assert_eq!(state.selected, 4); // Jumps to u3

        // 3. Search and filtering mode
        state.start_filter();
        state.input_char('快');
        state.input_char('排');
        let matches = state.filtered_indices();
        // u1 ("快排"), a1 ("快速排序"), u3 ("双轴快排")
        assert_eq!(matches.len(), 3);
        assert_eq!(matches, vec![0, 1, 4]);

        // Stepping within filtered matches
        state.selected = 4;
        state.step(-1);
        assert_eq!(state.selected, 1);
        state.step(-1);
        assert_eq!(state.selected, 0);

        // Clear filter
        state.clear_filter();
        assert!(!state.filtering);
        assert!(state.query.is_empty());
        assert_eq!(state.filtered_indices().len(), 5);

        // 4. Detail scroll
        assert_eq!(state.detail_scroll, 0);
        state.scroll_detail(3);
        assert_eq!(state.detail_scroll, 3);
        state.scroll_detail(-2);
        assert_eq!(state.detail_scroll, 1);
    }

    /// 错误只能落在有显示位置的模态上；其余模态返回 `false` 由调用方回落到通知。
    #[test]
    fn set_error_reports_when_modal_cannot_show_errors() {
        let overflow = moc_protocol::methods::ModelSwitchOverflow {
            tokens: 3_000,
            context_window: 1_000,
            threshold: 850,
            target_tokens: 680,
        };
        let mut card = Modal::SwitchModel {
            session_id: "ses".into(),
            selector: "p/tiny".into(),
            overflow,
            option: 0,
            loading: true,
            error: None,
        };
        assert!(card.set_error("切换模型失败：断开"));
        assert!(matches!(
            &card,
            Modal::SwitchModel {
                loading: false,
                error: Some(error),
                ..
            } if error == "切换模型失败：断开"
        ));

        let mut sessions = Modal::Sessions {
            sessions: Vec::new(),
            cwd: "/tmp".into(),
            scope: super::SessionScope::Folder,
            picker: PickerState {
                loading: true,
                ..Default::default()
            },
        };
        assert!(sessions.set_error("读取失败"));
        assert!(matches!(
            &sessions,
            Modal::Sessions { picker: PickerState { loading: false, error: Some(error), .. }, .. }
                if error == "读取失败"
        ));

        assert!(!Modal::Help.set_error("没有位置显示"));
        assert!(
            !Modal::Context { usage: None }.set_error("没有位置显示"),
            "上下文占用卡片不承载错误"
        );
    }

    /// 正文窗的滚动、焦点与命中：都按上一帧缓存的几何换算。
    #[test]
    fn context_content_navigation_follows_cached_geometry() {
        use moc_protocol::methods::{ContextContentResult, ContextContentSection};
        use ratatui::layout::Rect;

        let mut state = super::ContextContentState::default();
        state.set_content(ContextContentResult {
            context_window: 500_000,
            total_tokens: 4_000,
            sections: ["intro", "tool policy", "read"]
                .into_iter()
                .map(|name| ContextContentSection {
                    name: name.into(),
                    text: "正文".into(),
                    tokens: 10,
                    truncated_bytes: 0,
                })
                .collect(),
        });
        assert_eq!(state.count(), 3);
        assert_eq!(state.expanded, vec![false; 3]);
        // 视口 10 行、全文 30 行：滚动夹在 [0, 20]。
        state.viewport = 10;
        state.layout = Some(super::ContextLayout {
            width: 40,
            spans: vec![0..10, 10..20, 20..30],
            lines: vec![ratatui::text::Line::default(); 30],
        });
        state.scroll_by(100);
        assert_eq!(state.scroll, 20, "滚到底部");
        state.scroll_by(-5);
        assert_eq!(state.scroll, 15);
        state.scroll_to_edge(false);
        assert_eq!(state.scroll, 0);

        // 焦点从首块开始逐块推进，并把目标块滚进视口。
        state.focus_step(1);
        assert_eq!(state.focus, Some(0));
        state.focus_step(1);
        assert_eq!(state.focus, Some(1));
        state.focus_step(10);
        assert_eq!(state.focus, Some(2), "越界夹在末块");
        assert_eq!(state.scroll, 20, "末块整块滚进视口");
        state.focus_step(-10);
        assert_eq!(state.focus, Some(0));
        assert_eq!(state.scroll, 0);

        // 展开状态切换会作废渲染缓存。
        state.layout = Some(super::ContextLayout {
            width: 40,
            spans: vec![0..1, 1..2],
            lines: vec![ratatui::text::Line::default(); 2],
        });
        state.toggle(1);
        assert_eq!(state.expanded, vec![false, true, false]);
        assert!(state.layout.is_none(), "折叠状态变了必须重排");

        // 命中：轨道覆盖正文区，行号换算成行表里的行。
        state.scrollbar.track = Rect::new(30, 4, 1, 10);
        state.scroll = 5;
        state.layout = Some(super::ContextLayout {
            width: 40,
            spans: vec![0..6, 6..14, 14..20],
            lines: vec![ratatui::text::Line::default(); 20],
        });
        assert_eq!(state.block_at(4), Some(0), "轨道首行 = 视口首行");
        assert_eq!(state.block_at(5), Some(1));
        assert_eq!(state.block_at(3), None, "轨道上方不是正文");
        assert_eq!(state.block_at(14), None, "轨道下方不是正文");
    }

    /// 滚动条几何：滑块位置与拖拽换算互为逆运算。
    #[test]
    fn context_scrollbar_maps_rows_to_offsets() {
        use ratatui::layout::Rect;

        let scrollbar = super::ContextScrollbar {
            track: Rect::new(10, 2, 1, 20),
            thumb: 4,
            max_offset: 100,
        };
        assert_eq!(scrollbar.thumb_top(0), 0, "顶部偏移贴轨道顶端");
        assert_eq!(scrollbar.thumb_top(100), 16, "底部偏移贴轨道末端");
        // 抓取滑块顶端后拖到轨道末端：偏移走满。
        assert_eq!(scrollbar.offset_at(2 + 16, 0), 100);
        assert_eq!(scrollbar.offset_at(2, 0), 0);
        assert_eq!(scrollbar.offset_at(2 + 8, 0), 50);
        // 抓取点偏下时同样的行号落在更小的偏移上。
        assert!(scrollbar.offset_at(2 + 8, 2) < scrollbar.offset_at(2 + 8, 0));
        // 内容不溢出：任何行号都回到顶端。
        let flat = super::ContextScrollbar {
            track: Rect::new(10, 2, 1, 20),
            thumb: 20,
            max_offset: 0,
        };
        assert_eq!(flat.offset_at(20, 3), 0);
        assert_eq!(flat.thumb_top(0), 0);
    }
}
