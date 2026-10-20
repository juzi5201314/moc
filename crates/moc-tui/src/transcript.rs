//! 转写块模型：一次会话在 TUI 里的线性呈现单元。
//!
//! [`Entry`] 包着 [`Block`] 与内容版本号 `gen`：渲染缓存以 `(gen, 宽度)`
//! 为 key,块内容每次变化 `gen` 自增,流式增量只让**活动的那一块**缓存失效。
//! 协议的 `SessionUpdate` 在 `crate::event` 里被折叠成这些块。

use moc_protocol::ToolResult;
use moc_protocol::methods::{TodoPhase, ToolDiff, ToolStatus};
use serde::{Deserialize, Serialize};

/// 转写内容的显示档位（ctrl+t 管思考、ctrl+o 管工具，逐档循环）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    /// 只留标题／摘要行。
    Hidden,
    /// 少量预览行：思考末尾 [`crate::render::scrollback::BRIEF_THINKING_ROWS`] 行、
    /// 工具正文 [`crate::render::scrollback::BRIEF_BODY_ROWS`] 行。
    #[default]
    Brief,
    /// 正文全文，不做截断。
    Full,
}

impl DetailLevel {
    /// 循环到下一档：隐藏 → 简略 → 完整 → 隐藏。
    pub fn next(self) -> Self {
        match self {
            Self::Hidden => Self::Brief,
            Self::Brief => Self::Full,
            Self::Full => Self::Hidden,
        }
    }

    /// 档位的中文名（切换提示用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Hidden => "隐藏",
            Self::Brief => "简略",
            Self::Full => "完整",
        }
    }
}

/// 当前时刻的 UNIX 毫秒时间戳。
pub fn now_ms() -> u64 {
    let Ok(since_epoch) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return 0;
    };
    // u128 毫秒 → u64：毫秒计数到 u64 溢出是 5.8 亿年后的事
    u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
}

/// 转写条目：块 + 内容版本（渲染缓存 key 的一部分）。
#[derive(Debug, Clone)]
pub struct Entry {
    pub generation: u64,
    pub block: Block,
    /// 单块档位覆盖；`None` 表示跟随所在层的全局档位。
    pub level_override: Option<DetailLevel>,
    /// 块落地时刻（UNIX 毫秒）：用户消息与助手回复据此显示时间戳。
    /// `None` 表示无从得知（如从历史恢复的会话）。
    pub at_ms: Option<u64>,
    /// 该块所在提示的耗时（毫秒）：提示结束时回填到助手块，
    /// 渲染为 `Worked for` 尾注。
    pub elapsed_ms: Option<u64>,
    /// 正文里每张顶层表格的图表视图态（下标 = 表格序号）。
    ///
    /// 纯进程内视图态：不进会话文件、不随历史恢复回填；追加正文时
    /// 整表清空——表格内容变了，旧的图表数据立刻作废。
    pub charts: Vec<Option<TableChartState>>,
}

impl Entry {
    pub fn new(block: Block) -> Self {
        Self {
            generation: 0,
            block,
            level_override: None,
            at_ms: None,
            elapsed_ms: None,
            charts: Vec::new(),
        }
    }

    /// 实时创建的块：带上落地时刻（历史恢复的块走 [`Entry::new`]，时间未知）。
    pub fn timed(block: Block) -> Self {
        Self {
            at_ms: Some(now_ms()),
            ..Self::new(block)
        }
    }

    /// 该块第 `table` 张顶层表格的图表态。
    pub fn table_chart(&self, table: usize) -> Option<&TableChartState> {
        self.charts.get(table).and_then(Option::as_ref)
    }

    /// 同上，可变访问：贴图时回填已编码的载荷。
    pub fn table_chart_mut(&mut self, table: usize) -> Option<&mut TableChartState> {
        self.charts.get_mut(table).and_then(Option::as_mut)
    }

    /// 设置/清除某张表格的图表态，并让渲染缓存失效。
    pub fn set_table_chart(&mut self, table: usize, state: Option<TableChartState>) {
        if self.charts.len() <= table {
            self.charts.resize_with(table + 1, || None);
        }
        self.charts[table] = state;
        self.touch();
    }

