//! 应用状态：会话、模型、转写块、折叠开关与界面数据。
//!
//! 状态集中在一个 `App` 结构上，`update` 全部由事件循环驱动；渲染层只读。

use std::collections::HashMap;

use moc_protocol::methods::TodoPhase;
use moc_protocol::{
    ContextUsageResult, ModelInfo, SessionInfo, SessionMode, SessionPermission, SessionUsageResult,
    Usage,
};

use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;

use crate::modal::{Modal, SessionScope};
use crate::protocol::AppEvent;
use crate::theme::Theme;
use crate::transcript::{
    Block, DetailLevel, Entry, TableChartState, ToolCallBlock, ToolCallFinish, ToolCallStart,
};
use crate::widgets::completion::CompletionState;
use crate::widgets::prompt::PromptState;

/// 焦点所在的区域：滚动区浏览或输入框。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Prompt,
    Scrollback,
    Btw,
}

impl Focus {
    pub fn toggle(self) -> Self {
        match self {
            Focus::Prompt => Focus::Scrollback,
            Focus::Scrollback | Focus::Btw => Focus::Prompt,
        }
    }
}

/// 一次性提示（notice）的存活时长。
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 上游配置不一致告警的存活时长：比 notice 长，避免用户错过静默改写。
const WARNING_TTL: std::time::Duration = std::time::Duration::from_secs(8);
/// Ctrl+C 二次确认的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlCPending {
    /// 输入框非空时首次按 Ctrl+C，再次按下清空输入框。
    ClearPrompt,
    /// 输入框为空（或中止 Prompt 后）首次按 Ctrl+C，再次按下退出应用。
    Quit,
}

/// Ctrl+C 二次确认有效窗口（3 秒内）。
pub const CTRL_C_TTL: std::time::Duration = std::time::Duration::from_secs(3);

/// 进行中的上下文压缩提示：记录它属于哪个会话与已显示的秒数。
#[derive(Debug, Clone)]
pub struct CompactionProgress {
    /// 发起压缩的会话；切到别的会话后不再更新它。
    pub session_id: String,
    /// 压缩开始时刻（前端计时）。
    pub started_at: std::time::Instant,
    /// 已经写进提示的整秒数。
    pub shown_secs: u64,
}

/// 进行中提示的前缀；完成时按它找回同一条提示原位替换。
const COMPACTION_RUNNING_PREFIX: &str = "上下文压缩中";

/// 进行中提示的文案。
fn compaction_running_note(secs: u64) -> String {
    format!("{COMPACTION_RUNNING_PREFIX}…({secs}s)")
}

/// 转写区右缘滚动条上一帧的几何：点击与拖拽都按它换算滚动位置。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollbackScrollbar {
    /// 轨道区域（屏幕坐标，一列宽）。
    pub track: Rect,
    /// 滑块行数。
    pub thumb: u16,
    /// 上一帧的最大滚动偏移（距尾部行数）。
    pub max_offset: usize,
}

impl ScrollbackScrollbar {
    /// 滑块顶端相对轨道顶端的行数。
    ///
    /// `scroll_offset` 是距尾部的行数：0（跟随尾部）时滑块贴底，等于
    /// `max_offset` 时贴顶。
    pub fn thumb_top(&self, scroll_offset: usize) -> u16 {
        let travel = self.track.height.saturating_sub(self.thumb);
        if self.max_offset == 0 || travel == 0 {
            return travel;
        }
        // 距历史顶端的比例：行数规模远小于 2^53，f64 表示无精度损失
        let from_top =
            self.max_offset.saturating_sub(scroll_offset) as f64 / self.max_offset as f64;
        // 结果已 clamp 到 [0, travel]，且 travel ≤ u16::MAX，窄化不会截断
        (f64::from(travel) * from_top)
            .round()
            .clamp(0.0, f64::from(travel)) as u16
    }

    /// 光标行对应的滚动偏移（距尾部行数）；`grab` 是按下点到滑块顶端的行差。
    ///
    /// 按在轨道空白处时滑块中线跟到光标（`grab` 取滑块行数的一半），与
    /// `/context` 正文窗的滚动条一致。
    pub fn offset_at(&self, row: u16, grab: i32) -> usize {
        let travel = i32::from(self.track.height.saturating_sub(self.thumb));
        if self.max_offset == 0 || travel <= 0 {
            return 0;
        }
        let top = (i32::from(row).saturating_sub(i32::from(self.track.y)) - grab).clamp(0, travel);
        // 乘积按 u64 计算：32 位目标上 top × max_offset 会超出 usize。
        let distance = top as u64 * self.max_offset as u64 / travel as u64;
        self.max_offset - distance as usize
    }
}

/// 转写区滚动条的拖拽状态。
#[derive(Debug, Clone, Copy)]
pub struct ScrollbackDrag {
    /// 按下时光标到滑块顶端的行差。
    pub grab: i32,
}

/// 应用状态。
pub struct App {
    pub theme: &'static Theme,
    /// UI 事件队列的发送端（供异步任务投递结果，如剪贴板图片）。
    pub events: UnboundedSender<AppEvent>,
    /// 当前会话信息（模型、模式、cwd 都从这里读）。
    pub session: Option<SessionInfo>,
    /// 当前会话是只读打开：另一个 core 进程持有写租约，本前端不能发提示或改状态。
    pub read_only: bool,
    /// 当前平台可用的沙盒模式；`/permission` 与设置页据此过滤。
    pub sandbox_modes: Vec<SessionPermission>,
    /// 可选模型集合（`config/get` 拉取，/model 菜单与 statusline 用）。
    pub models: Vec<ModelInfo>,
    /// 转写条目（用户消息、回复、工具调用、todo……）。
    pub blocks: Vec<Entry>,
    /// 是否仍有未加载的更早 transcript。
    pub history_has_more: bool,
    /// 下一页应使用的游标（当前最早条目的 ID）。
    pub history_before_entry_id: Option<String>,
    /// 避免滚动期间重复发送历史请求。
    pub history_loading: bool,
    /// 滚动到历史边缘后由事件循环发起异步分页。
    pub history_load_requested: bool,
    /// 最近一次渲染计算出的可滚动总偏移。
    pub scroll_max_offset: usize,
    /// Todo 清单（`TodoChanged` 最新快照）。
    pub todos: Vec<TodoPhase>,
    /// 右上角 todo HUD 的交互与命中状态。
    pub todo_hud: TodoHudState,
    /// 思考块的显示档位（ctrl+t 循环）。
    pub thinking_detail: DetailLevel,
    /// 工具块的显示档位（ctrl+o 循环）。
    pub tool_detail: DetailLevel,
    /// 视图版本：全局档位等影响全部块渲染的因素变化时自增。
    pub view_gen: u64,
    /// 输入框状态（多行编辑、历史、chip）。
    pub prompt: PromptState,
    /// 持久化输入历史；展示范围按偏好过滤。
    pub history: crate::history::InputHistory,
    /// 斜杠补全菜单；`None` = 关闭。
    pub completion: Option<CompletionState>,
    /// 输入文本变更代次；异步资源补全只接受最新代次。
    pub prompt_revision: u64,
    /// 当前资源补全请求的 prefix。
    pub completion_prefix: Option<String>,
    /// 打开的模态；键盘优先路由给它。
    pub modal: Option<Modal>,
    /// 当前焦点。
    pub focus: Focus,
    /// 旁路问答只驻留内存，独立于主转写、输入历史及用量。
    pub btw: crate::btw::BtwState,
    /// 最近一次绘制的输入框区域，用于鼠标点击恢复焦点。
    pub prompt_area: Rect,
    /// 顶部上下文占用区域，用于鼠标点击打开详情。
    pub context_area: Rect,
    /// 输入框下边框右端模式名的屏幕区域（鼠标点击就地弹出模式菜单）。
    pub mode_area: Rect,
    /// 输入框下边框模型名区域（鼠标点击打开模型选择）。
    pub model_tag_area: Rect,
    /// 输入框下边框权限区域（鼠标点击切换权限）。
    pub permission_tag_area: Rect,
    /// 模态选项行命中矩形（Permission、Model、Sessions 等）。
    pub modal_item_hits: Vec<(usize, Rect)>,
    /// 计划审阅中的执行模型文本命中区。
    pub plan_model_area: Rect,
    /// 计划批注视图的正文区域（鼠标行选择换算用）。
    pub plan_body_area: Rect,
    /// 计划批注视图可见源码行号（屏幕行 → 0-based 源码行）。
    pub plan_row_map: Vec<usize>,
    /// 消息回退时间线区域（鼠标滚动/点击换算用）。
    pub rewind_timeline_area: Rect,
    /// 消息回退详情区域（鼠标滚动换算用）。
    pub rewind_detail_area: Rect,
    /// 提问卡片各选项（含自定义行）的命中矩形；`options.len()` 下标代表自定义行。
    /// 每帧渲染时重建，下标越界（问题切换后）自然落空。
    pub ask_option_hits: Vec<(usize, Rect)>,
    /// 就地模式菜单；`None` = 关闭。
    pub mode_menu: Option<ModeMenu>,
    /// 当前这条用户消息的临时契约覆盖（`/ask` 注入）；整条消息回复完毕才清空。
    ///
    /// 只影响图例与状态栏的展示，会话方言与 plan 叠加态仍以 `session` 为准。
    pub transient_contract: Option<moc_protocol::methods::PromptContract>,
    /// Core 估算的上下文组成与总量。
    pub context_usage: Option<ContextUsageResult>,
    /// 滚动偏移（自尾部算起的行数；0 = 跟随尾部）。
    pub scroll_offset: usize,
    /// 是否有进行中的提示。
    pub prompt_running: bool,
    /// 是否已经发出取消当前提示的请求。
    pub cancel_requested: bool,
    /// 当前提示开始时间（statusline 计时）。
    pub prompt_started_at: Option<std::time::Instant>,
    /// 进行中的上下文压缩提示；`None` = 当前没有压缩在等模型。
    pub compaction_progress: Option<CompactionProgress>,
    /// 当前 thinking block 开始时间（用于计算 duration_ms）。
    pub thinking_started_at: Option<std::time::Instant>,
    /// 本次模型响应是否已有思考块：usage 的 reasoning_tokens 只回填它，
    /// 没有思考块的响应（如纯工具调用）不得动上一响应的块。
    thinking_has_block: bool,
    pub usage: Usage,
    /// 会话累计用量（`/usage` 与状态栏共用，由 core replay 出的权威统计刷新）。
    pub session_usage: Option<SessionUsageResult>,
    /// 用量刷新请求序号；回执按它丢弃乱序或跨会话迟到的结果。
    usage_request: u64,
    usage_applied: u64,
    /// 会话累计费用（美元；需要模型 pricing 配置）。
    pub cost_usd: f64,
    /// git 分支（异步刷新）。
    pub git_branch: Option<String>,
    /// 独立 tui.toml 的 statusline 模板。
    pub statusline_template: Option<String>,
    /// 最近一次绘制的转写区，用于鼠标选区的绝对坐标换算。
    pub scrollback_area: Rect,
    /// 转写区右缘滚动条的上一帧几何（点击与拖拽换算用）。
    pub scrollbar: ScrollbackScrollbar,
    /// 视口命中表（点击折叠用）：`(块下标, 标题行, 标题后行)`。
    pub mouse_hits: Vec<(usize, u16, u16)>,
    /// 视口行文本（`(行, 文本)`，选择复制用）。
    pub paint_lines: Vec<(u16, String)>,
    /// 终端图形能力（启动探测一次）；`None` = 本终端不出图表入口。
    pub graphics: Option<crate::render::graphics::GraphicsSupport>,
    /// 内联图片的解码 / 编码缓存（工具结果、markdown 图片、SVG 代码块共用）。
    pub images: crate::render::image::ImageStore,
    /// 本帧要贴的位图槽位（`ScrollbackView::paint` 填充，帧后由
    /// [`crate::render::graphics::emit`] 消费）。
    pub image_slots: Vec<ImageSlot>,
    /// 本帧可点击的表格入口（`paint` 填充，命中判定读它）。
    pub chart_hits: Vec<ChartHit>,
    /// 已贴到终端上的位图（kitty 依靠它删旧放置，其余协议靠它跳过多余重发）。
    pub image_placed: Vec<Placed>,
    /// 上一帧是否被浮层覆盖：切换瞬间整批重放，避免残影或盖住浮层。
    pub image_covered: bool,
    /// 浮层结束后需要整批重放（浮层期间图像被覆盖或擦除，位置指纹已不可信）。
    pub image_resume: bool,
    /// 图像消失或尺寸变化后请求清屏重绘：sixel / iTerm2 不能单独擦除像素，
    /// 只靠 ratatui 的文本 diff 会把旧图留在“仍然空白”的占位格上。
    pub image_repaint: bool,
    /// 鼠标悬停的表格入口 `(块下标, 表格序号)`。
    pub hover_chart: Option<(usize, usize)>,
    /// 鼠标拖选区（屏幕坐标 (列, 行) 起点 / 终点）。
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// 按下未抬起的位置（区分点击与拖选）。
    pub pending_click: Option<(u16, u16)>,
    /// 滚动条拖拽；`Some` 时左键拖动只改滚动偏移，不进入文本选区。
    pub scrollbar_drag: Option<ScrollbackDrag>,
    /// ctrl+c 二次确认状态及触发时刻（3 秒内有效）。
    pub ctrl_c_pending: Option<(CtrlCPending, std::time::Instant)>,
    /// Esc 二次确认状态。
    pub clear_pending: bool,
    /// 一次性提示（操作反馈、启动警告）；带 TTL 自动消失。
    pub notice: Option<String>,
    /// 当前 notice 的设置时刻（toast 超时判定）。
    pub notice_at: Option<std::time::Instant>,
    /// 上游实际配置与请求不一致的告警；同一时刻最多一条，新告警替换旧告警。
    pub warning: Option<String>,
    /// 当前 warning 的设置时刻（超时判定）。
    pub warning_at: Option<std::time::Instant>,
    /// 退出标志。
    pub terminal_focused: bool,
    pub should_quit: bool,
    pub preferences: crate::config::TuiConfig,
    pub keymap: crate::config::KeyMap,
    pub tui_store: Option<crate::config::TuiStore>,
    pub settings: Option<crate::settings::SettingsPanel>,
    pub next_config_id: u64,
    /// 设置页正在写入的配置范围；`Some` 时新改动先排队。
    pub config_flight: Option<moc_protocol::ConfigScope>,
    /// 排队等待写入的配置改动；写入串行，期间的改动合并到最近一次。
    pub config_queue: Option<ConfigWrite>,
    pub startup_cwd: String,
    pub startup_mode: SessionMode,
    /// 启动时新会话是否直接进 plan 叠加态（`--plan`）。
    pub startup_plan: bool,
    pub startup_pending: Option<String>,
    /// 本工作区插件注册的斜杠命令，供补全与命令分派使用。
    pub plugin_commands: Vec<moc_protocol::methods::PluginCommandInfo>,
    /// 最近一次 `plugin/list` 的结果，供 `/plugins` 展示。
    pub plugins: Vec<moc_protocol::methods::PluginInfo>,
    pub subagents: std::collections::BTreeMap<String, crate::subagent::SubagentView>,
    pub subagent_selected: Option<String>,
    pub subagent_area: Rect,
    pub subagent_hits: Vec<(String, Rect)>,
    pub subagent_scroll: usize,
    pub subagent_generation: u64,
    /// 聚焦中的子代理；`Some` 时当前会话就是它，`Esc` 或双击 `←` 返回。
    pub focused: Option<FocusedAgent>,
    /// 返回主会话时要还原的输入草稿与滚动偏移。
    pub focus_restore: Option<(String, usize)>,
    /// 双击 `←` 返回的第一次按键时刻。
    pub left_tap_pending: Option<std::time::Instant>,
    pub queues: HashMap<String, std::collections::VecDeque<crate::queue::QueuedPrompt>>,
    /// 已提交给 core、但还没在安全边界写入会话的补充消息，按会话隔离。
    pub steers: HashMap<String, std::collections::VecDeque<crate::queue::PendingSteer>>,
    /// 已收到 `SteeringApplied` 的 core ID；用于处理事件先于 RPC 响应的竞态。
    pub resolved_steers: HashMap<String, std::collections::HashSet<u64>>,
    pub inflight_prompts: std::collections::HashSet<String>,
    /// 本前端发起、会占住会话提示锁的操作（模型切换、手动压缩）在飞的会话。
    ///
    /// 期间队列不派发：core 用提示锁拒绝并发提示，抢跑只会把用户刚发出的消息变成
    /// `SessionBusy` 报错；等操作收尾的事件到达再继续派发。
    pub inflight_session_ops: std::collections::HashSet<String>,
    pub next_queue_id: u64,
    pub queue_hits: Vec<(u64, Rect, Rect)>,
    pub queue_area: Rect,
    pub queue_offset: usize,
    pub interrupt_pending: std::collections::HashSet<String>,
    pub prompt_stash: Option<PromptState>,

