//! 应用状态：会话、模型、转写块、折叠开关与界面数据。
//!
//! 状态集中在一个 `App` 结构上，`update` 全部由事件循环驱动；渲染层只读。

use std::collections::{HashMap, VecDeque};

use moc_protocol::methods::TodoPhase;
use moc_protocol::{
    ContextUsageResult, ModelInfo, SessionEngine, SessionInfo, SessionPermission,
    SessionUsageResult, Usage,
};

use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;

use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::theme::Theme;
use crate::transcript::{Block, DetailLevel, Entry};
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
/// 选区复制后的轻量反馈只短暂留在输入框下边框。
const SELECTION_COPY_TTL: std::time::Duration = std::time::Duration::from_secs(2);
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
fn compaction_running_prefix() -> std::borrow::Cow<'static, str> {
    rust_i18n::t!("tui.compaction.running_prefix")
}

/// 进行中提示的文案。
fn compaction_running_note(secs: u64) -> String {
    rust_i18n::t!("tui.compaction.running_fmt", secs = secs).into_owned()
}

/// 一次模型生成失败后的等待重试：警告便签的秒数原地倒数。
///
/// 与 [`CompactionProgress`] 同款的进行中便签；`head`/`tail` 是秒数两侧的静态
/// 片段，`message` 是 core 的完整原文（定稿时原样写回）。
#[derive(Debug, Clone)]
pub struct RetryWait {
    /// 发起提示的会话；切到别的会话后不再更新它。
    pub session_id: String,
    /// 便签在 `blocks` 里的下标（历史分页前移时要同步平移）。
    pub note_index: usize,
    /// 下次尝试的截止时刻；按 core 下发的等待时长在本地推算。
    pub deadline: std::time::Instant,
    /// 已写进便签的剩余秒文本；只有它变了才重写便签。
    pub shown: String,
    /// 秒数前缀（含结尾的「，」）。
    pub head: String,
    /// 秒数之后的全部静态内容（「秒后自动重试…」+ 失败原因）。
    pub tail: String,
    /// core 下发的完整文案；等待结束定稿时写回便签。
    pub message: String,
}