    /// 内容变化时统一走这里，保证缓存失效。
    pub(crate) fn touch(&mut self) {
        self.generation += 1;
    }

    /// 提示收尾：把结束时刻与耗时回填到助手块（`Worked for` 尾注 + 回复时间）。
    pub fn stamp_prompt(&mut self, at_ms: u64, elapsed_ms: u64) {
        if !matches!(self.block, Block::Assistant { .. }) {
            return;
        }
        self.at_ms = Some(at_ms);
        self.elapsed_ms = Some(elapsed_ms);
        self.touch();
    }

    /// 该块渲染用的档位：单块覆盖优先，否则用调用方给的层档位。
    pub fn level(&self, axis: DetailLevel) -> DetailLevel {
        self.level_override.unwrap_or(axis)
    }

    /// 切换单块档位：非完整 → 完整；完整 → 当前层的收起形态
    /// （简略层回到简略，其余回到隐藏）。目标与层档位相同时不留覆盖。
    pub fn toggle_level(&mut self, axis: DetailLevel) {
        if !self.block.foldable() {
            return;
        }
        let collapsed = if axis == DetailLevel::Brief {
            DetailLevel::Brief
        } else {
            DetailLevel::Hidden
        };
        self.level_override = match self.level(axis) {
            DetailLevel::Full => (collapsed != axis).then_some(collapsed),
            _ => Some(DetailLevel::Full),
        };
        self.generation += 1;
    }

    /// 丢弃单块覆盖，让该块重新跟随所在层的全局档位。
    pub fn clear_level_override(&mut self) {
        if self.level_override.take().is_some() {
            self.generation += 1;
        }
    }

    /// 续写流式文本（Assistant / Thinking）。
    ///
    /// 追加正文会让表格序号与内容漂移，已展开的图表必须整表作废（回到表格形态），
    /// 否则用户看到的是上一轮 token 的旧图。
    pub fn push_text(&mut self, delta: &str) {
        match &mut self.block {
            Block::Assistant { text } | Block::Thinking { text, .. } => text.push_str(delta),
            _ => return,
        }
        self.charts.clear();
        self.touch();
    }

    /// 工具执行结束：回填终态。
    pub fn finish_tool(
        &mut self,
        status: ToolStatus,
        result: ToolResult,
        duration_ms: Option<u64>,
        diff: Option<ToolDiff>,
    ) {
        let Block::Tool(tool) = &mut self.block else {
            return;
        };
        tool.status = Some(status);
        tool.images = ToolImage::from_sources(result.images);
        tool.result = Some(result.content);
        tool.duration_ms = duration_ms;
        tool.diff = diff;
        tool.running = false;
        tool.progress = None;
        self.touch();
    }
}

/// 一次工具调用的落地信息：`ToolStarted` 的四个字段，建块用。
///
/// 名字与参数都是字符串，按位置传参在调用点极易错位，因此用具名字段承载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallStart {
    /// 调用 ID。
    pub id: String,
    /// 工具名。
    pub name: String,
    /// 父调用 ID；`Some` 表示 `run_code` 沙盒里发起、以该调用为父的嵌套调用。
    pub parent_id: Option<String>,
    /// 参数 JSON 原文（core 实际执行的那一份）。
    pub arguments: String,
}

/// 一张表格的图表视图态。
///
/// 存活期等于「块存在且正文没变」：正文追加时会整表清空。载荷按像素尺寸
/// 缓存，终端尺寸变了（`px` 不同）就重新栅格化——绝不缩放已有的图。
#[derive(Debug, Clone)]
pub struct TableChartState {
    /// 图表数据（类别、系列、单位、极性）。
    pub spec: moc_core::chart::ChartSpec,
    /// 已编码的图形载荷；`None` = 尚未贴过图。
    pub payload: Option<std::sync::Arc<crate::render::graphics::encode::Payload>>,
    /// 载荷对应的像素尺寸；与当前布局不一致即失效。
    pub px: (u32, u32),
    /// kitty 图像 ID（进程内唯一，放置/删除都靠它对齐）。
    pub image_id: u32,
}