    /// 工具调用 ID 到批次 ID 的暂存映射，批次开始事件先于各个工具开始事件。
    tool_batches: HashMap<String, String>,
    /// 工具调用 ID 到转写块下标的索引，工具起止更新走热路径。
    tool_indices: HashMap<String, usize>,
    generation_start: Option<usize>,
}

/// 排队等待写入的配置改动。
pub struct ConfigWrite {
    pub scope: moc_protocol::ConfigScope,
    pub cwd: String,
    pub document: serde_json::Value,
    /// 上一次成功写入后的文件版本；写入回执会刷新它。
    pub revision: String,
}

/// 一帧里要贴的一张位图：屏幕格坐标 + 内容指纹。
///
/// `hash` 覆盖占位行文本与图片键：行内容、全局视图或图片内容变了就重发，
/// 纯粹的光标移动/滚动不改指纹，靠 `x`/`y` 变化触发重贴。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSlot {
    /// 所在转写块下标。
    pub entry: usize,
    /// 该位图的载荷来源。
    pub source: SlotSource,
    /// 屏幕列（滚动区原点 + 正文缩进）。
    pub x: u16,
    /// 屏幕行。
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    /// 位图像素尺寸。
    pub px: (u32, u32),
    pub hash: u64,
    /// kitty 放置 ID：与图像 ID 分开，同一张图可以贴多处。
    pub placement_id: u32,
}

/// 位图载荷的来源：位图数据要么挂在转写块上，要么归 `ImageStore` 缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotSource {
    /// 表格图表：载荷在 `Entry::table_chart` 里，随块一起失效。
    Chart { table: usize },
    /// 通用图片（工具结果 / markdown / SVG）：载荷在 `App::images`。
    Image(crate::render::image::ImageKey),
}

/// 一个可点击的表格入口：整行命中，列区间限定在按钮文字上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartHit {
    pub entry: usize,
    pub table: usize,
    pub row: u16,
    pub x: u16,
    pub w: u16,
}

/// 已经贴到终端上的位图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub image_id: u32,
    pub placement_id: u32,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    pub hash: u64,
    pub proto: crate::render::graphics::GraphicsProto,
}

/// 聚焦中的子代理：主 TUI 整体切到它的会话。
///
/// 参见 `docs/tui.md` 的子代理一节：`Ctrl+N` 名册里 `Enter` 进入，
/// `Esc`（输入框为空）或双击 `←` 返回；返回时主会话按磁盘真相重载，
/// 聚焦期间主会话的更新被忽略。
#[derive(Debug, Clone)]
pub struct FocusedAgent {
    /// 返回时切回的会话。
    pub home: SessionInfo,
    /// 返回时还原的输入草稿。
    pub home_draft: String,
    /// 返回时还原的滚动偏移。
    pub home_scroll: usize,
}

impl App {
    /// 代理面（名册、侧栏）挂靠的会话：聚焦子代理时仍是主会话。
    ///
    /// 人类管理的是「我这一支的子代理」，不是子代理眼里的兄弟；
    /// 模型面的 `list_agents` 在子代理里返回兄弟，那是给模型看的视角。
    pub fn agents_root_id(&self) -> Option<String> {
        match &self.focused {
            Some(focused) => Some(focused.home.id.clone()),
            None => self.session.as_ref().map(|session| session.id.clone()),
        }
    }

    /// 是否聚焦在某个子代理上。
    pub fn focused_agent(&self) -> bool {
        self.focused.is_some()
    }

    /// 取当前会话 ID；没有会话时提示先去配置模型。
    pub fn require_session(&mut self) -> Option<String> {
        let Some(session) = self.session.as_ref() else {
            self.set_notice("请先在 /config 配置一个主模型");
            return None;
        };
        Some(session.id.clone())
    }

    pub fn new(theme: &'static Theme, events: UnboundedSender<AppEvent>) -> Self {
        let images = crate::render::image::ImageStore::new(events.clone());
        Self {
            theme,
            events,
            session: None,
            read_only: false,
            sandbox_modes: vec![SessionPermission::Full],
            models: Vec::new(),
            blocks: Vec::new(),
            generation_start: None,
            history_has_more: false,
            history_before_entry_id: None,
            history_loading: false,
            history_load_requested: false,
            scroll_max_offset: 0,
            todos: Vec::new(),
            todo_hud: TodoHudState::default(),
            thinking_detail: DetailLevel::default(),
            tool_detail: DetailLevel::default(),
            view_gen: 0,
            prompt: PromptState::default(),
            history: crate::history::InputHistory::default(),
            completion: None,
            prompt_revision: 0,
            completion_prefix: None,
            modal: None,
            focus: Focus::Prompt,
            btw: crate::btw::BtwState::default(),
            prompt_area: Rect::default(),
            context_area: Rect::default(),
            mode_area: Rect::default(),
            model_tag_area: Rect::default(),
            permission_tag_area: Rect::default(),
            modal_item_hits: Vec::new(),
            plan_model_area: Rect::default(),
            plan_body_area: Rect::default(),
            plan_row_map: Vec::new(),
            rewind_timeline_area: Rect::default(),
            rewind_detail_area: Rect::default(),
            ask_option_hits: Vec::new(),
            mode_menu: None,
            transient_contract: None,
            context_usage: None,
            scroll_offset: 0,
            prompt_running: false,
            cancel_requested: false,
            prompt_started_at: None,
            compaction_progress: None,
            thinking_started_at: None,
            thinking_has_block: false,
            usage: Usage::default(),
            session_usage: None,
            usage_request: 0,
            usage_applied: 0,
            cost_usd: 0.0,
            git_branch: None,
            statusline_template: None,
            scrollback_area: Rect::default(),
            scrollbar: ScrollbackScrollbar::default(),
            mouse_hits: Vec::new(),
            paint_lines: Vec::new(),
            graphics: None,
            images,
            image_slots: Vec::new(),
            chart_hits: Vec::new(),
            image_placed: Vec::new(),
            image_covered: false,
            image_resume: false,
            image_repaint: false,
            hover_chart: None,
            selection: None,
            pending_click: None,
            scrollbar_drag: None,
            ctrl_c_pending: None,
            clear_pending: false,
            notice: None,
            notice_at: None,
            warning: None,
            warning_at: None,
            terminal_focused: true,
            should_quit: false,
            preferences: crate::config::TuiConfig::default(),
            keymap: crate::config::KeyMap::compile(&crate::config::TuiConfig::default())
                .expect("内置键位合法"),
            tui_store: None,
            settings: None,
            next_config_id: 0,
            config_flight: None,
            config_queue: None,
            startup_cwd: String::new(),
            startup_mode: SessionMode::Normal,
            startup_plan: false,
            startup_pending: None,
            plugin_commands: Vec::new(),
            plugins: Vec::new(),
            subagents: std::collections::BTreeMap::new(),
            subagent_selected: None,
            focused: None,
            focus_restore: None,
            left_tap_pending: None,
            subagent_area: Rect::default(),
            subagent_hits: Vec::new(),
            subagent_scroll: 0,
            subagent_generation: 0,
            queues: HashMap::new(),
            steers: HashMap::new(),
            resolved_steers: HashMap::new(),
            inflight_prompts: std::collections::HashSet::new(),
            inflight_session_ops: std::collections::HashSet::new(),
            next_queue_id: 0,
            queue_hits: Vec::new(),
            queue_area: Rect::default(),
            queue_offset: 0,
            interrupt_pending: std::collections::HashSet::new(),
            prompt_stash: None,
            tool_batches: HashMap::new(),
            tool_indices: HashMap::new(),
        }
    }

