//! Anthropic Messages 编解码。
//!
//! 两处最容易写错的地方，都在本模块内闭合：
//!
//! 1. **按线上角色合并历史**：`Role::User` 与 `Role::Tool` 线上同为 `user`，
//!    连续的这两类消息合并为一条线上 user 消息（内容块按 IR 顺序拼接），
//!    `Role::Assistant` 从不合并。这既满足 Anthropic 的 user/assistant 交替要求，
//!    也让「工具结果 + 中途插入的用户消息」自然落在同一条消息里。
//! 2. **缓存断点落位**：合并会改变下标，因此规整过程同步产出
//!    「IR 消息下标 → 线上 (消息下标, 内容块下标)」映射，`cache_control` 据此落位。

use std::collections::{BTreeMap, BTreeSet};

use rust_i18n::t;
use serde_json::{Value, json};

use crate::error::LlmError;
use crate::types::FinishReason;

/// 默认 API 版本头。
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 1 小时缓存 TTL 所需的 beta 头。
const EXTENDED_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
mod request;
pub(crate) use request::encode;
pub(super) use request::encode_inspected;
mod response;
pub(crate) use response::decode_response;
pub(crate) use response::kind_from_error_type;
pub(crate) use response::parse_error_body;
pub(crate) use response::parse_error_value;
mod stream;

/// 线上消息与断点落位。
struct Normalized {
    /// 顶层 `system` 的文本块，按出现顺序。
    system_parts: Vec<String>,
    /// 顶层 `system` 的内容块形态：写缓存断点时产生。
    system_override: Option<Value>,
    /// 线上消息。
    messages: Vec<Value>,
    /// IR 消息下标 → 该消息在线上请求中的落点。
    placements: Vec<Option<Placement>>,
}

/// IR 消息在线上请求中的落点。
#[derive(Debug, Clone, Copy)]
enum Placement {
    /// 内容块坐标。
    Block {
        /// 线上消息下标。
        message: usize,
        /// 该 IR 消息首块在线上消息内的内容块下标。
        first_block: usize,
        /// 该 IR 消息自身的内容块数。
        blocks: usize,
    },
    /// 消息被上提到顶层 `system` 字段：断点落在 `system` 边界的最后一块上。
    System {
        /// 该 IR 消息首块在 `system` 块数组中的下标。
        first_block: usize,
        /// 该 IR 消息自身的内容块数。
        blocks: usize,
    },
}

/// 缓存落位结果。
struct CacheOutcome {
    /// 是否因为 1h TTL 需要 beta 头。
    needs_extended_ttl_beta: bool,
}

impl Normalized {
    /// 顶层 `system` 字段：无缓存断点时用字符串，有断点时是内容块数组。
    fn system_field(&self) -> Option<Value> {
        match &self.system_override {
            Some(value) => Some(value.clone()),
            None if self.system_is_empty() => None,
            None => Some(Value::String(self.system_parts.join("\n\n"))),
        }
    }

    /// 系统提示是否为空：全部文本块为空即视为没有系统提示。
    fn system_is_empty(&self) -> bool {
        self.system_parts.iter().all(String::is_empty)
    }

    /// 系统提示的内容块形态：每个文本块一个线上块，顺序不变。
    fn system_blocks(&self) -> Vec<Value> {
        self.system_parts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect()
    }

    /// 把 `cache_control` 落到指定线上消息的第 `block` 个内容块上。
    fn attach_message_block(
        &mut self,
        cache_control: &Value,
        message: usize,
        block: usize,
    ) -> Result<(), LlmError> {
        let content = self.messages[message]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .expect("线上消息由本模块构造成内容块数组");
        let len = content.len();
        let Some(target) = content.get_mut(block) else {
            return Err(LlmError::Encode(
                t!("llm.err.cache_block_oob", len = len, block = block).into_owned(),
            ));
        };
        target
            .as_object_mut()
            .expect("内容块由本模块构造成 JSON 对象")
            .insert("cache_control".into(), cache_control.clone());
        Ok(())
    }

    /// 把 `cache_control` 落到 IR 第 `message` 条指令消息的第 `block` 块（`None` 表示最后一块）。
    ///
    /// 指令消息以 system / developer 角色下发时被上提到顶层 `system`，块下标换算到
    /// 该系统数组内；以 user 角色下发时它留在 `messages` 里，块边界与 IR 内容块一一对应。
    fn attach_instruction_block(
        &mut self,
        cache_control: &Value,
        message: usize,
        block: Option<usize>,
    ) -> Result<(), LlmError> {
        match self.placements.get(message).copied().flatten() {
            Some(Placement::System {
                first_block,
                blocks,
            }) => self.attach_system_breakpoint(
                cache_control,
                Some(first_block + block.unwrap_or(blocks - 1)),
            ),
            Some(Placement::Block {
                message: line,
                first_block,
                blocks,
            }) => self.attach_message_block(
                cache_control,
                line,
                first_block + block.unwrap_or(blocks - 1),
            ),
            None => Err(LlmError::Encode(
                t!("llm.err.cache_no_instr", message = message).into_owned(),
            )),
        }
    }

    /// 把 `cache_control` 落到系统提示的第 `block` 块上；`None` 表示最后一块。
    ///
    /// 最后一块之后即「系统提示 + 工具定义」的边界，是 Anthropic 官方推荐的最小
    /// 缓存单元；更靠前的块用于把与工作目录无关的静态前缀单独缓存。多次调用会累积
    /// 到同一份块数组上，因此可以同时落多个块。
    fn attach_system_breakpoint(
        &mut self,
        cache_control: &Value,
        block: Option<usize>,
    ) -> Result<(), LlmError> {
        let mut blocks = match self.system_override.take() {
            Some(Value::Array(blocks)) => blocks,
            _ => self.system_blocks(),
        };
        if self.system_is_empty() {
            return Err(LlmError::Encode(
                t!("llm.err.anthropic_cache_system").into_owned(),
            ));
        }
        let target = block.unwrap_or(blocks.len() - 1);
        let Some(target_block) = blocks.get_mut(target) else {
            return Err(LlmError::Encode(
                t!(
                    "llm.err.system_block_oob",
                    len = blocks.len(),
                    target = target
                )
                .into_owned(),
            ));
        };
        target_block
            .as_object_mut()
            .expect("system 块由本模块构造成 JSON 对象")
            .insert("cache_control".into(), cache_control.clone());
        self.system_override = Some(Value::Array(blocks));
        Ok(())
    }
}

/// Anthropic 流式状态机。
pub(crate) struct AnthropicStreamDecoder {
    /// 首个事件已发出 [`StreamEvent::Start`]。
    started: bool,
    /// 已到终态（`message_stop`、错误或收尾完成）。
    terminated: bool,
    /// `message_delta` 里记录的结束原因。
    stop_reason: Option<FinishReason>,
    /// 服务端工具块（`server_tool_use` / `mcp_tool_use`）的内容块下标。
    ///
    /// 这些块的 `input_json_delta` 由服务端自己执行，不是客户端工具调用，
    /// 必须整段跳过。下标按响应内出现顺序最多几十个，`BTreeSet` 足够。
    server_tool_indices: BTreeSet<usize>,
    /// 块序号稀疏、仅在开始/首个增量/结束访问；未流式化的 input 留到块结束再交付。
    tool_inputs: BTreeMap<usize, Value>,
    /// 至今收到的原始 usage 字段。`message_delta` 的用量是累计值且可选字段可缺省，
    /// 缺省即沿用 `message_start` 的值，因此必须先按原始字段合并再归一化。
    usage: serde_json::Map<String, Value>,
}

#[cfg(test)]
mod tests;