/// 转写区右缘滚动条上一帧的几何：点击与拖拽都按它换算滚动位置。
#[derive(Debug, Clone, Copy, Default)]
pub struct ScrollbackScrollbar {
    /// 轨道命中区域（屏幕坐标，包含悬停展开所需的右侧留白）。
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
    pub recent_models: crate::model_history::RecentModels,
    /// `config/get` 下发的 `[models.fusion]` 默认组合；双侧都配置时才有值。
    pub fusion_defaults: Option<moc_protocol::methods::FusionPair>,
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
    /// 当前会话目标（`GoalChanged` 最新快照）；无目标为 `None`。
    pub goal: Option<moc_protocol::methods::GoalView>,
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
    /// 打开的模态；更换时统一使用 `set_modal` / `take_modal`。
    pub modal: Option<Modal>,
    /// 当前模态已在本帧实际绘制。
    pub modal_drawn: bool,
    /// 当前模态已走过至少一帧；尺寸不足未绘制时也允许普通 Esc 退出。
    pub modal_frame_seen: bool,
    /// 等待作答的授权与提问，共用 FIFO 并持有各自的答复发送端。
    pub pending_modals: VecDeque<Modal>,
    /// 当前焦点。
    pub focus: Focus,
    /// 旁路问答只驻留内存，独立于主转写、输入历史及用量。
    pub btw: crate::btw::BtwState,
    /// 最近一次绘制的输入框区域，用于鼠标点击恢复焦点。
    pub prompt_area: Rect,
    /// 顶部上下文占用区域，用于鼠标点击打开详情。
    pub context_area: Rect,
    /// 输入框下边框右端引擎名的屏幕区域（鼠标点击就地弹出引擎菜单）。
    pub engine_area: Rect,
    /// 输入框下边框模型名区域（鼠标点击打开模型选择）。
    pub model_tag_area: Rect,
    /// 输入框下边框权限区域（鼠标点击就地弹出权限菜单）。
    pub permission_tag_area: Rect,
    /// 模态选项行命中矩形（Permission、Model、Sessions 等）。
    pub modal_item_hits: Vec<(usize, Rect)>,
    /// 模型选择器详情卡控制行的命中矩形：`(详情行, 区域)`。
    pub modal_detail_hits: Vec<(crate::modal::DetailRow, Rect)>,
    /// 模型选择器下拉候选的命中矩形：`(过滤后候选下标, 区域)`。
    pub modal_menu_hits: Vec<(usize, Rect)>,
    /// 通用模态卡片的外框；点到卡片外即关闭（仅浏览/选择类模态）。
    pub modal_card_area: Rect,
    /// 模型选择器详情卡的外框（点外关闭判定用；未绘制时为空）。
    pub modal_detail_area: Rect,
    /// 模型选择器下拉浮层的外框（点外关闭判定用；未展开时为空）。
    pub modal_menu_area: Rect,
    /// 本帧画出的可点击键位（提示条、回退 dock 等）：点中等价于按下该键。
    /// 模态绘制时整表重建，底层界面的键位不会漏到模态上。
    pub key_hits: Vec<crate::render::labels::KeyHit>,
    /// 输入框下边框左端提示（空闲「快捷键」、运行中「停止」）的命中区。
    pub legend_tip_area: Rect,
    /// workflow 胶囊命中区（活跃 run / 待审批徽标）。
    pub workflow_capsule_area: Rect,
    /// 聚焦子代理时头部「Esc 返回」的命中区。
    pub header_back_area: Rect,
    /// statusline 右侧会话用量统计段的命中区（点击与 `/usage` 同义）。
    pub usage_area: Rect,
    /// 「回到底部」浮动胶囊的命中区；滚离底部才登记，不占转写行数。
    pub jump_bottom_area: Rect,
    /// 计划审阅中的执行模型文本命中区。
    /// 计划审阅正文区域（鼠标命中换算用；计划/源码标签共用）。
    pub plan_body_area: Rect,
    /// 计划源码标签可见源码行号（屏幕行 → 0-based 源码行）。
    pub plan_row_map: Vec<usize>,
    /// 计划标签渲染行 → 源码行区间（与 plan_body_area 上方 scroll_offset 对齐）。
    pub plan_line_sources: Vec<Option<(usize, usize)>>,
    /// 计划审阅标签栏条目命中区（渲染写入，鼠标只读）。
    pub plan_tab_hits: Vec<(crate::modal::PlanTab, Rect)>,
    /// 计划审阅选区条按钮命中区。
    pub plan_selbar_hits: Vec<(crate::modal::PlanSelAction, Rect)>,
    /// 计划审阅动作区控件命中区（芯片 + 按钮）。
    pub plan_control_hits: Vec<(crate::modal::PlanControl, Rect)>,
    /// 批注编辑器按钮命中区。
    pub plan_editor_hits: Vec<(crate::modal::PlanEditorAction, Rect)>,
    /// 计划审阅正文滚动条轨道命中区（溢出时才登记；空矩形 = 无滚动条）。
    pub plan_scrollbar_area: Rect,
    /// 计划审阅当前标签的正文总行数（滚动条滑块与拖拽换算用）。
    pub plan_body_total: usize,
    /// 从模型芯片打开选择器时停放的计划审阅状态；选择器关闭后还原。
    pub parked_plan: Option<Box<crate::modal::PlanReviewState>>,
    /// 模型选择器的锚点覆盖：从计划模型芯片打开时记芯片矩形，让选择器
    /// 贴着审阅卡弹出而非回落到输入框图例；选择器关闭时清空。
    pub model_picker_anchor: Option<Rect>,
    /// 消息回退时间线区域（鼠标滚动/点击换算用）。
    pub rewind_timeline_area: Rect,
    /// 消息回退详情区域（鼠标滚动换算用）。
    pub rewind_detail_area: Rect,
    /// 提问卡片各选项（含自定义行）的命中矩形；`options.len()` 下标代表自定义行。
    /// 每帧渲染时重建，下标越界（问题切换后）自然落空。
    pub ask_option_hits: Vec<(usize, Rect)>,
    /// 提问卡片顶部问题导航条各问的命中矩形（点击跳问）。
    pub ask_nav_hits: Vec<(usize, Rect)>,
    /// 提问卡片整体区域：点击卡片外的转写区折叠。
    pub ask_card_area: Rect,
    /// 问题正文窗口区域：滚轮悬停其上 = 滚动问题正文而非动选项光标。
    pub ask_question_area: Rect,
    /// 详情区区域（单栏时的底部聚焦项描述区，或双栏时的右栏预览窗）：
    /// 滚轮悬停其上 = 滚动详情区。
    pub ask_detail_area: Rect,
    /// 折叠细条区域（占据输入框位；点击展开回作答态）。
    pub ask_strip_area: Rect,
    /// 就地引擎菜单；`None` = 关闭。
    pub engine_menu: Option<EngineMenu>,
    /// 就地权限菜单；`None` = 关闭。与引擎菜单同类互斥。
    pub permission_menu: Option<PermissionMenu>,
    /// 当前这条用户消息的临时模式覆盖（`/ask` 注入）；整条消息回复完毕才清空。
    ///
    /// 只影响图例与状态栏的展示，会话引擎与 plan 叠加态仍以 `session` 为准。
    pub transient_mode: Option<moc_protocol::methods::PromptMode>,
    /// Core 估算的上下文组成与总量。
    pub context_usage: Option<ContextUsageResult>,
    /// 滚动偏移（自尾部算起的行数；0 = 跟随尾部）。
    pub scroll_offset: usize,
    /// 是否有进行中的提示。
    pub prompt_running: bool,
    /// 独立技能摘要操作；占用前台，状态只显示为不落盘的本地 Note。
    pub skills_compacting: bool,
    /// 是否已经发出取消当前提示的请求。
    pub cancel_requested: bool,
    /// 当前提示开始时间（statusline 计时）。
    pub prompt_started_at: Option<std::time::Instant>,
    /// 进行中的上下文压缩提示；`None` = 当前没有压缩在等模型。
    pub compaction_progress: Option<CompactionProgress>,
    /// 等待自动重试的倒数便签；`None` = 当前不在重试等待中。
    pub retry_wait: Option<RetryWait>,
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
    /// DEC 2026 同步输出是否启用（启动探测；`TerminalGuard::draw` 消费，
    /// `/doctor` 展示同一份）。
    pub sync_output: bool,
    /// kitty 键盘消歧义协商结果（`/doctor` 展示用）。
    pub keyboard_enhanced: bool,
    /// 内联图片的解码 / 编码缓存（工具结果、markdown 图片、SVG 代码块共用）。
    pub images: crate::render::image::ImageStore,
    /// 本帧要贴的位图槽位（`ScrollbackView::paint` 填充，帧后由
    /// [`crate::render::graphics::emit`] 消费）。
    pub image_slots: Vec<ImageSlot>,
    /// 本帧可点击的表格入口（`paint` 填充，命中判定读它）。
    pub chart_hits: Vec<ChartHit>,
    /// 本帧可点击的参考链接折叠入口（`paint` 填充，命中判定读它）。
    pub link_hits: Vec<LinkHit>,
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
    /// 鼠标悬停的参考链接入口（块下标）。
    pub hover_link: Option<usize>,
    /// 鼠标拖选区（屏幕坐标 (列, 行) 起点 / 终点）。
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// 按下未抬起的位置（区分点击与拖选）。
    pub pending_click: Option<(u16, u16)>,
    /// 最近一次选区复制的时刻；反馈不占用通知槽位。
    pub selection_copied_at: Option<std::time::Instant>,
    /// 滚动条拖拽；`Some` 时左键拖动只改滚动偏移，不进入文本选区。
    pub scrollbar_drag: Option<ScrollbackDrag>,
    /// 最近的鼠标坐标；绘制时按当前轨道判定悬停，避免布局变化后沿用旧命中状态。
    pub scrollbar_pointer: Option<(u16, u16)>,
    /// ctrl+c 二次确认状态及触发时刻（3 秒内有效）。
    pub ctrl_c_pending: Option<(CtrlCPending, std::time::Instant)>,
    /// Esc 二次确认状态。
    pub clear_pending: bool,
    /// 一次性提示（操作反馈、启动警告）；带 TTL 自动消失。
    pub notice: Option<String>,
    /// 当前 notice 的设置时刻（toast 超时判定）。
    pub notice_at: Option<std::time::Instant>,
    /// 当前 notice 是否报告失败（失败用错误色，其余操作反馈用中性强调色）。
    pub notice_error: bool,
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
    pub startup_engine: SessionEngine,
    /// 启动时新会话是否直接进 plan 叠加态（`--plan`）。
    pub startup_plan: bool,
    pub startup_pending: Option<String>,
    /// 命令注册表：内置命令元数据 + 本工作区插件命令的动态来源。
    pub commands: crate::commands::Registry,
    /// `config/get` 下发的特性开关快照；命令门控与引擎清单共用。
    pub features: moc_protocol::features::FeatureStates,
    /// 最近一次 `plugin/list` 的结果，供 `/plugins` 展示。
    pub plugins: Vec<moc_protocol::methods::PluginInfo>,
    pub subagents: std::collections::BTreeMap<String, crate::subagent::SubagentView>,
    pub subagent_selected: Option<String>,
    pub subagent_area: Rect,
    pub subagent_hits: Vec<(String, Rect)>,
    /// 侧栏标题行右端 `×` 的命中区（侧栏不画时清空）。
    pub subagent_close_area: Rect,
    pub subagent_scroll: usize,
    pub subagent_generation: u64,
    /// 侧栏显隐覆盖：`None` = 自动（有任一 running 子代理就显示）。
    /// 用户手动切换后记住选择；新一轮代理开跑时回自动。
    pub subagent_sidebar: Option<bool>,
    /// 上一轮名册状态里是否存在 running（sync 时检测「无 → 有」边沿）。
    subagent_running_seen: bool,
    /// 聚焦中的子代理；`Some` 时当前会话就是它，`Esc` 或双击 `←` 返回。
    pub focused: Option<FocusedAgent>,
    /// 返回主会话时要还原的输入草稿与滚动偏移。
    pub focus_restore: Option<(String, usize)>,
    /// workflow 状态栏胶囊：待审批数与活跃 run。
    pub workflow: crate::workflow::WorkflowCapsule,
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

/// 一个可点击的参考链接折叠入口：列区间限定在按钮文字上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkHit {
    pub entry: usize,
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
/// 参见 `docs/tui.md` 的子代理一节：点子代理侧栏的卡片进入，
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

mod input;
mod pickers;
mod progress;
mod session;
mod status;
mod transcript;
mod views;

impl App {
    pub fn new(theme: &'static Theme, events: UnboundedSender<AppEvent>) -> Self {
        let images = crate::render::image::ImageStore::new(events.clone());
        Self {
            theme,
            events,
            session: None,
            fusion_defaults: None,
            read_only: false,
            sandbox_modes: vec![SessionPermission::Full],
            models: Vec::new(),
            recent_models: crate::model_history::RecentModels::default(),
            blocks: Vec::new(),
            generation_start: None,
            history_has_more: false,
            history_before_entry_id: None,
            history_loading: false,
            history_load_requested: false,
            scroll_max_offset: 0,
            todos: Vec::new(),
            goal: None,
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
            modal_drawn: false,
            modal_frame_seen: false,
            pending_modals: VecDeque::new(),
            focus: Focus::Prompt,
            btw: crate::btw::BtwState::default(),
            prompt_area: Rect::default(),
            context_area: Rect::default(),
            engine_area: Rect::default(),
            model_tag_area: Rect::default(),
            permission_tag_area: Rect::default(),
            modal_item_hits: Vec::new(),
            modal_detail_hits: Vec::new(),
            modal_menu_hits: Vec::new(),
            modal_card_area: Rect::default(),
            modal_detail_area: Rect::default(),
            modal_menu_area: Rect::default(),
            key_hits: Vec::new(),
            legend_tip_area: Rect::default(),
            workflow_capsule_area: Rect::default(),
            header_back_area: Rect::default(),
            usage_area: Rect::default(),
            plan_body_area: Rect::default(),
            plan_row_map: Vec::new(),
            plan_line_sources: Vec::new(),
            plan_tab_hits: Vec::new(),
            plan_selbar_hits: Vec::new(),
            plan_control_hits: Vec::new(),
            plan_editor_hits: Vec::new(),
            plan_scrollbar_area: Rect::default(),
            plan_body_total: 0,
            parked_plan: None,
            model_picker_anchor: None,
            rewind_timeline_area: Rect::default(),
            rewind_detail_area: Rect::default(),
            ask_option_hits: Vec::new(),
            ask_nav_hits: Vec::new(),
            ask_card_area: Rect::default(),
            ask_question_area: Rect::default(),
            ask_detail_area: Rect::default(),
            ask_strip_area: Rect::default(),
            engine_menu: None,
            permission_menu: None,
            transient_mode: None,
            context_usage: None,
            scroll_offset: 0,
            prompt_running: false,
            skills_compacting: false,
            cancel_requested: false,
            prompt_started_at: None,
            compaction_progress: None,
            retry_wait: None,
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
            jump_bottom_area: Rect::default(),
            scrollbar: ScrollbackScrollbar::default(),
            mouse_hits: Vec::new(),
            paint_lines: Vec::new(),
            graphics: None,
            sync_output: false,
            keyboard_enhanced: false,
            images,
            image_slots: Vec::new(),
            chart_hits: Vec::new(),
            link_hits: Vec::new(),
            image_placed: Vec::new(),
            image_covered: false,
            image_resume: false,
            image_repaint: false,
            hover_chart: None,
            hover_link: None,
            selection: None,
            pending_click: None,
            selection_copied_at: None,
            scrollbar_drag: None,
            scrollbar_pointer: None,
            ctrl_c_pending: None,
            clear_pending: false,
            notice: None,
            notice_at: None,
            notice_error: false,
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
            startup_engine: SessionEngine::Standard,
            startup_plan: false,
            startup_pending: None,
            commands: crate::commands::registry(),
            features: moc_protocol::features::FeatureStates::defaults(),
            plugins: Vec::new(),
            subagents: std::collections::BTreeMap::new(),
            subagent_selected: None,
            focused: None,
            focus_restore: None,
            workflow: crate::workflow::WorkflowCapsule::default(),
            left_tap_pending: None,
            subagent_area: Rect::default(),
            subagent_hits: Vec::new(),
            subagent_close_area: Rect::default(),
            subagent_scroll: 0,
            subagent_generation: 0,
            subagent_sidebar: None,
            subagent_running_seen: false,
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

    /// 换窗时作废绘制状态，避免旧卡片的输入误答新请求。
    pub fn set_modal(&mut self, modal: Option<Modal>) {
        self.modal = modal;
        self.modal_drawn = false;
        self.modal_frame_seen = false;
    }

    /// 取出模态时同样作废绘制状态。
    pub fn take_modal(&mut self) -> Option<Modal> {
        self.modal_drawn = false;
        self.modal_frame_seen = false;
        self.modal.take()
    }

    /// 授权与提问互不覆盖；已有队列时新请求也不能插队。
    pub fn enqueue_request_modal(&mut self, modal: Modal) {
        debug_assert!(modal.request_session_id().is_some());
        if matches!(
            self.modal,
            Some(Modal::Permission { .. } | Modal::Ask { .. })
        ) || !self.pending_modals.is_empty()
        {
            self.pending_modals.push_back(modal);
            self.show_next_pending_modal();
        } else {
            // 无待答请求时，沿用请求弹窗替换其他浏览窗口的行为。
            self.set_modal(Some(modal));
            self.focus = Focus::Prompt;
        }
    }

    /// 其他窗口若替换了当前请求，仍沿用原来的取消语义；队列等待窗口空闲。
    pub fn show_next_pending_modal(&mut self) -> bool {
        // 接收端已取消的请求无需再展示；同步检查不等待任务退出。
        self.pending_modals.retain(|modal| match modal {
            Modal::Permission { reply, .. } => reply.as_ref().is_some_and(|tx| !tx.is_closed()),
            Modal::Ask { reply, .. } => reply.as_ref().is_some_and(|tx| !tx.is_closed()),
            _ => true,
        });
        if self.modal.is_none()
            && let Some(modal) = self.pending_modals.pop_front()
        {
            self.set_modal(Some(modal));
            self.focus = Focus::Prompt;
            return true;
        }
        false
    }

    /// 关闭或离开会话时丢弃未答请求，由发送端析构维持原有取消语义。
    pub fn cancel_session_modals(&mut self, session_id: &str) {
        self.pending_modals
            .retain(|modal| modal.request_session_id() != Some(session_id));
        if self.modal.as_ref().and_then(Modal::request_session_id) == Some(session_id) {
            self.set_modal(None);
        }
        self.show_next_pending_modal();
    }

    /// 设置一次性提示并刷新计时（操作反馈、引导，中性配色）。
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.notice_at = Some(std::time::Instant::now());
        self.notice_error = false;
    }

    /// 设置报告失败的一次性提示（错误配色）。
    pub fn set_error_notice(&mut self, text: impl Into<String>) {
        self.set_notice(text);
        self.notice_error = true;
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

    /// 保留过期的反馈截止时刻直到事件循环消费，避免其他工作延迟后漏掉重绘。
    pub fn selection_copy_deadline(&self) -> Option<std::time::Instant> {
        self.selection_copied_at.map(|at| at + SELECTION_COPY_TTL)
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

    /// 有在飞提示或会话操作：`available_during_task=false` 的命令此时不提示也不执行。
    pub fn busy(&self) -> bool {
        self.prompt_running || !self.inflight_session_ops.is_empty()
    }

    // ---------- 输入框编辑（委托给 PromptState） ----------

    // ---------- 输入历史 ----------

    // ---------- statusline 数据 ----------
}

#[cfg(test)]
mod streaming_budget_tests;

#[cfg(test)]
mod detail_control_tests;

#[cfg(test)]
mod tool_recovery_tests;

/// 会话引擎的展示名；CLM 以括号标识，保持实验状态始终可见。
pub fn engine_label(engine: SessionEngine) -> &'static str {
    match engine {
        SessionEngine::Standard => "standard",
        SessionEngine::Ptc => "ptc",
        SessionEngine::ClmExperimental => "(clm)",
    }
}

/// 会话引擎顺序：就地菜单条目与键盘循环共用同一份顺序。
/// plan/ask 是模式不是引擎，不在此列。
pub const ENGINES: [SessionEngine; 3] = [
    SessionEngine::Standard,
    SessionEngine::Ptc,
    SessionEngine::ClmExperimental,
];

/// Todo HUD 的一次左键按下：区分单击固定与拖动卡片。
#[derive(Debug, Clone, Copy)]
pub struct TodoHudPress {
    start: (u16, u16),
    card_origin: (u16, u16),
    from_card: bool,
    dragging: bool,
}

/// 右上角 todo HUD 的交互与命中状态：缩小胶囊与展开卡片互斥，单击切换。
#[derive(Debug, Default)]
pub struct TodoHudState {
    /// 上一帧折叠胶囊的屏幕命中区；展开时为空。
    pub chip_area: Rect,
    /// 上一帧展开卡片的屏幕命中区；折叠时为空。
    pub card_area: Rect,
    /// 缩小态：只显示右上角胶囊；默认展开态只显示卡片。
    pub collapsed: bool,
    /// 左键按下信息。
    press: Option<TodoHudPress>,
    /// 拖动后的卡片原点；会话内保留，展开时生效。
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

mod menus;
mod todo_hud;
pub use menus::initial_fusion_draft;
use menus::model_cost;
pub use menus::model_selector_label;

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

/// 状态图例就地菜单条目的展示契约：标签 + 侧边提示框里的一句说明。
pub trait LegendItem {
    /// 菜单里显示的名字（与图例用词一致）。
    fn label(&self) -> &'static str;
    /// 侧边提示框对高亮项的一句话说明。
    fn description(&self) -> std::borrow::Cow<'static, str>;
}

/// 点击状态图例后就地弹出的选择菜单（引擎、权限共用）。
pub struct LegendMenu<T> {
    /// 键盘 / 悬停高亮的条目下标（`items` 顺序）。
    pub selected: usize,
    /// 可选项快照：打开菜单时按特性 flag / 平台能力过滤后的清单。
    pub items: Vec<T>,
    /// 上一帧绘制的菜单区域；鼠标命中使用。
    pub area: Rect,
}

/// 引擎图例菜单。
pub type EngineMenu = LegendMenu<SessionEngine>;
/// 权限图例菜单。
pub type PermissionMenu = LegendMenu<SessionPermission>;

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
mod tests;