    /// 写入配置：有写入在途时先排队，等回执落地后带着新版本接着写。
    pub fn write_config(
        &mut self,
        connection: &crate::protocol::EmbeddedConnection,
        write: ConfigWrite,
    ) {
        if self.config_flight.is_some() {
            self.config_queue = Some(write);
            return;
        }
        self.config_flight = Some(write.scope);
        let client = connection.client.clone();
        let events = self.events.clone();
        let panel = self.settings.as_ref().map_or(0, |panel| panel.id);
        let action = moc_protocol::ConfigAction::Save {
            scope: write.scope,
            cwd: write.cwd,
            document: write.document,
            revision: write.revision,
        };
        tokio::spawn(async move {
            let result = client
                .call::<moc_protocol::ConfigManage>(action)
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse {
                panel,
                job: crate::settings::Job::Save,
                result,
            });
        });
    }

    /// 是否有配置写入在途或排队。
    pub fn saving_config(&self) -> bool {
        self.config_flight.is_some() || self.config_queue.is_some()
    }

    /// 设置一次性提示并刷新计时。
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.notice_at = Some(std::time::Instant::now());
    }

    /// 设置上游配置不一致告警并刷新计时。
    ///
    /// 只保留最新一条：同一次提示内多次不一致（多轮各自一次请求）会彼此
    /// 覆盖，绝不堆叠。
    pub fn set_warning(&mut self, text: impl Into<String>) {
        self.warning = Some(text.into());
        self.warning_at = Some(std::time::Instant::now());
    }

    /// notice 的消失时刻；`None` 表示当前没有显示中的 notice。
    ///
    /// 只返回仍在未来的时刻，事件循环才能安全地 `sleep_until`——返回已过去的
    /// 时刻会让 select 立刻再次命中，形成空转。
    pub fn notice_deadline(&self) -> Option<std::time::Instant> {
        let at = self.notice_at.filter(|_| self.notice.is_some())?;
        let deadline = at + NOTICE_TTL;
        (std::time::Instant::now() < deadline).then_some(deadline)
    }

    /// 检查当前是否存在指定类型且在 3 秒 TTL 内有效的 Ctrl+C 待确认状态。
    pub fn has_ctrl_c_pending(&self, expected: CtrlCPending) -> bool {
        match self.ctrl_c_pending {
            Some((kind, at)) => kind == expected && at.elapsed() <= CTRL_C_TTL,
            None => false,
        }
    }

    /// 消费指定类型的 Ctrl+C 待确认状态，若在 3 秒 TTL 内有效则重置为 None 并返回 true。
    pub fn take_ctrl_c_pending(&mut self, expected: CtrlCPending) -> bool {
        if self.has_ctrl_c_pending(expected) {
            self.ctrl_c_pending = None;
            true
        } else {
            self.ctrl_c_pending = None;
            false
        }
    }

    /// 消费「双击 `←`」的第一次按键：时间窗内返回 true（第二次即触发返回主会话）。
    ///
    /// 窗口与 `Ctrl+C` 的二次确认一致（3 秒），避免引入第二套节奏。
    pub fn take_left_tap_pending(&mut self, now: std::time::Instant) -> bool {
        let repeated = self
            .left_tap_pending
            .is_some_and(|at| now.duration_since(at) <= CTRL_C_TTL);
        self.left_tap_pending = None;
        repeated
    }

    /// notice 是否仍在显示期内。
    pub fn notice_visible(&self) -> bool {
        self.notice_deadline().is_some()
    }

    /// warning 的消失时刻；`None` 表示当前没有显示中的告警。语义同 [`Self::notice_deadline`]。
    pub fn warning_deadline(&self) -> Option<std::time::Instant> {
        let at = self.warning_at.filter(|_| self.warning.is_some())?;
        let deadline = at + WARNING_TTL;
        (std::time::Instant::now() < deadline).then_some(deadline)
    }

    /// warning 是否仍在显示期内。
    pub fn warning_visible(&self) -> bool {
        self.warning_deadline().is_some()
    }

    /// 当前模型展示名；未连接时显示 `idle`。
    pub fn model_display(&self) -> &str {
        self.session
            .as_ref()
            .map_or("idle", |session| model_selector_label(&session.model))
    }
    /// 找到指定调用 ID 的工具条目。
    pub fn tool_entry_mut(&mut self, tool_call_id: &str) -> Option<&mut Entry> {
        let index = *self.tool_indices.get(tool_call_id)?;
        match self.blocks.get(index).map(|entry| &entry.block) {
            Some(Block::Tool(tool)) if tool.id == tool_call_id => self.blocks.get_mut(index),
            _ => None,
        }
    }

    /// 工具执行开始：把更新里的调用信息落地成转写块。
    ///
    /// 模型直发的调用与 `run_code` 沙盒里的嵌套调用共用这里，形状只差 `parent_id`
    /// （嵌套调用据此在滚动区缩进）；批次归属在批次开始时就登记好了。
    pub fn start_tool_block(&mut self, call: ToolCallStart) {
        // 工具调用开始即思考流结束：耗时不能把随后的工具执行算进去。
        self.finish_thinking();
        let ToolCallStart {
            id,
            name,
            parent_id,
            arguments,
        } = call;
        let index = self.blocks.len();
        self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
            batch_id: self.tool_batches.remove(&id),
            id: id.clone(),
            parent_id,
            name,
            arguments,
            status: None,
            result: None,
            diff: None,
            duration_ms: None,
            running: true,
            progress: None,
            images: Vec::new(),
        })));
        self.tool_indices.insert(id, index);
    }

    /// 登记同一模型响应返回的工具批次。
    pub fn register_tool_batch(
        &mut self,
        batch_id: String,
        calls: impl IntoIterator<Item = String>,
    ) {
        for tool_call_id in calls {
            self.tool_batches.insert(tool_call_id, batch_id.clone());
        }
    }

    /// 工具执行结束：回填终态。
    ///
    /// `name` / `parent_id` / `arguments` 只在调用块缺席时用得上：批次中断或前端中途
    /// 接入时更新里没有配对的 `ToolStarted`，这时用它们（以及本地已登记的批次）补建一个
    /// 与正常路径同形的终态块，而不是把结果丢掉。
    pub fn finish_tool_block(&mut self, finish: ToolCallFinish) {
        let ToolCallFinish {
            id,
            name,
            parent_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
        } = finish;
        if let Some(entry) = self.tool_entry_mut(&id) {
            entry.finish_tool(status, result, duration_ms, diff);
        } else {
            // 前端中途接入时没有配对的开始更新：补建终态块前同样先定稿思考流。
            self.finish_thinking();
            let index = self.blocks.len();
            self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
                batch_id: self.tool_batches.remove(&id),
                id: id.clone(),
                parent_id,
                name,
                arguments,
                status: Some(status),
                result: Some(result.content),
                diff,
                duration_ms,
                running: false,
                progress: None,
                // 正常路径在 `Entry::finish_tool` 里填；这条兜底分支不能把图丢掉
                images: crate::transcript::ToolImage::from_sources(result.images),
            })));
            self.tool_indices.insert(id, index);
        }
    }

    /// 流式尾部是否已有该类块。
    pub fn has_streaming(&self, matches: impl Fn(&Block) -> bool) -> bool {
        self.generation_start
            .is_none_or(|start| self.blocks.len() > start)
            && self
                .blocks
                .last()
                .is_some_and(|entry| matches(&entry.block))
    }

    /// 追加一条系统提示。
    pub fn push_note(&mut self, text: String, error: bool) {
        self.blocks.push(Entry::new(Block::Note { text, error }));
    }

    /// 上下文压缩开始：插入进行中提示；换方法重试时沿用同一条计时。
    pub fn start_compaction(&mut self) {
        if self.compaction_progress.is_some() {
            return;
        }
        let Some(session_id) = self.session.as_ref().map(|session| session.id.clone()) else {
            return;
        };
        self.push_note(compaction_running_note(0), false);
        self.compaction_progress = Some(CompactionProgress {
            session_id,
            started_at: std::time::Instant::now(),
            shown_secs: 0,
        });
    }

    /// 刷新进行中提示的秒数；返回提示是否变化（供事件循环决定重绘）。
    pub fn tick_compaction(&mut self) -> bool {
        let Some(current) = self.session.as_ref().map(|session| session.id.clone()) else {
            self.compaction_progress = None;
            return false;
        };
        let Some(progress) = self.compaction_progress.as_mut() else {
            return false;
        };
        if progress.session_id != current {
            self.compaction_progress = None;
            return false;
        }
        let secs = u64::try_from(progress.started_at.elapsed().as_secs()).unwrap_or(u64::MAX);
        if secs == progress.shown_secs {
            return false;
        }
        progress.shown_secs = secs;
        self.replace_compaction_note(compaction_running_note(secs), false);
        true
    }

    /// 压缩收尾：把进行中提示原位替换为终态；没有进行中提示时追加一条。
    pub fn finish_compaction(&mut self, text: String, error: bool) {
        self.compaction_progress = None;
        self.replace_compaction_note(text, error);
    }

    /// 用终态文案替换最近一条进行中提示；找不到时追加，不留下两条。
    fn replace_compaction_note(&mut self, text: String, error: bool) {
        let existing = self.blocks.iter_mut().rev().find(|entry| {
            matches!(&entry.block, Block::Note { text, .. } if text.starts_with(COMPACTION_RUNNING_PREFIX))
        });
        match existing {
            Some(entry) => {
                entry.block = Block::Note { text, error };
                entry.touch();
            }
            None => self.blocks.push(Entry::new(Block::Note { text, error })),
        }
    }

    /// 隐式 todo 提醒：按工具样式插入一条占位块，让用户知道刚刚注入了一次提醒。
    pub fn push_todo_reminder(&mut self) {
        self.push_block(Block::Tool(ToolCallBlock {
            id: format!("todo-reminder-{}", self.blocks.len()),
            batch_id: None,
            parent_id: None,
            name: "TodoReminder".into(),
            arguments: String::new(),
            status: Some(moc_protocol::methods::ToolStatus::Completed),
            result: None,
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            images: Vec::new(),
        }));
    }

    /// 更新运行中工具的进度行（按 `tool_call_id` 定位）。
    pub fn set_tool_progress(
        &mut self,
        tool_call_id: &str,
        rows: Vec<moc_protocol::methods::ToolProgressRow>,
    ) {
        if let Some(entry) = self.tool_entry_mut(tool_call_id) {
            let Block::Tool(tool) = &mut entry.block else {
                return;
            };
            tool.progress = Some(rows);
            entry.touch();
        }
    }

    /// 追加一条用户消息。
    pub fn push_user(&mut self, text: String) {
        self.blocks.push(Entry::timed(Block::User { text }));
    }

    /// 追加一条实时块（助手正文 / 思维链 / 工具调用）：带落地时刻。
    pub fn push_block(&mut self, block: Block) {
        self.blocks.push(Entry::timed(block));
    }

    /// 切换一张表格的图表形态：已展开则还原成表格，否则就地分析后展开。
    ///
    /// 分析与渲染共用同一次 markdown 解析（`render_markdown_with` 交出的纯文本
    /// 快照），因此「屏幕上的表格」与「图表吃到的数据」不会出现两套口径。
    /// 只处理助手正文的**顶层表格**：引用块 / 列表内嵌表格没有独立命中区。
    pub fn toggle_table_chart(&mut self, entry: usize, table: usize) {
        let Some(block) = self.blocks.get(entry) else {
            return;
        };
        if block.table_chart(table).is_some() {
            if let Some(block) = self.blocks.get_mut(entry) {
                block.set_table_chart(table, None);
            }
            return;
        }
        let theme = self.theme;
        let width = usize::from(self.scrollback_area.width)
            .saturating_sub(crate::render::PROSE_INDENT)
            .max(8);
        let prepared: Result<moc_core::chart::ChartSpec, &'static str> = {
            let Some(entry_block) = self.blocks.get(entry) else {
                return;
            };
            let Block::Assistant { text } = &entry_block.block else {
                return;
            };
            let rendered = crate::render::markdown::render_markdown_with(
                text,
                width,
                theme,
                crate::render::markdown::RenderViews {
                    charts: &[],
                    graphics: None,
                    streaming: false,
                    ..crate::render::markdown::RenderViews::default()
                },
            );
            match rendered.raw.get(table) {
                None => Err("表格内容已变化，请重试"),
                Some(raw) => {
                    let analysis = moc_core::chart::analyze(raw);
                    moc_core::chart::plan(&analysis)
                        .map(|plan| moc_core::chart::build(&analysis, &plan))
                        .ok_or("这张表无法图表化")
                }
            }
        };
        let spec = match prepared {
            Ok(spec) => spec,
            Err(text) => {
                self.set_notice(text);
                return;
            }
        };
        // 编号必须与内联图片共用同一个空间：kitty 里用已存在的 `i=` 重传会
        // 释放旧图像、清掉它的全部放置，两套自增器各自从 1 起必然互相覆盖。
        let image_id = self.images.allocate_image_id();
        if let Some(block) = self.blocks.get_mut(entry) {
            block.set_table_chart(
                table,
                Some(TableChartState {
                    spec,
                    payload: None,
                    px: (0, 0),
                    image_id,
                }),
            );
        }
    }

    /// 让含指定图片键的工具块失效；返回是否命中。
    ///
    /// 图片从「解码中」变成真像素时，只有含它的块需要重渲染。`view_gen` 是全局
    /// 失效，长会话里会让最近 64 块把所有 markdown 重解析一遍，图片一多就成了
    /// 肉眼可见的卡顿。markdown 图片的键在渲染期才算出，反查不到，由调用方
    /// 回落到全局失效。
    pub fn touch_blocks_with_image(&mut self, key: &crate::render::image::ImageKey) -> bool {
        let mut hit = false;
        for entry in &mut self.blocks {
            let matches = matches!(
                &entry.block,
                Block::Tool(tool) if tool.images.iter().any(|image| image.key == *key)
            );
            if matches {
                entry.touch();
                hit = true;
            }
        }
        hit
    }

    pub fn start_generation(&mut self) {
        // 新一轮生成开始：上一响应若没有正文/工具收尾，思考块在这里定稿。
        self.finish_thinking();
        self.thinking_has_block = false;
        self.generation_start = Some(self.blocks.len());
    }

    pub fn commit_generation(&mut self) {
        self.generation_start = None;
    }

    pub fn discard_generation(&mut self) {
        if let Some(start) = self.generation_start.take() {
            self.blocks.truncate(start);
            self.view_gen = self.view_gen.wrapping_add(1);
        }
        self.tool_indices
            .retain(|_, index| *index < self.blocks.len());
        self.thinking_started_at = None;
        self.thinking_has_block = false;
    }

    /// 撤掉上一条提示在转写尾部留下的块：那一轮已经被 `/retry` 从会话里剪掉了。
    ///
    /// 只撤「肯定不在会话里」的块——说明性提示、状态为取消的工具，以及（整轮被丢弃时）
    /// 没跑完的助手正文与思考；碰到已落库的内容（跑完的工具、用户消息……）就停手，
    /// 界面与会话不会因此错位。`keep_reply` 用于重放工具的情况：assistant 消息还留在
    /// 会话里，它的正文不能跟着撤。
    pub fn rewind_failed_turn(&mut self, keep_reply: bool) -> bool {
        let mut removed = false;
        while self
            .blocks
            .last()
            .is_some_and(|entry| trimmable_tail_block(&entry.block, keep_reply))
        {
            self.blocks.pop();
            removed = true;
        }
        if !removed {
            return false;
        }
        self.generation_start = self.generation_start.map(|_| self.blocks.len());
        self.tool_indices
            .retain(|_, index| *index < self.blocks.len());
        self.thinking_started_at = None;
        self.thinking_has_block = false;
        self.view_gen = self.view_gen.wrapping_add(1);
        true
    }

    /// 开始新的思考块：定稿上一块，并登记计时与 usage 回填目标。
    pub fn start_thinking(&mut self) {
        self.finish_thinking();
        self.thinking_has_block = true;
        self.thinking_started_at = Some(std::time::Instant::now());
        self.push_block(Block::Thinking {
            text: String::new(),
            reasoning_tokens: None,
            duration_ms: None,
        });
    }

    /// 结束当前思考块的流式增量并记录实际思考时长。
    ///
    /// 只在思考流真正结束的边界调用：正文/工具开始、下一轮生成开始、提示收尾。
    pub fn finish_thinking(&mut self) {
        let Some(started_at) = self.thinking_started_at.take() else {
            return;
        };
        let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(entry) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.block, Block::Thinking { .. }))
            && let Block::Thinking { duration_ms, .. } = &mut entry.block
        {
            *duration_ms = Some(elapsed_ms);
            entry.touch();
        }
    }

    /// 提示收尾：把耗时与结束时刻回填到最后一条助手块（`Worked for` 尾注）。
    pub fn finish_prompt(&mut self) {
        self.finish_thinking();
        let elapsed = self.prompt_started_at.map(|at| at.elapsed());
        let was_cancel_requested = self.cancel_requested;
        self.reset_prompt_state();
        self.prompt_running = false;
        if was_cancel_requested {
            self.set_notice("已停止");
        }
        let Some(elapsed) = elapsed else {
            return;
        };
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if let Some(entry) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.block, Block::Assistant { .. }))
        {
            entry.stamp_prompt(crate::transcript::now_ms(), elapsed_ms);
        }
    }

    /// 思考块档位循环一档并使视图缓存失效。
    ///
    /// 手动展开的单块也要交还档位控制：否则全局切到隐藏／简略后，
    /// 它仍停在完整档，看起来像快捷键失灵。
    pub fn cycle_thinking_detail(&mut self) {
        self.thinking_detail = self.thinking_detail.next();
        for entry in &mut self.blocks {
            if matches!(entry.block, Block::Thinking { .. }) {
                entry.clear_level_override();
            }
        }
        self.view_gen += 1;
    }

    /// 工具块档位循环一档并使视图缓存失效。
    pub fn cycle_tool_detail(&mut self) {
        self.tool_detail = self.tool_detail.next();
        for entry in &mut self.blocks {
            if matches!(entry.block, Block::Tool(_)) {
                entry.clear_level_override();
            }
        }
        self.view_gen += 1;
    }

    /// 块的层档位：思考块看 `thinking_detail`、工具块看 `tool_detail`、
    /// 其余块（IRC 卡片等）固定简略预览。
    pub fn block_level(&self, block: &Block) -> DetailLevel {
        match block {
            Block::Thinking { .. } => self.thinking_detail,
            Block::Tool(_) => self.tool_detail,
            _ => DetailLevel::Brief,
        }
    }

    /// Todo 快照更新：转写里替换最后一个 Todo 块（避免重复树），否则追加。
    pub fn update_todos(&mut self, phases: Vec<TodoPhase>) {
        self.todos = phases.clone();
        if self.todos.is_empty() {
            self.todo_hud.reset();
        }
        let has_todo_block = self
            .blocks
            .iter()
            .any(|entry| matches!(entry.block, Block::Todo { .. }));
        if has_todo_block {
            // 替换最后一个 Todo 块：直接原地改写并推进版本
            if let Some(entry) = self
                .blocks
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.block, Block::Todo { .. }))
            {
                entry.block = Block::Todo { phases };
                entry.touch();
            }
        } else {
            self.blocks.push(Entry::new(Block::Todo { phases }));
        }
    }

    /// 打开普通会话模型选择器。
    pub fn open_model_picker(&mut self) {
        let current = self
            .session
            .as_ref()
            .map_or_else(String::new, |session| session.model.clone());
        self.open_model_picker_for(current, &[], crate::modal::ModelTarget::Session);
    }

    /// 选择计划执行模型，不修改当前会话模型。
    pub fn open_plan_model_picker(&mut self) {
        let state = match self.modal.take() {
            Some(Modal::Plan { state }) => state,
            modal => {
                self.modal = modal;
                return;
            }
        };
        if state.submitting || self.models.is_empty() {
            self.modal = Some(Modal::Plan { state });
            return;
        }
        self.plan_model_area = Rect::default();
        self.open_model_picker_for(
            state.execution_model.clone(),
            &[],
            crate::modal::ModelTarget::PlanReview(state),
        );
    }

    /// 打开 `/config` 的模型角色选择器：与 `Ctrl+P` 共用候选、搜索与思考等级。
    ///
    /// `specials` 是角色伪值（沿用主模型 / 沿用当前会话模型），排在模型前面；
    /// 调用方必须已经确认 `models` 非空，否则这个字段应当退回文本编辑。
    pub fn open_config_model_picker(
        &mut self,
        path: Vec<String>,
        label: String,
        current: String,
        specials: &[(&str, &str)],
    ) {
        self.open_model_picker_for(
            current,
            specials,
            crate::modal::ModelTarget::Config { path, label },
        );
    }

    fn open_model_picker_for(
        &mut self,
        current: String,
        specials: &[(&str, &str)],
        target: crate::modal::ModelTarget,
    ) {
        if self.models.is_empty() {
            self.set_notice("没有可用的模型，请先在设置中添加");
            return;
        }
        let entries = Self::picker_entries(&self.models, specials);

        let current_id = current
            .rsplit_once(':')
            .map_or(current.as_str(), |(id, _)| id);
        let selected = entries
            .iter()
            .position(|entry| entry.selector == current_id)
            .unwrap_or(0);
        let mut level = crate::modal::LevelState::default();
        // 当前值已经写明等级、且该条目仍支持它时沿用；否则用高亮模型的最高等级。
        if let Some((_, level_text)) = current.rsplit_once(':')
            && let Some(entry) = entries.get(selected)
            && entry.levels.iter().any(|candidate| candidate == level_text)
        {
            level.manual = Some(level_text.to_owned());
        }
        let picker = crate::modal::PickerState {
            selected,
            offset: selected.saturating_sub(7),
            ..Default::default()
        };
        self.modal = Some(Modal::Model {
            entries,
            current,
            picker,
            level,
            target,
        });
    }

    /// 选择器候选：角色伪值排在模型前面，模型带上各自的思考等级与上下文窗口。
    fn picker_entries(
        models: &[ModelInfo],
        specials: &[(&str, &str)],
    ) -> Vec<crate::modal::ModelEntry> {
        let mut entries = Vec::with_capacity(models.len() + specials.len());
        for (selector, label) in specials {
            entries.push(crate::modal::ModelEntry {
                selector: (*selector).to_owned(),
                label: (*label).to_owned(),
                display_name: String::new(),
                levels: Vec::new(),
                context_window: None,
                max_output_tokens: None,
                inputs: Vec::new(),
                remote_compaction: false,
                pricing: None,
            });
        }
        entries.extend(models.iter().map(|model| crate::modal::ModelEntry {
            selector: model.id.clone(),
            label: model.id.clone(),
            display_name: model.display_name.clone(),
            levels: model.thinking_levels.clone().unwrap_or_else(|| {
                crate::modal::DEFAULT_LEVELS
                    .iter()
                    .map(|level| (*level).to_owned())
                    .collect()
            }),
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            inputs: model.inputs.clone(),
            remote_compaction: model.remote_compaction,
            pricing: model.pricing,
        }));
        entries
    }

    /// 打开就地模式菜单：高亮当前方言，并让出可能叠在输入框上方的补全菜单。
    /// 会话跑过第一条提示后方言锁定，菜单不再提供。
    pub fn open_mode_menu(&mut self) {
        let Some(session) = self.session.as_ref() else {
            self.set_notice("当前没有活动会话");
            return;
        };
        if session.mode_locked {
            self.set_notice("会话方言已锁定；换方言请新建会话");
            return;
        }

        self.completion = None;
        let current = self.session.as_ref().map(|session| session.mode);
        self.mode_menu = Some(ModeMenu {
            selected: current
                .and_then(|mode| MODES.iter().position(|candidate| *candidate == mode))
                .unwrap_or(0),
            area: Rect::default(),
        });
    }

    /// 打开会话恢复选择器（列表由调用方异步填充后更新）。
    pub fn open_session_picker(&mut self) {
        self.modal = Some(Modal::Sessions {
            sessions: Vec::new(),
            cwd: self.startup_cwd.clone(),
            scope: SessionScope::default(),
            picker: crate::modal::PickerState {
                loading: true,
                ..Default::default()
            },
        });
    }

    /// 打开消息回退选择器；数据由调用方异步加载。
    pub fn open_rewind(&mut self) {
        self.modal = Some(Modal::Rewind {
            state: crate::modal::RewindState::default(),
        });
    }

    /// 用加载的会话替换当前会话与转写。
    ///
    /// 前端同时只呈现一个会话：上一个会话的提示计时、取消标记、临时模式、用量与
    /// todo 都在此收尾，否则队列不再派发、状态栏与停止键都停留在上一会话。
    /// 会话范围的在飞记账（`inflight_prompts`/`inflight_session_ops`/`interrupt_pending`）
    /// 按会话保留：切回仍在运行的会话时，「运行中」的判定与排队派发才正确。
    pub fn switch_session(&mut self, info: SessionInfo, blocks: Vec<Entry>) {
        if let Some(previous) = &self.session
            && previous.id != info.id
        {
            self.btw.leave_session(&previous.id);
            if self.focus == Focus::Btw {
                self.focus = Focus::Prompt;
            }
        }
        let clm_active = info.mode == SessionMode::ClmExperimental;
        let session_id = info.id.clone();
        self.reset_prompt_state();
        // 默认按可写处理；只读加载由 `AppEvent::Loaded` 在处理完本函数后标回。
        self.read_only = false;
        self.compaction_progress = None;
        self.prompt_running = self.inflight_prompts.contains(&session_id);
        self.usage = Usage::default();
        // 用量统计按会话重取；在飞请求都属于上一个会话，水位线一抬就直接作废。
        self.session_usage = None;
        self.usage_applied = self.usage_request;
        self.cost_usd = 0.0;
        self.todos.clear();
        self.todo_hud.reset();
        self.session = Some(info);
        self.blocks = blocks;
        self.context_usage = None;
        self.plan_model_area = Rect::default();
        self.plan_body_area = Rect::default();
        self.plan_row_map.clear();
        if clm_active {
            self.set_notice("CLM 实验模式已启用：模型将维护实时上下文");
        }
        self.tool_indices.clear();
        // 批次暂存只服务于当前视图的流：转写里已有批次归属的完整形态，
        // 换会话后旧的批次表中的调用既无块可续、也可能被兜底补建误用。
        self.tool_batches.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        self.scroll_offset = 0;
        self.history_has_more = false;
        self.history_before_entry_id = None;
        self.history_loading = false;
        self.history_load_requested = false;
        self.scroll_max_offset = 0;
        self.view_gen += 1;
        // 「按项目」范围下，召回历史要跟着会话的 cwd 走。
        self.refresh_input_history();
    }

    /// 收掉与「当前提示」绑定的本地状态；[`App::finish_prompt`] 与会话切换共用。
    ///
    /// `prompt_running` 不在其中：提示收尾要清它，切会话则按会话级在飞记账重算。
    fn reset_prompt_state(&mut self) {
        self.thinking_started_at = None;
        self.thinking_has_block = false;
        self.generation_start = None;
        self.prompt_started_at = None;
        self.cancel_requested = false;
        self.ctrl_c_pending = None;
        self.transient_contract = None;
    }

    pub fn set_history_cursor(&mut self, has_more: bool, before_entry_id: Option<String>) {
        self.history_has_more = has_more;
        self.history_before_entry_id = before_entry_id;
        self.history_loading = false;
    }

    pub fn begin_history_load(&mut self) -> bool {
        if self.history_loading || !self.history_has_more {
            return false;
        }
        self.history_loading = true;
        true
    }

    pub fn prepend_history(&mut self, mut blocks: Vec<Entry>) {
        if blocks.is_empty() {
            self.history_loading = false;
            return;
        }
        if let Some(start) = &mut self.generation_start {
            *start += blocks.len();
        }
        blocks.append(&mut self.blocks);
        self.blocks = blocks;
        self.tool_batches.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        self.view_gen = self.view_gen.wrapping_add(1);
        self.history_loading = false;
    }

    /// 在增量流末尾追加文本：已存在同类流式块则续写，否则由调用方先新建。
    pub fn append_streaming(
        &mut self,
        pick: impl Fn(&mut Block) -> Option<&mut String>,
        delta: &str,
    ) {
        let Some(entry) = self.blocks.last_mut() else {
            return;
        };
        let Some(text) = pick(&mut entry.block) else {
            return;
        };
        text.reserve(delta.len());
        text.push_str(delta);
        // 正文继续增长后表格序号与内容都可能漂移，旧图立刻作废。
        // 旧图的清屏由 graphics::emit 在帧后发现并标记，不在这里猜协议。
        entry.charts.clear();
        entry.touch();
    }

    /// 追加受预算约束的流式文本；用于思考内容，避免单个块吞噬整个会话内存。
    pub fn append_streaming_limited(
        &mut self,
        pick: impl Fn(&mut Block) -> Option<&mut String>,
        delta: &str,
        max_bytes: usize,
        marker: &str,
    ) {
        let Some(entry) = self.blocks.last_mut() else {
            return;
        };
        let Some(text) = pick(&mut entry.block) else {
            return;
        };
        if text.len() >= max_bytes {
            return;
        }
        let remaining = max_bytes - text.len();
        if delta.len() <= remaining {
            text.reserve(delta.len());
            text.push_str(delta);
        } else {
            let mut marker_bytes = marker.len().min(remaining);
            while marker_bytes > 0 && !marker.is_char_boundary(marker_bytes) {
                marker_bytes -= 1;
            }
            let prefix_limit = remaining - marker_bytes;
            let mut prefix = prefix_limit.min(delta.len());
            while prefix > 0 && !delta.is_char_boundary(prefix) {
                prefix -= 1;
            }
            text.reserve(prefix + marker_bytes);
            text.push_str(&delta[..prefix]);
            text.push_str(&marker[..marker_bytes]);
        }
        entry.touch();
    }

    // ---------- 输入框编辑（委托给 PromptState） ----------

    pub fn prompt_changed(&mut self) {
        self.prompt_revision = self.prompt_revision.wrapping_add(1);
        self.completion = CompletionState::derive(&self.prompt.text, &self.plugin_commands);
        self.completion_prefix = None;
    }

    /// 插入一个字符。
    pub fn prompt_insert_char(&mut self, ch: char) {
        self.prompt.insert_char(ch);
        self.prompt_changed();
    }

    /// 输入框当前文本快照。
    pub fn prompt_text(&self) -> &str {
        &self.prompt.text
    }

    // ---------- 输入历史 ----------

    /// 挂载持久化输入历史，并按当前展示范围填充输入框。
    pub fn load_input_history(&mut self, home: &std::path::Path) {
        self.history = crate::history::InputHistory::load(home);
        self.refresh_input_history();
    }

    /// 把一条已提交文本写进持久化历史；内存历史由输入框在提交时维护。
    pub fn record_input_history(&mut self, text: &str) {
        let project = self.history_project();
        self.history.record(text, &project);
    }

    /// 按当前偏好与项目重新填充输入历史；切换范围或会话时调用。
    pub fn refresh_input_history(&mut self) {
        let entries = self
            .history
            .visible(self.preferences.history_scope, &self.history_project());
        self.prompt.seed_history(entries);
    }

    /// 历史归属的项目目录：有会话时用会话 cwd，否则用启动目录。
    fn history_project(&self) -> String {
        self.session
            .as_ref()
            .map_or_else(|| self.startup_cwd.clone(), |session| session.cwd.clone())
    }

    // ---------- statusline 数据 ----------

    /// 当前模型的定价（从模型目录按 selector 匹配）。
    pub fn current_pricing(&self) -> Option<&moc_protocol::methods::ModelPricing> {
        let model = self.session.as_ref()?.model.as_str();
        let base = model.split(':').next()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .as_ref()
            .and_then(|info| info.pricing.as_ref())
    }

    /// 当前模型的上下文窗口。
    pub fn context_window(&self) -> Option<u64> {
        let model = self.session.as_ref()?.model.as_str();
        let base = model.split(':').next()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .and_then(|info| info.context_window)
    }

    /// 当前上下文占用：请求实测值优先，未请求时使用 core 估算值。
    pub fn context_used(&self) -> u64 {
        if self.usage.input_tokens > 0 {
            self.usage.input_tokens
        } else {
            self.context_usage
                .as_ref()
                .map_or(0, |usage| usage.total_tokens)
        }
    }

    /// 应用一次请求级 usage 刷新：费用累计与上下文占用读它。
    pub fn apply_usage(&mut self, usage: Usage) {
        let previous = &self.usage;
        let delta_in = usage.input_tokens.saturating_sub(previous.input_tokens);
        let delta_out = usage.output_tokens.saturating_sub(previous.output_tokens);
        let delta_cached = usage
            .cached_input_tokens
            .unwrap_or(0)
            .saturating_sub(previous.cached_input_tokens.unwrap_or(0));
        if (delta_in > 0 || delta_out > 0)
            && let Some(pricing) = self.current_pricing()
        {
            // input_tokens 已折入缓存命中：非命中输入单独按全价计
            let uncached = delta_in.saturating_sub(delta_cached);
            let cache_price = pricing.cache_read.unwrap_or(pricing.input);
            self.cost_usd += uncached as f64 / 1e6 * pricing.input
                + delta_out as f64 / 1e6 * pricing.output
                + delta_cached as f64 / 1e6 * cache_price;
        }
        self.usage = usage;
        // reasoning_tokens 属于本次响应：没有思考块的响应（上游可能报 0）
        // 不能按「最近一块」写回，否则会把上一响应的 token 数抹掉。
        if self.thinking_has_block
            && let Some(reasoning_tokens) = usage.reasoning_tokens
            && reasoning_tokens > 0
            && let Some(entry) = self
                .blocks
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.block, Block::Thinking { .. }))
            && let Block::Thinking {
                reasoning_tokens: stored,
                ..
            } = &mut entry.block
            && *stored != Some(reasoning_tokens)
        {
            *stored = Some(reasoning_tokens);
            entry.touch();
        }
    }

    /// 发出一次会话用量刷新，返回请求序号；回执带回序号以丢弃迟到的旧结果。
    pub fn begin_usage_request(&mut self) -> u64 {
        self.usage_request = self.usage_request.wrapping_add(1);
        self.usage_request
    }

    /// 应用一次会话用量回执：比已应用结果更旧的（乱序或跨会话迟到）直接丢弃。
    pub fn apply_session_usage(&mut self, request: u64, usage: SessionUsageResult) {
        if request <= self.usage_applied {
            return;
        }
        self.usage_applied = request;
        self.session_usage = Some(usage);
    }

    /// 会话累计输入／输出 token；统计还没回来时按 0 处理。
    pub fn session_tokens(&self) -> (u64, u64) {
        self.session_usage.as_ref().map_or((0, 0), |usage| {
            (usage.total.input_tokens, usage.total.output_tokens)
        })
    }

    /// 会话累计输出速率（tok/s），与 `/usage` 同源：输出 token 总量除以生成阶段
    /// 耗时（首 token 等待与工具执行不计入）。没有可拆分数据（旧会话）时退回
    /// 整段请求用时，与升级前的口径一致。
    pub fn session_tokens_per_second(&self) -> f64 {
        let Some(usage) = &self.session_usage else {
            return 0.0;
        };
        let total = &usage.total;
        if total.output_tokens == 0 {
            return 0.0;
        }
        let ms = if total.gen_ms > 0 {
            total.gen_ms
        } else if total.ttft_calls == 0 {
            total.active_ms
        } else {
            return 0.0;
        };
        if ms == 0 {
            0.0
        } else {
            total.output_tokens as f64 * 1000.0 / ms as f64
        }
    }

    /// todo 完成进度 `(done, total)`。
    pub fn todo_progress(&self) -> (usize, usize) {
        let total: usize = self.todos.iter().map(|phase| phase.items.len()).sum();
        let done = self
            .todos
            .iter()
            .flat_map(|phase| phase.items.iter())
            .filter(|item| item.status == moc_protocol::methods::TodoStatus::Completed)
            .count();
        (done, total)
    }

    /// 图例与状态栏展示的模式标签：会话方言 + plan 叠加态 + 本次提示的临时契约。
    pub fn display_mode(&self) -> String {
        let Some(session) = self.session.as_ref() else {
            return "idle".to_owned();
        };
        let mut label = mode_label(session.mode).to_owned();
        if session.plan_active {
            label.push_str("+plan");
        }
        if self.transient_contract.is_some() {
            label.push_str("+ask");
        }
        label
    }
}

