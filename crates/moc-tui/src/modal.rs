//! 模态卡片：权限、提问、模型选择、会话恢复、计划审批、帮助。
//!
//! 每种模态自带键盘状态（选中项等）与答复通道（oneshot sender）；键盘
//! 事件优先路由给打开的模态（见 `crate::event`），渲染浮层在 `crate::render`。

use std::ops::Range;

use moc_protocol::Role;
use moc_protocol::methods::{
    AskParams, AskResult, ContextContentResult, ContextContentSection, ModelSwitchOverflow,
    PermissionRequestParams, PermissionResult, SessionInfo, TranscriptEntry,
};

use ratatui::layout::Rect;
use ratatui::text::Line;
use rust_i18n::t;

/// 可搜索列表模态的状态。
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    /// 当前过滤字符串。
    pub query: String,
    /// 过滤结果中的选中项下标。
    pub selected: usize,
    /// 列表 viewport 的起点；模型列表按显示行（含分组标题），其余按条目。
    pub offset: usize,
    /// 列表 viewport 的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    /// 保留鼠标坐标，悬停在渲染层按最终命中矩形计算，不直接写 `selected`。
    pub pointer: Option<ratatui::layout::Position>,
    /// 是否正在请求后端数据。
    pub loading: bool,
    /// 最近一次请求错误。
    pub error: Option<String>,
}

/// 选择器里的一行：真实模型，或模型角色字段的伪值（沿用父会话当前模型 / 沿用当前会话模型）。
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// 写回配置或会话的 selector，不含思考等级；伪值就是 `inherit` / `none`。
    pub selector: String,
    /// 展示名：模型是 `供应商/模型`，伪值是中文标签。
    pub label: String,
    /// 模型展示名，同时参与搜索；伪值为空。
    pub display_name: String,
    /// 该模型允许选择的思考等级；伪值没有等级，这里是空列表。
    pub levels: Vec<String>,
    /// 上下文窗口 token 数；未配置或伪值为 `None`。
    pub context_window: Option<u64>,
    /// 单次输出上限 token 数；未配置或伪值为 `None`。
    pub max_output_tokens: Option<u32>,
    /// 接受的输入模态；伪值为空。
    pub inputs: Vec<moc_protocol::methods::ModelInput>,
    /// 每百万 token 定价；未配置为 `None`。
    pub pricing: Option<moc_protocol::methods::ModelPricing>,
    /// 模型声明支持 Fast Mode（`fast_mode = true`）；伪值与 Fusion 特殊项为 `false`。
    pub fast_mode: bool,
}

impl ModelEntry {
    /// 优先展示配置的名称；未配置或等于 selector 时去掉供应商前缀。
    pub fn name(&self) -> &str {
        if self.selector == FUSION_ENTRY_SELECTOR {
            "Fusion"
        } else if self.display_name.is_empty() || self.display_name == self.selector {
            self.label
                .split_once('/')
                .map_or(&self.label, |(_, name)| name)
        } else {
            &self.display_name
        }
    }

    pub fn provider(&self) -> &str {
        self.selector
            .split_once('/')
            .map_or("", |(provider, _)| provider)
    }

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

mod models;
pub use models::default_level;
pub use models::detail_rows;
pub use models::filtered_entries;
pub use models::fusion_selector;
pub use models::fusion_side_level;
pub use models::fusion_side_pick;
pub use models::menu_items;
pub use models::parse_fusion;
pub use models::side_entry;
pub use models::side_level;
mod sessions;
pub use sessions::filtered_sessions;
pub use sessions::fork_source_label;
pub use sessions::short_session_id;

/// 思考等级条的键盘状态；本次打开期间按 selector 保留手动选择。
#[derive(Debug, Clone, Default)]
pub struct LevelState {
    pub manual: Option<String>,
    pub remembered: std::collections::HashMap<String, String>,
}

impl LevelState {
    /// 离开前保存手动等级，回来时恢复；未调整的模型仍使用原默认策略。
    pub fn change_entry(&mut self, before: Option<&ModelEntry>, after: Option<&ModelEntry>) {
        if before.map(|entry| &entry.selector) == after.map(|entry| &entry.selector) {
            return;
        }
        if let (Some(entry), Some(level)) = (before, self.manual.take()) {
            self.remembered.insert(entry.selector.clone(), level);
        }
        self.manual = after.and_then(|entry| self.remembered.get(&entry.selector).cloned());
    }

