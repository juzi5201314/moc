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
}

impl Entry {
    pub fn new(block: Block) -> Self {
        Self {
            generation: 0,
            block,
            level_override: None,
            at_ms: None,
            elapsed_ms: None,
        }
    }

    /// 实时创建的块：带上落地时刻（历史恢复的块走 [`Entry::new`]，时间未知）。
    pub fn timed(block: Block) -> Self {
        Self {
            at_ms: Some(now_ms()),
            ..Self::new(block)
        }
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

    /// 续写流式文本（Assistant / Thinking）。
    pub fn push_text(&mut self, delta: &str) {
        match &mut self.block {
            Block::Assistant { text } | Block::Thinking { text, .. } => text.push_str(delta),
            _ => return,
        }
        self.touch();
    }

    /// 续写工具调用参数。
    pub fn push_args(&mut self, delta: &str) {
        let Block::Tool(tool) = &mut self.block else {
            return;
        };
        tool.arguments.push_str(delta);
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
        tool.result = Some(result.content);
        tool.duration_ms = duration_ms;
        tool.diff = diff;
        tool.running = false;
        tool.progress = None;
        self.touch();
    }
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
    /// 流式累积的 JSON 参数。
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
            Block::User { .. } | Block::Assistant { .. } | Block::Todo { .. } | Block::Note { .. } => {
                false
            }
        }
    }
}

impl ToolCallBlock {
    /// 是否有值得展开的正文：无正文工具成功时只占一行；失败时正文是错误原因，仍可展开。
    pub fn has_body(&self) -> bool {
        tool_body_allowed(&self.name)
            || self.status.is_some_and(|status| status != ToolStatus::Completed)
    }
}

/// 正文策略：成功的「回显参数／致谢／模型向载荷」类工具不渲染正文。
fn tool_body_allowed(name: &str) -> bool {
    !matches!(
        name,
        "read"
            | "find"
            | "todo"
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