#[cfg(test)]
mod streaming_budget_tests {
    use super::*;

    #[test]
    fn thinking_stream_is_bounded_without_splitting_utf8() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.push_block(Block::Thinking {
            text: String::new(),
            reasoning_tokens: None,
            duration_ms: None,
        });
        app.append_streaming_limited(
            |block| match block {
                Block::Thinking { text, .. } => Some(text),
                _ => None,
            },
            &"思考".repeat(100),
            32,
            "…",
        );
        let Block::Thinking { text, .. } = &app.blocks[0].block else {
            panic!("期望思考块");
        };
        assert!(text.len() <= 32);
        assert!(text.is_char_boundary(text.len()));
    }
}

#[cfg(test)]
mod detail_control_tests {
    use super::*;
    use moc_protocol::methods::ToolStatus;

    fn thinking() -> Block {
        Block::Thinking {
            text: "思考".into(),
            reasoning_tokens: None,
            duration_ms: None,
        }
    }

    fn tool() -> Block {
        Block::Tool(ToolCallBlock {
            id: "call_1".into(),
            batch_id: None,
            parent_id: None,
            name: "bash".into(),
            arguments: "{}".into(),
            status: Some(ToolStatus::Completed),
            result: Some("done".into()),
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            images: Vec::new(),
        })
    }