    /// 当前生效的等级：手动值仍在候选内就用它，否则用候选里最高的等级。
    pub fn resolve(&self, levels: &[String]) -> Option<String> {
        if let Some(manual) = self.manual.as_ref()
            && levels.contains(manual)
        {
            return Some(manual.clone());
        }
        default_level(levels)
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
    /// 「在新会话执行」的目标引擎；默认沿用当前会话引擎。
    pub execution_engine: moc_protocol::SessionEngine,
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

mod plan;

impl PlanReviewState {}

/// 按字符数截断，避免把多字节字符切断。
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

/// 模型选择器的写入目标。
pub enum ModelTarget {
    Session,
    /// `/config` 的模型角色字段：选中后写回该路径的配置值。
    Config {
        path: Vec<String>,
        /// 字段标签，用作改动提示的前缀。
        label: String,
    },
    /// workflow role 的模型选择：来源不同回写不同——审批窗写覆盖表，
    /// run 面板调用 `session/workflow_set_role_model`。
    WorkflowRole {
        /// 目标标识：role_id 或 model() ref_id。
        key: crate::workflow::FocusTarget,
        /// 选择后还原的 modal 状态（装箱收窄：审批状态比常规 target 大）。
        back: Box<crate::workflow::WorkflowRoleBack>,
    },
}

/// Fusion 组合在详情卡内的编辑草稿：两侧各存一份带等级的 selector。
#[derive(Debug, Clone, Default)]
pub struct FusionDraft {
    pub lead: String,
    pub sidekick: String,
}

/// Fusion 选择器在模型选择器里的特殊条目 selector。
pub const FUSION_ENTRY_SELECTOR: &str = "fusion";

/// 详情卡的控制行（渲染顺序即定义顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailRow {
    Level,
    Fast,
    Lead,
    LeadLevel,
    Sidekick,
    SidekickLevel,
    Apply,
}

/// 模型列表与下拉的滚动约束；真实可见行数由绘制层回写。
#[derive(Debug, Clone)]
pub struct ModelScroll {
    pub max_offset: usize,
    /// 打开、键盘导航或搜索后显露选中项；普通重绘不拉回手动滚动的位置。
    pub reveal_selection: bool,
}

impl Default for ModelScroll {
    fn default() -> Self {
        Self {
            max_offset: 0,
            reveal_selection: true,
        }
    }
}

impl ModelScroll {
    pub fn scroll_by(&mut self, offset: &mut usize, delta: isize) {
        *offset = offset.saturating_add_signed(delta).min(self.max_offset);
        self.reveal_selection = false;
    }
}

/// 模型选择器的交互状态：鼠标位置、列表滚动、详情焦点和下拉。
#[derive(Debug, Clone, Default)]
pub struct DetailState {
    /// 保留坐标，在最终布局上计算悬停，避免滚动后沿用旧条目下标。
    pub pointer: Option<ratatui::layout::Position>,
    pub list_scroll: ModelScroll,
    /// `None` 时焦点在候选列表；`Some` 时焦点在详情卡这一行。
    pub focus: Option<DetailRow>,
    /// Fusion 组合草稿（两侧都是带等级的 selector，只有 Session 目标会用到）。
    pub fusion: FusionDraft,
    /// 展开的下拉；`row` 是它挂靠的详情行。
    pub menu: Option<MenuState>,
    /// 选择器内联切换中的目标 selector；回执落地后写回 `current` 且不关闭选择器。
    pub pending: Option<String>,
}

/// 详情行上展开的下拉状态。
#[derive(Debug, Clone)]
pub struct MenuState {
    pub row: DetailRow,
    pub selected: usize,
    pub offset: usize,
    pub scroll: ModelScroll,
    /// 模型下拉的过滤字符串；等级下拉不使用。
    pub query: String,
}

/// 下拉一次最多可见的候选行数。
pub const MENU_PAGE: usize = 8;

impl MenuState {
    /// 在候选间移动（夹紧不循环），绘制时按实际高度显露选中项。
    pub fn move_selection(&mut self, delta: i32, count: usize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta as isize)
            .min(count.saturating_sub(1));
        self.scroll.reveal_selection = true;
    }

