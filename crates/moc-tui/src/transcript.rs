//! 转写块模型：一次会话在 TUI 里的线性呈现单元。
//!
//! [`Entry`] 包着 [`Block`] 与内容版本号 `gen`：渲染缓存以 `(gen, 宽度)`
//! 为 key,块内容每次变化 `gen` 自增,流式增量只让**活动的那一块**缓存失效。
//! 协议的 `SessionUpdate` 在 `crate::event` 里被折叠成这些块。

use moc_protocol::ToolResult;
use moc_protocol::methods::{TodoPhase, ToolDiff, ToolStatus};

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
    /// 单块折叠覆盖（与全局开关叠加：任一为真即折叠）。
    pub folded: bool,
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
            folded: false,
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

    /// 切换单块折叠。
    pub fn toggle_fold(&mut self) {
        if self.block.foldable() {
            self.folded = !self.folded;
            self.generation += 1;
        }
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
    /// Todo 清单快照（`TodoChanged` 时替换前一块，而非追加）。
    Todo { phases: Vec<TodoPhase> },
    /// 系统提示（授权等待、错误、压缩等）。
    Note { text: String, error: bool },
}

impl Block {
    /// 块是否响应折叠切换。
    pub fn foldable(&self) -> bool {
        matches!(self, Block::Tool(_) | Block::Thinking { .. })
    }
}