    /// Ctrl+T / Ctrl+O 切换全局档位时收回该层的手动展开，另一层不受影响。
    #[test]
    fn cycling_detail_reclaims_manual_expansion() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.push_block(thinking());
        app.push_block(tool());
        app.blocks[0].toggle_level(app.thinking_detail);
        app.blocks[1].toggle_level(app.tool_detail);
        assert_eq!(app.blocks[0].level_override, Some(DetailLevel::Full));
        assert_eq!(app.blocks[1].level_override, Some(DetailLevel::Full));

        // 转一圈回到简略：手动展开必须被收回，块跟随全局档位。
        for _ in 0..3 {
            app.cycle_thinking_detail();
        }
        assert_eq!(app.thinking_detail, DetailLevel::Brief);
        assert_eq!(app.blocks[0].level_override, None);
        assert_eq!(app.blocks[0].level(app.thinking_detail), DetailLevel::Brief);
        assert_eq!(
            app.blocks[1].level_override,
            Some(DetailLevel::Full),
            "Ctrl+T 不该碰工具块"
        );

        app.blocks[0].toggle_level(app.thinking_detail);
        app.cycle_tool_detail();
        assert_eq!(app.blocks[1].level_override, None);
        assert_eq!(
            app.blocks[0].level_override,
            Some(DetailLevel::Full),
            "Ctrl+O 不该碰思考块"
        );
    }
}