/// 一次工具调用的终态：`ToolFinished` 去掉 `prompt_id` 后的字段，回填用。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallFinish {
    /// 调用 ID。
    pub id: String,
    /// 工具名；块缺席时用它命名。
    pub name: String,
    /// 父调用 ID；块缺席时用它建立嵌套关系。
    pub parent_id: Option<String>,
    /// 参数 JSON 原文；块缺席时用它填摘要。
    pub arguments: String,
    /// 终态。
    pub status: ToolStatus,
    /// 工具结果。
    pub result: ToolResult,
    /// 执行耗时。
    pub duration_ms: Option<u64>,
    /// 文件变更 diff。
    pub diff: Option<ToolDiff>,
}

/// 一次工具调用在转写里的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallBlock {
    /// 调用 ID（与协议的 `tool_call_id` 一致）。
    pub id: String,
    /// 同一模型响应中的工具批次；历史记录按 assistant 条目恢复。
    pub batch_id: Option<String>,
    /// 父工具调用 ID；PTC 子调用据此在滚动区缩进展示。
    pub parent_id: Option<String>,
    /// 工具名。
    pub name: String,
    /// 参数 JSON 原文（core 执行时下发的那一份）。
    pub arguments: String,
    /// 终态；运行中为 `None`。
    pub status: Option<ToolStatus>,
    /// 工具结果文本。
    pub result: Option<String>,
    /// 文件变更 diff。
    pub diff: Option<ToolDiff>,
    /// 执行耗时。
    pub duration_ms: Option<u64>,
    /// 是否仍在执行。
    pub running: bool,
    /// 运行中工具的进度行（目前只有 `wait_agent` 推送）。
    pub progress: Option<Vec<moc_protocol::methods::ToolProgressRow>>,
    /// 结果携带的内联图片（含惰性缓存键，渲染期只查表）。
    pub images: Vec<ToolImage>,
}

/// 结果图片最多贴几张：再多就不是「看一眼」而是画廊了。
pub const MAX_TOOL_IMAGES: usize = 4;

/// 工具结果里的一张图片：键在结果落地时算一次，渲染期只做哈希查表。
#[derive(Debug, Clone)]
pub struct ToolImage {
    /// 内容寻址的缓存键。
    pub key: crate::render::image::ImageKey,
    /// MIME 类型（`image/png` 等），回落文本时原样显示到终端。
    ///
    /// 展示用的这份已过滤控制字符；解码用的是下面那份原样值。
    pub media_type: std::sync::Arc<str>,
    /// 协议层给的原始 MIME：参与解码期的格式嗅探，不该被展示层的清洗改写。
    raw_media_type: std::sync::Arc<str>,
    /// 不含 `data:` 前缀的 base64 载荷。
    ///
    /// 只有真正会显示的前 [`MAX_TOOL_IMAGES`] 张保留：转写块活到会话结束，
    /// 给一张永远不显示的图留几 MB base64 就是纯占内存。
    pub data: Option<std::sync::Arc<str>>,
}

/// 图片的同一性只由缓存键决定：逐字节比 base64 在热路径上毫无意义。
impl PartialEq for ToolImage {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ToolImage {}

impl ToolImage {
    /// 协议层图片 → 渲染层图片。
    ///
    /// 载荷按值接管（不克隆）：调用方拿到的是刚从协议层解析出来的 `Vec`。
    /// URL 来源对工具结果无意义，直接丢弃。
    pub fn from_sources(sources: Vec<moc_protocol::ImageSource>) -> Vec<Self> {
        let mut kept = 0usize;
        sources
            .into_iter()
            .filter_map(|source| {
                let moc_protocol::ImageSource::Base64 { media_type, data } = source else {
                    return None;
                };
                let key = crate::render::image::ImageKey::from_bytes(&media_type, &data);
                let keep_payload = kept < MAX_TOOL_IMAGES;
                if keep_payload {
                    kept += 1;
                }
                Some(Self {
                    key,
                    media_type: std::sync::Arc::from(sanitize_media_type(&media_type)),
                    raw_media_type: std::sync::Arc::from(media_type),
                    data: keep_payload.then(|| std::sync::Arc::from(data)),
                })
            })
            .collect()
    }