    pub fn keep_visible(&mut self, count: usize) {
        self.selected = self.selected.min(count.saturating_sub(1));
        self.scroll.reveal_selection = true;
    }
}

/// 详情行下拉的内容：等级行给该侧模型的全部档位与当前值下标，
/// 模型行给过滤后的候选（与列表同一套模糊匹配，去掉 Fusion 项与伪值）。
pub enum MenuItems<'a> {
    Levels {
        levels: &'a [String],
        current: Option<usize>,
    },
    Models {
        entries: Vec<(usize, &'a ModelEntry)>,
        current: Option<usize>,
    },
}

impl MenuItems<'_> {
    pub fn len(&self) -> usize {
        match self {
            MenuItems::Levels { levels, .. } => levels.len(),
            MenuItems::Models { entries, .. } => entries.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 当前生效值在候选里的下标；菜单打开时 `selected` 指向它。
    pub fn current(&self) -> Option<usize> {
        match self {
            MenuItems::Levels { current, .. } | MenuItems::Models { current, .. } => *current,
        }
    }
}

impl PickerState {
    pub fn reset(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.offset = 0;
        self.error = None;
    }

    pub fn move_selection(&mut self, delta: i32, count: usize, page: usize) {
        move_index(&mut self.selected, &mut self.offset, delta, count, page);
        self.scroll.reveal_selection = true;
    }

    pub fn keep_visible(&mut self, page: usize, count: usize) {
        keep_index_visible(&mut self.selected, &mut self.offset, page, count);
        self.scroll.reveal_selection = true;
    }
}

/// 选择/下拉共用的移动语义：夹紧不循环，并把选中项收进 `page` 行可见窗。
fn move_index(selected: &mut usize, offset: &mut usize, delta: i32, count: usize, page: usize) {
    if count == 0 {
        *selected = 0;
        *offset = 0;
        return;
    }
    let max = count - 1;
    *selected = if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        selected.saturating_add(delta as usize).min(max)
    };
    keep_index_visible(selected, offset, page.max(1), count);
}

fn keep_index_visible(selected: &mut usize, offset: &mut usize, page: usize, count: usize) {
    if count == 0 {
        *offset = 0;
        *selected = 0;
        return;
    }
    *selected = (*selected).min(count - 1);
    if *selected < *offset {
        *offset = *selected;
    } else if *selected >= *offset + page {
        *offset = *selected + 1 - page;
    }
    *offset = (*offset).min(count.saturating_sub(page));
}
/// 单个回退目标条目（Turn / Entry）
#[derive(Debug, Clone)]
pub struct RewindItem {
    pub entry_id: String,
    pub role: Role,
    pub is_user: bool,
    /// 已回退掉的条目：选中它执行的是前滚（redo），不是回退。
    pub future: bool,
    pub at_ms: u64,
    pub preview: String,
    pub full_text: String,
    pub tool_calls: Vec<String>,
    pub prompt_index: usize,
    pub item_index: usize,
    pub entry: TranscriptEntry,
}

impl RewindItem {
    pub fn role_label(&self) -> std::borrow::Cow<'static, str> {
        match self.role {
            Role::User => t!("tui.role.user"),
            Role::Assistant => t!("tui.role.assistant"),
            Role::Tool => t!("tui.role.tool"),
            Role::Custom => t!("tui.role.reminder"),
            Role::System | Role::Developer => t!("tui.role.system"),
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
    /// `items` 中第一条已回退（可前滚）条目的下标；没有已回退条目时等于 `items.len()`。
    pub future_start: usize,
    /// 当前在 items 中的选中项下标
    pub selected: usize,
    /// 是否处于搜索过滤态
    pub filtering: bool,
    /// 搜索过滤关键词
    pub query: String,
    /// 时间线列表 viewport 的起点（已过滤结果中的下标）
    pub list_scroll: usize,
    /// 时间线 viewport 的滚动约束；真实可见行数由绘制层回写。
    pub scroll: ModelScroll,
    /// 保留鼠标坐标，悬停在渲染层按最终布局计算，不直接写 `selected`。
    pub pointer: Option<ratatui::layout::Position>,
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
            future_start: 0,
            selected: 0,
            filtering: false,
            query: String::new(),
            list_scroll: 0,
            scroll: ModelScroll::default(),
            pointer: None,
            detail_scroll: 0,
        }
    }
}