/// 调用块的补建：`ToolFinished` 自带工具名、父调用与参数，
/// 前端在缺席 `ToolStarted` 时据此补出与正常路径同形的终态块。
#[cfg(test)]
mod tool_recovery_tests {
    use super::*;
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    fn result(tool_call_id: &str, content: &str) -> ToolResult {
        ToolResult {
            tool_call_id: tool_call_id.to_owned(),
            content: content.to_owned(),
            is_error: false,
            images: Vec::new(),
        }
    }

    #[test]
    fn orphan_tool_finish_recovers_name_arguments_and_batch() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.register_tool_batch("batch_1".to_owned(), ["call_1".to_owned()]);
        app.finish_tool_block(ToolCallFinish {
            id: "call_1".to_owned(),
            name: "bash".to_owned(),
            parent_id: None,
            arguments: r#"{"command":"cargo test"}"#.to_owned(),
            status: ToolStatus::Completed,
            result: result("call_1", "退出码 0"),
            duration_ms: Some(7),
            diff: None,
        });

        let Block::Tool(tool) = &app.blocks[0].block else {
            panic!("缺席调用开始时应当补建工具块");
        };
        assert_eq!(
            tool.name, "bash",
            "补建的块必须用更新里的工具名，而不是调用 ID"
        );
        assert_eq!(tool.arguments, r#"{"command":"cargo test"}"#);
        assert_eq!(tool.batch_id.as_deref(), Some("batch_1"));
        assert_eq!(tool.result.as_deref(), Some("退出码 0"));
        assert_eq!(tool.status, Some(ToolStatus::Completed));
        assert!(!tool.running);
    }

    #[test]
    fn orphan_ptc_child_finish_keeps_parent_and_name() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.finish_tool_block(ToolCallFinish {
            id: "call_parent:1".to_owned(),
            name: "read".to_owned(),
            parent_id: Some("call_parent".to_owned()),
            arguments: r#"{"path":"artifact://ses_1/a.txt"}"#.to_owned(),
            status: ToolStatus::Error,
            result: result("call_parent:1", "读取失败"),
            duration_ms: Some(3),
            diff: None,
        });

        let Block::Tool(tool) = &app.blocks[0].block else {
            panic!("缺席子调用开始时应当补建工具块");
        };
        assert_eq!(tool.name, "read");
        assert_eq!(
            tool.parent_id.as_deref(),
            Some("call_parent"),
            "子调用保持缩进归属"
        );
        assert_eq!(tool.arguments, r#"{"path":"artifact://ses_1/a.txt"}"#);
        assert_eq!(tool.status, Some(ToolStatus::Error));
    }
}

/// 会话方言的展示名；CLM 以括号标识，保持实验状态始终可见。
pub fn mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Normal => "normal",
        SessionMode::Ptc => "ptc",
        SessionMode::ClmExperimental => "(clm)",
    }
}

/// 会话方言顺序：就地菜单条目与键盘循环共用同一份顺序。
/// plan/ask 不是方言，不在此列。
pub const MODES: [SessionMode; 3] = [
    SessionMode::Normal,
    SessionMode::Ptc,
    SessionMode::ClmExperimental,
];

/// Todo HUD 的一次左键按下：区分单击固定与拖动卡片。
#[derive(Debug, Clone, Copy)]
pub struct TodoHudPress {
    start: (u16, u16),
    card_origin: (u16, u16),
    from_card: bool,
    dragging: bool,
}

/// 右上角 todo HUD 的交互与命中状态。
#[derive(Debug, Default)]
pub struct TodoHudState {
    /// 上一帧折叠胶囊的屏幕命中区。
    pub chip_area: Rect,
    /// 上一帧展开卡片的屏幕命中区；关闭时为空。
    pub card_area: Rect,
    /// 指针在胶囊或展开卡片内。
    pub hovered: bool,
    /// 点击固定：指针移出仍保持展开。
    pub pinned: bool,
    /// 取消固定后需先离开胶囊/卡片，hover 才允许重新展开。
    ///
    /// 两个命中区分开判断；用包围盒会把卡片与胶囊之间的大片区域变成死区。
    pub suppress: Option<(Rect, Rect)>,
    /// 左键按下信息。
    press: Option<TodoHudPress>,
    /// 拖动后的卡片原点；仅 pinned 时用于绘制，hover 展开固定用右上角。
    pub drag_origin: Option<(u16, u16)>,
    /// marquee 当前任务文本；变化时重置起点。
    pub marquee_key: String,
    /// marquee 起始时刻；`None` = 不滚动。
    pub marquee_start: Option<std::time::Instant>,
    /// 本帧胶囊是否在滚动；事件循环据此安排 tick。
    pub marquee_active: bool,
    /// 本帧卡片是否已绘制；图像悬挂判定使用。
    pub card_visible: bool,
}

/// 左键位移超过该格数才算拖动，否则按单击处理。
const TODO_DRAG_THRESHOLD: u16 = 2;

impl TodoHudState {
    /// 每帧绘制开头：清命中区与帧标志，保留交互状态。
    pub fn begin_frame(&mut self) {
        self.chip_area = Rect::default();
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_active = false;
    }

    /// 模态、设置页或空间不足时收起；保留固定与记忆位置。
    pub fn on_hidden(&mut self) {
        self.hovered = false;
        self.suppress = None;
        self.press = None;
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_active = false;
    }