    /// 仅测试用：绕开 `from_sources`（那里要完整走一遍协议层载荷）。
    #[cfg(test)]
    pub fn for_test(
        key: crate::render::image::ImageKey,
        media_type: &str,
        data: Option<&str>,
    ) -> Self {
        Self {
            key,
            media_type: std::sync::Arc::from(media_type),
            raw_media_type: std::sync::Arc::from(media_type),
            data: data.map(std::sync::Arc::from),
        }
    }

    /// 解码工厂：交给 `ImageStore`，在后台线程里才真正构造载荷。
    ///
    /// 返回 `None` 表示载荷在落地时就被丢弃了（超出显示上限的那几张）。
    pub fn spec_factory(
        &self,
    ) -> Option<impl FnOnce() -> crate::render::image::ImageSourceSpec + Send + 'static> {
        let data = self.data.clone()?;
        let media_type = self.raw_media_type.clone();
        Some(move || crate::render::image::ImageSourceSpec::Base64 {
            media_type: media_type.as_ref().into(),
            data: data.as_ref().into(),
        })
    }
}

/// 转写中的一个可渲染块。
#[derive(Debug, Clone)]
pub enum Block {
    /// 用户输入。
    User { text: String },
    /// 助手正文（markdown，流式拼接）。
    Assistant { text: String },
    /// 思维链（流式拼接；展示由全局开关与折叠状态共同决定）。
    Thinking {
        text: String,
        reasoning_tokens: Option<u64>,
        duration_ms: Option<u64>,
    },
    /// 一次工具调用。
    Tool(ToolCallBlock),
    /// 其它代理投递进来的消息（IRC 卡片）。
    AgentMessage {
        meta: moc_protocol::methods::AgentMessageMeta,
        body: String,
    },
    /// Todo 清单快照（`TodoChanged` 时替换前一块，而非追加）。
    Todo { phases: Vec<TodoPhase> },
    /// 系统提示（授权等待、错误、压缩等）。
    Note { text: String, error: bool },
}

impl Block {
    /// 块是否响应折叠切换。
    pub fn foldable(&self) -> bool {
        match self {
            Block::Thinking { .. } | Block::AgentMessage { .. } => true,
            Block::Tool(tool) => tool.has_body(),
            Block::User { .. }
            | Block::Assistant { .. }
            | Block::Todo { .. }
            | Block::Note { .. } => false,
        }
    }
}

impl ToolCallBlock {
    /// 是否有值得展开的正文：无正文工具成功时只占一行；失败时正文是错误原因，仍可展开。
    pub fn has_body(&self) -> bool {
        tool_body_allowed(&self.name)
            || !self.images.is_empty()
            || self
                .status
                .is_some_and(|status| status != ToolStatus::Completed)
    }
}

/// 媒体类型只保留 `type/subtype` 里合法的那部分字符。
///
/// 这个字段会被原样写进终端（`[图片 image/png]`），而它来自 MCP / 模型输出；
/// 一个带 ESC 的字符串就能往终端里注入控制序列。图片正文本身早就是原样渲染的，
/// 但这一处是新加的写入口，顺手在这里堵住。
fn sanitize_media_type(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(64));
    for ch in raw.chars() {
        if out.len() >= 64 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '+' | '-' | '_') {
            out.push(ch);
        }
    }
    if out.is_empty() {
        "image/*".to_owned()
    } else {
        out
    }
}

/// 正文策略：成功的「回显参数／致谢／模型向载荷」类工具不渲染正文。
fn tool_body_allowed(name: &str) -> bool {
    !matches!(
        name,
        "read"
            | "find"
            | "todo"
            | "TodoReminder"
            | "skill"
            | "clm_context"
            | "spawn_agent"
            | "list_agents"
            | "stop_agent"
            | "yield"
            | "write_plan"
            | "exit_plan"
    )
}