mod rewind;

impl RewindState {}

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

impl ContextLayout {}

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

impl ContextScrollbar {}

/// 正文窗的滚动条拖拽状态。
#[derive(Debug, Clone, Copy)]
pub struct ContextDrag {
    /// 按下时光标到滑块顶端的行差（点空白处时取滑块中线）。
    pub grab: i32,
}

/// 上下文快照：总览、来源目录与正文共用一条导航栈。
#[derive(Debug, Default)]
pub struct ContextContentState {
    /// 拉取结果；`None` = 还在读。
    pub content: Option<ContextContentResult>,
    /// 读取失败的原因。
    pub error: Option<String>,
    pub current: Option<ContextContentSection>,
    pub navigation: Vec<(Option<ContextContentSection>, usize, Option<usize>, bool)>,
    pub stale: bool,
    pub raw: bool,
    pub copy: bool,
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
    pub request: u64,
    pub session_id: String,
    pub pages: std::collections::HashMap<String, moc_protocol::methods::ContextReadResult>,
    pub pending: std::collections::HashSet<String>,
    pub node_errors: std::collections::HashMap<String, String>,
    pub loads: Vec<(String, Option<u64>)>,
    pub refresh: bool,
    pub cursors: std::collections::HashMap<String, u64>,
    pub previous: std::collections::HashMap<String, Vec<u64>>,
}

mod context;