    /// 空清单或切换会话：全部复位。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn on_resize(&mut self) {
        self.hovered = false;
        self.suppress = None;
        self.press = None;
        self.card_area = Rect::default();
        self.card_visible = false;
        self.marquee_key.clear();
        self.marquee_start = None;
        self.marquee_active = false;
    }

    pub fn on_focus_lost(&mut self) {
        self.hovered = false;
        self.press = None;
        self.marquee_active = false;
    }

    pub fn is_open(&self) -> bool {
        self.pinned || (self.hovered && self.suppress.is_none())
    }

    /// 命中胶囊或展开中的卡片。
    pub fn hit(&self, pos: (u16, u16)) -> bool {
        self.chip_area.contains(pos.into())
            || (self.is_open() && self.card_area.contains(pos.into()))
    }

    /// 鼠标移动：更新 hover 与 suppression；返回是否落在 HUD 上。
    pub fn pointer_moved(&mut self, pos: (u16, u16)) -> bool {
        // 按住卡片时卡片随光标移动，旧命中区不能拿来判定离开。
        if self.press.is_some_and(|press| press.from_card) {
            return true;
        }
        if let Some((chip, card)) = self.suppress {
            if chip.contains(pos.into()) || card.contains(pos.into()) {
                self.hovered = false;
                return true;
            }
            self.suppress = None;
        }
        let inside = self.hit(pos);
        self.hovered = inside;
        inside
    }

    /// 左键按下：命中则记录单击/拖动起点并消费。
    pub fn pointer_down(&mut self, pos: (u16, u16)) -> bool {
        if self.is_open() && self.card_area.contains(pos.into()) {
            self.press = Some(TodoHudPress {
                start: pos,
                card_origin: (self.card_area.x, self.card_area.y),
                from_card: true,
                dragging: false,
            });
            return true;
        }
        if self.chip_area.contains(pos.into()) {
            self.press = Some(TodoHudPress {
                start: pos,
                card_origin: (self.card_area.x, self.card_area.y),
                from_card: false,
                dragging: false,
            });
            return true;
        }
        false
    }

    /// 左键拖动：卡片按下后越过阈值即移动卡片并自动固定。
    pub fn pointer_drag(&mut self, pos: (u16, u16), area: Rect) -> bool {
        let Some(press) = self.press.as_mut() else {
            return false;
        };
        if !press.from_card {
            return false;
        }
        if !press.dragging {
            let dx = pos.0.abs_diff(press.start.0);
            let dy = pos.1.abs_diff(press.start.1);
            if dx.max(dy) <= TODO_DRAG_THRESHOLD {
                return true;
            }
            press.dragging = true;
            self.pinned = true;
            self.hovered = true;
            self.suppress = None;
        }
        let dx = i32::from(pos.0) - i32::from(press.start.0);
        let dy = i32::from(pos.1) - i32::from(press.start.1);
        let origin = (
            i32::from(press.card_origin.0) + dx,
            i32::from(press.card_origin.1) + dy,
        );
        let size = (self.card_area.width, self.card_area.height);
        self.drag_origin = Some(clamp_todo_origin(origin, size, area));
        true
    }

    /// 左键抬起：拖动结束，或单击切换固定。
    pub fn pointer_up(&mut self, pos: (u16, u16)) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        if press.from_card && press.dragging {
            return true;
        }
        if !self.hit(pos) {
            if !self.pinned {
                self.hovered = false;
            }
            return true;
        }
        if self.pinned {
            self.pinned = false;
            self.hovered = false;
            self.suppress = Some((self.chip_area, self.card_area));
            self.card_area = Rect::default();
        } else {
            self.pinned = true;
            self.hovered = true;
            self.suppress = None;
            if press.from_card {
                self.drag_origin = Some((self.card_area.x, self.card_area.y));
            }
        }
        true
    }
}

/// 把卡片原点夹在转写区内；尺寸大于区域时退回区域原点。
fn clamp_todo_origin(origin: (i32, i32), size: (u16, u16), area: Rect) -> (u16, u16) {
    let max_x = area.right().saturating_sub(size.0).max(area.x);
    let max_y = area.bottom().saturating_sub(size.1).max(area.y);
    let x = origin.0.clamp(i32::from(area.x), i32::from(max_x));
    let y = origin.1.clamp(i32::from(area.y), i32::from(max_y));
    (
        u16::try_from(x).unwrap_or(area.x),
        u16::try_from(y).unwrap_or(area.y),
    )
}

/// 把拖选的两个端点归一成阅读顺序：先上后下，同一行从左到右。
///
/// 坐标是屏幕格 `(列, 行)`；绘制与复制都以同一顺序切片，反向拖选因此
/// 不需要各自再判方向。
pub fn ordered_selection(
    ((ax, ay), (bx, by)): ((u16, u16), (u16, u16)),
) -> ((u16, u16), (u16, u16)) {
    if (ay, ax) <= (by, bx) {
        ((ax, ay), (bx, by))
    } else {
        ((bx, by), (ax, ay))
    }
}

/// 点击图例模式名后就地弹出的菜单。
pub struct ModeMenu {
    /// 键盘 / 悬停高亮的条目下标（[`MODES`] 顺序）。
    pub selected: usize,
    /// 上一帧绘制的菜单区域；鼠标命中使用。
    pub area: Rect,
}

impl ModeMenu {
    /// 命中的条目下标；点在边框和菜单外返回 `None`。
    pub fn entry_at(&self, column: u16, row: u16) -> Option<usize> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row.checked_sub(self.area.y.saturating_add(1))?);
        (index < MODES.len()).then_some(index)
    }
}

/// 模型展示名：选择串即 `供应商/模型:等级`，思考关闭时省掉 `:off` 后缀。
pub fn model_selector_label(selector: &str) -> &str {
    selector.strip_suffix(":off").unwrap_or(selector)
}

/// 这个块是否属于「没跑完、已经从会话里剪掉」的那一轮。
///
/// 说明性提示（错误说明、压缩进度）纯属界面产物，跟着一起撤；用户消息、跑完的工具
/// 调用都是真实历史，不能算进来。
fn trimmable_tail_block(block: &Block, keep_reply: bool) -> bool {
    match block {
        Block::Note { .. } => true,
        Block::Tool(tool) => tool.status == Some(moc_protocol::methods::ToolStatus::Cancelled),
        Block::Assistant { .. } | Block::Thinking { .. } => !keep_reply,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ModeMenu, mode_label, model_selector_label};
    use moc_protocol::SessionMode;
    use ratatui::layout::Rect;

    /// M17：切会话收尾上一会话的本地状态，但保留会话级在飞记账。
    ///
    /// 不重置会让队列永久不派发（`prompt_running` 卡真）、状态栏显示上一会话的用量、
    /// Esc/Ctrl+C 走进「停止当前提示」分支。
    #[test]
    fn switch_session_resets_local_prompt_state() {
        use super::{App, CtrlCPending};
        use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};
        use moc_protocol::{SessionInfo, SessionMode, Usage};
        use tokio::sync::mpsc::unbounded_channel;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let session = |id: &str| SessionInfo {
            id: id.into(),
            title: "标题".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            mode: SessionMode::Normal,
            plan_active: false,
            mode_locked: false,
            parent_session_id: None,
            forked_from: None,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        };

        // 旧会话正在跑提示：计时、用量、todo、临时模式与取消标记都在飞。
        app.session = Some(session("ses_a"));
        app.prompt_running = true;
        app.prompt_started_at = Some(std::time::Instant::now());
        app.thinking_started_at = Some(std::time::Instant::now());
        app.cancel_requested = true;
        app.transient_contract = Some(moc_protocol::methods::PromptContract::Ask);
        app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
        app.usage = Usage {
            input_tokens: 10,
            ..Usage::default()
        };
        app.cost_usd = 1.5;
        app.todos = vec![TodoPhase {
            phase: "阶段".into(),
            items: vec![TodoItem {
                status: TodoStatus::Pending,
                task: "任务".into(),
                reason: None,
            }],
        }];
        app.todo_hud.pinned = true;
        app.todo_hud.drag_origin = Some((4, 5));
        app.todo_hud.chip_area = Rect::new(1, 2, 3, 1);
        app.inflight_prompts.insert("ses_a".into());
        app.inflight_session_ops.insert("ses_a".into());
        app.interrupt_pending.insert("ses_a".into());
        app.register_tool_batch("batch_a".into(), ["call_a".into()]);

        // 切到一个空闲会话：本地状态清空，会话级记账仍留给原会话回收。
        app.switch_session(session("ses_b"), Vec::new());
        assert!(!app.prompt_running, "新会话按空闲呈现");
        assert!(!app.cancel_requested);
        assert!(app.ctrl_c_pending.is_none());
        assert!(app.prompt_started_at.is_none());
        assert!(app.thinking_started_at.is_none());
        assert!(app.transient_contract.is_none());
        assert_eq!(app.usage, Usage::default());
        assert_eq!(app.cost_usd, 0.0);
        assert!(app.todos.is_empty());
        assert!(!app.todo_hud.pinned, "todo HUD 状态不跨会话");
        assert!(app.todo_hud.drag_origin.is_none());
        assert!(app.todo_hud.chip_area.is_empty());
        assert!(app.tool_batches.is_empty(), "批次暂存不跨会话留存");
        assert!(app.inflight_prompts.contains("ses_a"), "在飞记账按会话保留");
        assert!(app.inflight_session_ops.contains("ses_a"));
        assert!(app.interrupt_pending.contains("ses_a"));

        // 切回仍在运行的会话：按记账继续呈现「运行中」，排队判定与停止键才对。
        app.switch_session(session("ses_a"), Vec::new());
        assert!(app.prompt_running, "切回运行中的会话应显示运行中");
    }

    /// 模型与模式在界面上一律用英文标识，且思考关闭时不出现在模型名里。
    #[test]
    fn labels_stay_english() {
        assert_eq!(model_selector_label("test/model:off"), "test/model");
        assert_eq!(model_selector_label("test/model:high"), "test/model:high");
        assert_eq!(model_selector_label("test/model"), "test/model");
        assert_eq!(mode_label(SessionMode::Normal), "normal");
        assert_eq!(mode_label(SessionMode::Ptc), "ptc");
        assert_eq!(mode_label(SessionMode::ClmExperimental), "(clm)");
    }

    /// 菜单命中只认边框之间的一行一条目；边框与菜单外都不算条目。
    #[test]
    fn mode_menu_hit_testing_skips_borders() {
        let menu = ModeMenu {
            selected: 1,
            area: Rect::new(10, 5, 24, 6),
        };
        assert_eq!(menu.entry_at(11, 5), None, "上边框不是条目");
        assert_eq!(menu.entry_at(11, 6), Some(0));
        assert_eq!(menu.entry_at(11, 8), Some(2));
        assert_eq!(menu.entry_at(11, 9), None, "末行之下不命中");
        assert_eq!(menu.entry_at(11, 10), None, "下边框不是条目");
        assert_eq!(menu.entry_at(9, 6), None, "左缘之外不命中");
        assert_eq!(menu.entry_at(34, 6), None, "右缘之外不命中");
        assert_eq!(menu.entry_at(11, 4), None, "菜单上方不命中");
    }

    /// 没有活动会话就不开菜单，只提示（此时图例本身显示 `idle`）。
    #[test]
    fn mode_menu_requires_an_active_session() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.open_mode_menu();
        assert!(app.mode_menu.is_none());
        assert_eq!(app.notice.as_deref(), Some("当前没有活动会话"));
    }

    /// HUD 交互：悬停展开、点击固定、取消固定后的抑制区、移出再进入重开。
    #[test]
    fn todo_hud_hover_pin_and_suppression() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.todo_hud.chip_area = Rect::new(70, 2, 10, 1);
        app.todo_hud.card_area = Rect::new(10, 2, 50, 10);

        assert!(app.todo_hud.pointer_moved((72, 2)));
        assert!(app.todo_hud.is_open(), "悬停胶囊应展开");

        assert!(app.todo_hud.pointer_down((12, 3)));
        assert!(app.todo_hud.pointer_up((12, 3)));
        assert!(app.todo_hud.pinned, "点击卡片应固定");
        assert!(!app.todo_hud.pointer_moved((5, 20)));
        assert!(app.todo_hud.is_open(), "固定后移出仍保持展开");

        assert!(app.todo_hud.pointer_down((12, 3)));
        assert!(app.todo_hud.pointer_up((12, 3)));
        assert!(!app.todo_hud.pinned);
        assert!(!app.todo_hud.is_open());
        assert!(app.todo_hud.pointer_moved((13, 4)));
        assert!(!app.todo_hud.is_open(), "指针未离开抑制区时不得重开");

        // 两个命中区分别判断：卡片与胶囊之间的空白不属于抑制区。
        assert!(!app.todo_hud.pointer_moved((65, 6)));
        assert!(!app.todo_hud.is_open());
        assert!(app.todo_hud.pointer_moved((72, 2)));
        assert!(app.todo_hud.is_open(), "离开后再进入胶囊可重新展开");
    }

    /// 拖动阈值：小位移仍是单击；越过阈值自动固定并夹在转写区内。
    #[test]
    fn todo_hud_drag_threshold_and_clamp() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.todo_hud.card_area = Rect::new(10, 2, 40, 8);
        app.todo_hud.hovered = true;
        let area = Rect::new(0, 0, 80, 24);

        assert!(app.todo_hud.pointer_down((12, 3)));
        // 按住卡片期间 Moved 落在旧命中区外也不能收起。
        assert!(app.todo_hud.pointer_moved((70, 20)));
        assert!(app.todo_hud.is_open());
        assert!(app.todo_hud.pointer_drag((14, 5), area));
        assert!(app.todo_hud.drag_origin.is_none(), "两格内不算拖动");
        assert!(!app.todo_hud.pinned);

        assert!(app.todo_hud.pointer_drag((200, 30), area));
        assert!(app.todo_hud.pinned, "拖动自动固定");
        assert_eq!(app.todo_hud.drag_origin, Some((40, 16)), "原点夹在转写区内");
        assert!(app.todo_hud.pointer_up((200, 30)));
        assert!(app.todo_hud.pinned, "拖动结束保持固定");
    }

    /// 非空更新保留固定；清空 todo 时 HUD 全量复位。
    #[test]
    fn todo_hud_resets_only_when_todos_clear() {
        use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.todo_hud.pinned = true;
        app.todo_hud.drag_origin = Some((4, 5));
        app.update_todos(vec![TodoPhase {
            phase: "阶段".into(),
            items: vec![TodoItem {
                status: TodoStatus::Pending,
                task: "任务".into(),
                reason: None,
            }],
        }]);
        assert!(app.todo_hud.pinned, "todo 仍在时保留固定");
        assert_eq!(app.todo_hud.drag_origin, Some((4, 5)));

        app.update_todos(Vec::new());
        assert!(!app.todo_hud.pinned);
        assert!(app.todo_hud.drag_origin.is_none());
    }

    #[test]
    fn finish_turn_reports_stop_after_cancel_request() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.prompt_running = true;
        app.cancel_requested = true;
        app.finish_prompt();

        assert!(!app.prompt_running);
        assert!(!app.cancel_requested);
        assert_eq!(app.notice.as_deref(), Some("已停止"));
    }

    /// 思考 token 只回填本次响应的思考块：下一响应若没有思考块，
    /// 上游报 0 不得把上一块已记下的 token 数抹掉。
    #[test]
    fn usage_reasoning_tokens_stay_with_their_response() {
        use super::{App, Usage};
        use crate::transcript::Block;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let tokens = |app: &App| match &app.blocks[0].block {
            Block::Thinking {
                reasoning_tokens, ..
            } => *reasoning_tokens,
            _ => panic!("期望思考块"),
        };

        app.start_thinking();
        app.finish_thinking();
        app.apply_usage(Usage {
            reasoning_tokens: Some(120),
            ..Usage::default()
        });
        assert_eq!(tokens(&app), Some(120));

        app.start_generation();
        app.apply_usage(Usage {
            reasoning_tokens: Some(0),
            ..Usage::default()
        });
        assert_eq!(tokens(&app), Some(120), "没有思考块的响应不能覆盖上一块");
    }

    /// 会话累计用量：请求级 usage 与累计统计分开存，状态栏的 in/out/tps 只读后者。
    #[test]
    fn session_usage_is_authoritative_and_ordered() {
        use super::{App, Usage};
        use moc_protocol::{ModelUsageSummary, SessionUsageResult};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let summary = |output: u64, active_ms: u64| ModelUsageSummary {
            model: "p/m".into(),
            calls: 2,
            input_tokens: 12_000,
            output_tokens: output,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            reasoning_tokens: None,
            active_ms,
            ttft_ms: 12_000,
            ttft_calls: 2,
            gen_ms: active_ms.saturating_sub(12_000),
        };
        let usage = |output: u64, active_ms: u64| SessionUsageResult {
            models: Vec::new(),
            total: summary(output, active_ms),
            tool_ms: 0,
        };

        let first = app.begin_usage_request();
        app.apply_session_usage(first, usage(1_200, 60_000));
        assert_eq!(app.session_tokens(), (12_000, 1_200));
        // TPS 只除生成阶段（60s - 12s），不把首 token 等待算进去。
        assert!((app.session_tokens_per_second() - 25.0).abs() < 1e-9);

        // 旧会话没有首 token 数据：退回整段用时。
        let mut legacy = usage(1_200, 60_000);
        legacy.total.gen_ms = 0;
        legacy.total.ttft_ms = 0;
        legacy.total.ttft_calls = 0;
        let legacy_request = app.begin_usage_request();
        app.apply_session_usage(legacy_request, legacy);
        assert!((app.session_tokens_per_second() - 20.0).abs() < 1e-9);
        let refreshed = app.begin_usage_request();
        app.apply_session_usage(refreshed, usage(1_200, 60_000));

        // 请求级 usage 不参与累计口径。
        app.apply_usage(Usage {
            input_tokens: 400,
            output_tokens: 80,
            ..Usage::default()
        });
        assert_eq!(app.session_tokens(), (12_000, 1_200));

        // 更旧的请求迟到：不得回退统计。
        app.apply_session_usage(first, usage(1, 1));
        assert_eq!(app.session_tokens(), (12_000, 1_200));

        let second = app.begin_usage_request();
        app.apply_session_usage(second, usage(2_400, 120_000));
        assert_eq!(app.session_tokens(), (12_000, 2_400));

        // 切会话：旧统计清空，在飞回执作废。
        app.switch_session(
            moc_protocol::SessionInfo {
                id: "ses_next".into(),
                title: "下一个".into(),
                model: "p/m".into(),
                permission: moc_protocol::SessionPermission::Full,
                mode: moc_protocol::SessionMode::Normal,
                plan_active: false,
                mode_locked: false,
                parent_session_id: None,
                forked_from: None,
                cwd: "/tmp".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
                lock: None,
            },
            Vec::new(),
        );
        assert_eq!(app.session_tokens(), (0, 0));
        app.apply_session_usage(second, usage(9_999, 1_000));
        assert_eq!(app.session_tokens(), (0, 0), "跨会话迟到的回执必须丢弃");
    }

    /// 告警与 notice 的 TTL：期内可见且给出未来的截止时刻，过期后两者都消失。
    #[test]
    fn warning_and_notice_expire_after_their_ttl() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);

        app.set_warning("effort max → high");
        app.set_notice("已停止");
        assert!(app.warning_visible());
        assert!(app.notice_visible());
        // 截止时刻必须落在未来，事件循环才能安全 sleep_until 而非空转
        assert!(app.warning_deadline().unwrap() > std::time::Instant::now());
        assert!(app.notice_deadline().unwrap() > std::time::Instant::now());

        let stale = std::time::Instant::now() - std::time::Duration::from_secs(600);
        app.warning_at = Some(stale);
        app.notice_at = Some(stale);
        assert!(!app.warning_visible());
        assert!(!app.notice_visible());
        assert!(app.warning_deadline().is_none());
        assert!(app.notice_deadline().is_none());
        // 过期的槽位不再需要重绘，事件循环据此不再安排超时分支
        assert_eq!(app.warning.as_deref(), Some("effort max → high"));
    }

    /// `/retry` 已把失败的那一轮从会话里剪掉：转写里对应的块也必须撤掉。
    #[test]
    /// `/retry` 撤掉的是「没跑完」的那一轮，已经落库的内容一律留着。
    fn rewind_drops_the_failed_tail_of_the_last_turn() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.push_block(crate::transcript::Block::User {
            text: "先看看仓库".into(),
        });
        app.push_block(crate::transcript::Block::Assistant {
            text: "上一轮的回答".into(),
        });
        app.push_block(crate::transcript::Block::User {
            text: "继续".into(),
        });
        app.push_block(crate::transcript::Block::Assistant {
            text: "半截正文".into(),
        });
        app.push_block(crate::transcript::Block::Note {
            text: "错误：连接中断".into(),
            error: true,
        });

        assert!(app.rewind_failed_turn(false));

        assert_eq!(app.blocks.len(), 3, "只撤最后一条用户消息之后的块");
        assert!(
            matches!(&app.blocks[2].block, crate::transcript::Block::User { text } if text == "继续")
        );
        // 尾部本来就没有可撤的块：不报撤过，免得把上一轮的正文当成失败回合吃掉。
        assert!(!app.rewind_failed_turn(false));
        assert_eq!(app.blocks.len(), 3);
    }

    #[test]
    fn failed_generation_removes_only_uncommitted_preview() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.push_block(crate::transcript::Block::Assistant {
            text: "已提交正文".into(),
        });
        app.start_generation();
        assert!(
            !app.has_streaming(|block| matches!(block, crate::transcript::Block::Assistant { .. }))
        );
        app.push_block(crate::transcript::Block::Assistant {
            text: "异常重复预览".into(),
        });
        app.discard_generation();
        assert_eq!(app.blocks.len(), 1);
        assert!(
            matches!(&app.blocks[0].block, crate::transcript::Block::Assistant { text } if text == "已提交正文")
        );
    }

    /// 正文继续流式增长时，旧图表状态必须立刻作废，避免 index 漂移后画旧图。
    #[test]
    fn streaming_append_invalidates_chart_state() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.scrollback_area = Rect::new(3, 0, 80, 30);
        app.push_block(crate::transcript::Block::Assistant {
            text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
        });
        app.toggle_table_chart(0, 0);
        assert!(app.blocks[0].table_chart(0).is_some());
        app.append_streaming(
            |block| match block {
                crate::transcript::Block::Assistant { text } => Some(text),
                _ => None,
            },
            "\n\n追加正文",
        );
        assert!(
            app.blocks[0].table_chart(0).is_none(),
            "正文变化后旧图必须清掉"
        );
        assert!(
            matches!(&app.blocks[0].block, crate::transcript::Block::Assistant { text } if text.ends_with("追加正文"))
        );
    }
}