impl ContextContentState {}

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
        /// 详情区的滚动行数：聚焦项预览（双栏右栏）或聚焦项描述区（单栏底区），
        /// 确认页上复用为答案列表偏移（切换选项/问题时归零）。
        preview_scroll: usize,
        /// 问题正文窗口的滚动行数（问题限高展示时启用；换题归零，动选项光标不归零）。
        question_scroll: usize,
        /// 折叠态：卡片收成输入框位的细条，按键/滚轮交给转写区滚动，
        /// 供用户查阅提问的上下文后再回来作答。
        parked: bool,
        reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, RpcErrorOf>>>,
    },
    /// 模型选择（`session/set_model`）。
    Model {
        entries: Vec<ModelEntry>,
        current: String,
        picker: PickerState,
        level: LevelState,
        /// 详情卡与下拉的交互状态（焦点行、Fusion 草稿、展开的下拉）。
        detail: DetailState,
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
    /// 计划审批（`session/plan_approve`）。装箱收窄 Modal：PlanReviewState 近 600 字节。
    Plan { state: Box<PlanReviewState> },
    /// 快捷键帮助；`scroll` 是正文滚动偏移（渲染时按可见行数收紧）。
    Help { scroll: usize },
    /// 当前会话实际发送的工具定义；请求序号用于丢弃关闭后的迟到结果。
    Tools {
        request: u64,
        tools: Option<Vec<moc_protocol::ToolDef>>,
        error: Option<String>,
        scroll: usize,
    },
    /// 上下文总览与逐层来源阅读。装箱收窄 Modal：ContextContentState 逾 700 字节。
    ContextContent { state: Box<ContextContentState> },
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
    /// 后台任务列表（/jobs）。
    Jobs {
        jobs: Vec<moc_protocol::methods::JobInfo>,
        selected: usize,
        /// 待确认删除的任务索引（需要按 x 两次）。
        pending_kill: Option<usize>,
        /// 列表 viewport 的起点与滚动约束（同 [`PickerState`] 的滚动语义）。
        offset: usize,
        scroll: ModelScroll,
        /// 保留鼠标坐标，悬停在渲染层按最终命中矩形计算。
        pointer: Option<ratatui::layout::Position>,
    },
    /// workflow 审批：phase 分层图 + role 模型覆盖 + 命令集 + 脚本/差异。
    WorkflowApproval {
        state: Box<crate::workflow::WorkflowApprovalState>,
    },
    /// workflow run 列表（/workflows）：选中后打开 run 面板。
    WorkflowRuns {
        runs: Vec<moc_protocol::methods::WorkflowRunSummary>,
        picker: PickerState,
    },
    /// workflow run 面板：实时着色图 + role 模型热改 + reports/artifacts/log/hole/停止。
    WorkflowRun {
        state: Box<crate::workflow::WorkflowRunState>,
    },
    /// 进程资源监控（`/monitor`）：数据在渲染时从 `procstats` 环形缓冲现取，
    /// 模态本身无状态。
    Monitor,
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
                params, question, ..
            } => {
                if *question >= params.questions.len() {
                    // 越过最后一问是确认页，不再是任何一问的标题。
                    t!("tui.title.confirm_answers").into_owned()
                } else {
                    params
                        .questions
                        .get(*question)
                        .and_then(|question| question.header.clone())
                        .unwrap_or_else(|| t!("tui.title.ask").into_owned())
                }
            }
            Modal::Model { .. } => t!("tui.title.model").into_owned(),
            Modal::Rewind { .. } => t!("tui.title.rewind").into_owned(),
            Modal::Sessions { scope, .. } => match scope {
                SessionScope::Folder => t!("tui.title.sessions_folder").into_owned(),
                SessionScope::All => t!("tui.title.sessions_all").into_owned(),
            },
            Modal::Plan { .. } => t!("tui.title.plan").into_owned(),
            Modal::Help { .. } => t!("tui.title.help").into_owned(),
            Modal::Tools { tools, .. } => match tools {
                Some(tools) => t!("tui.title.tools_fmt", count = tools.len()).into_owned(),
                None => t!("tui.title.tools").into_owned(),
            },
            Modal::ContextContent { state } => state
                .current
                .as_ref()
                .map_or_else(|| t!("tui.title.context").into_owned(), |node| {
                    node.name.clone()
                }),
            Modal::Jobs { .. } => t!("tui.title.jobs").into_owned(),
            Modal::SwitchModel { .. } => t!("tui.title.switch_model").into_owned(),
            Modal::WorkflowApproval { state } => match state.request.kind {
                moc_protocol::methods::WorkflowApprovalKind::Amend => {
                    t!("tui.title.wf_amend").into_owned()
                }
                moc_protocol::methods::WorkflowApprovalKind::HoleFill => {
                    t!("tui.title.wf_hole").into_owned()
                }
                _ => t!("tui.title.wf_approval").into_owned(),
            },
            Modal::WorkflowRuns { .. } => t!("tui.title.wf_runs").into_owned(),
            Modal::WorkflowRun { state } => {
                t!("tui.title.wf_run_fmt", title = state.title.as_str()).into_owned()
            }
            Modal::Monitor => t!("tui.title.monitor").into_owned(),
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
            Modal::Plan { .. } => 4,
            Modal::Help { .. } | Modal::Tools { .. } | Modal::ContextContent { .. } => 0,
            Modal::Jobs { jobs, .. } => jobs.len(),
            Modal::SwitchModel { .. } => 2,
            Modal::WorkflowApproval { state } => state.rows(),
            Modal::WorkflowRuns { runs, picker } => runs
                .iter()
                .filter(|run| {
                    let query = picker.query.to_lowercase();
                    run.run_id.to_lowercase().contains(&query)
                        || run
                            .title
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .count(),
            Modal::WorkflowRun { state } => state.roles.len(),
            Modal::Monitor => 0,
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
            Modal::Jobs { selected, .. } => {
                *selected = selected
                    .saturating_add_signed(isize::try_from(delta).expect("方向增量可表示为 isize"))
                    .min(count.saturating_sub(1));
            }
            Modal::Ask { .. }
            | Modal::Rewind { .. }
            | Modal::Help { .. }
            | Modal::Tools { .. }
            | Modal::ContextContent { .. }
            | Modal::Monitor => {}
            Modal::SwitchModel { option, .. } => {
                *option = (*option as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::WorkflowApproval { state } => {
                state.focus = (state.focus as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::WorkflowRun { state } => {
                state.focus = (state.focus as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::Model { picker, detail, .. } => {
                picker.selected = picker
                    .selected
                    .saturating_add_signed(delta as isize)
                    .min(count - 1);
                detail.list_scroll.reveal_selection = true;
                detail.pointer = None;
            }
            Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. } => {
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

#[cfg(test)]
mod tests;
