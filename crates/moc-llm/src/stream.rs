//! 流式对象与累加器。
//!
//! [`ChatStream`] 是「SSE 字节流 → [`StreamEvent`] 序列 → 完整 [`ChatResponse`]」
//! 的唯一通道：事件按到达顺序交付给调用方，同时喂给内部累加器，因此
//! [`ChatStream::finish`] 与 [`ChatStream::interrupt`] 都能给出可直接回灌历史的
//! assistant 消息。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;

use crate::codec::StreamDecoder;
use crate::codec::sse::SseDecoder;
use crate::config::{LlmConfig, Protocol};
use crate::error::LlmError;
use crate::guard::StreamGuard;
use crate::types::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, Message, ReasoningBlock, Role,
    StreamEvent, ToolCall, Usage,
};

/// 累加块种类，与 [`StreamEvent::index`] 一起构成归组键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BlockKind {
    /// 正文文本。
    Text,
    /// 思维链。
    Reasoning,
    /// 工具调用。
    ToolCall,
}

/// 累加中的内容块。
enum AccumulatedBlock {
    Text {
        text: String,
        phase: Option<String>,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
        /// Responses 的完整 reasoning item（`output_item.done` 给出）。
        item: Option<Box<serde_json::Value>>,
        /// Anthropic 的不可读思维链（`redacted_thinking`）的不透明数据。
        redacted: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
}

/// 事件累加器。
///
/// 块定位用 `HashMap<(BlockKind, index), slot>`：线上 index 在同一次响应内稠密且
/// 很小，但跨协议的 index 空间不同（Chat 的文本恒为 0、Anthropic 与 Responses
/// 用各自的内容块序号），因此不做按种类分桶的下标寻址；每帧一次哈希查找相比同帧
/// 的 JSON 解析开销可以忽略。
#[derive(Default)]
struct Accumulator {
    id: Option<String>,
    model: Option<String>,
    /// 上游回显的推理强度；只在 [`StreamEvent::Start`] 首次给出时填充。
    effort: Option<String>,
    blocks: Vec<AccumulatedBlock>,
    slots: HashMap<(BlockKind, usize), usize>,
    usage: Usage,
    finish_reason: Option<FinishReason>,
}

impl Accumulator {
    /// 消费一个事件。
    fn apply(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { id, model, effort } => {
                if self.id.is_none() {
                    self.id = id.clone();
                }
                if self.model.is_none() {
                    self.model = model.clone();
                }
                if self.effort.is_none() {
                    self.effort = effort.clone();
                }
            }
            StreamEvent::TextDelta { index, text } => {
                self.with_block(BlockKind::Text, *index, |slot| match slot {
                    AccumulatedBlock::Text { text: buffer, .. } => buffer.push_str(text),
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ResponsePhase { index, phase } => {
                self.with_block(BlockKind::Text, *index, |slot| {
                    if let AccumulatedBlock::Text { phase: target, .. } = slot {
                        *target = Some(phase.clone());
                    }
                });
            }
            StreamEvent::ReasoningDelta { index, text } => {
                self.with_block(BlockKind::Reasoning, *index, |slot| match slot {
                    AccumulatedBlock::Reasoning { text: buffer, .. } => buffer.push_str(text),
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                self.with_block(BlockKind::Reasoning, *index, |slot| match slot {
                    AccumulatedBlock::Reasoning {
                        signature: buffer, ..
                    } => {
                        buffer.get_or_insert_with(String::new).push_str(signature);
                    }
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ReasoningItem { index, item } => {
                let redacted = redacted_data(item);
                self.with_block(BlockKind::Reasoning, *index, |slot| match slot {
                    AccumulatedBlock::Reasoning {
                        item: target,
                        redacted: target_redacted,
                        ..
                    } => match redacted {
                        // Anthropic 的不透明思维链有专用 IR 形态：编码器只接受
                        // `Redacted`，原样保留 item 会让产物无法再编码。
                        Some(data) => *target_redacted = Some(data),
                        // 完整 item 是权威形态（含 encrypted_content），
                        // 覆盖增量阶段积累的摘要文本。
                        None => *target = Some(item.clone()),
                    },
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                self.with_block(BlockKind::ToolCall, *index, |slot| match slot {
                    AccumulatedBlock::ToolCall {
                        id: id_buffer,
                        name: name_buffer,
                        ..
                    } => {
                        id_buffer.clone_from(id);
                        name_buffer.clone_from(name);
                    }
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ToolCallArgumentsDelta { index, json } => {
                self.with_block(BlockKind::ToolCall, *index, |slot| match slot {
                    AccumulatedBlock::ToolCall { arguments, .. } => arguments.push_str(json),
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::ToolCallSnapshot {
                index,
                id,
                name,
                arguments,
            } => {
                self.with_block(BlockKind::ToolCall, *index, |slot| match slot {
                    AccumulatedBlock::ToolCall {
                        id: target_id,
                        name: target_name,
                        arguments: target_arguments,
                    } => {
                        if let Some(id) = id
                            && target_id != id
                        {
                            target_id.clone_from(id);
                        }
                        if let Some(name) = name
                            && target_name != name
                        {
                            target_name.clone_from(name);
                        }
                        if let Some(arguments) = arguments {
                            target_arguments.clone_from(arguments);
                        }
                    }
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
                });
            }
            StreamEvent::TextSnapshot { .. } => unreachable!("正文快照在入队前转为缺失后缀"),
            StreamEvent::Usage(usage) => self.usage.merge(*usage),
            StreamEvent::Done { finish_reason } => {
                self.finish_reason = Some(finish_reason.clone());
            }
        }
    }

    /// 已交付文本不可撤回；协议完整正文必须包含之前的全部增量。
    fn text_suffix(&self, index: usize, text: &str) -> Result<usize, LlmError> {
        let Some(&slot) = self.slots.get(&(BlockKind::Text, index)) else {
            return Ok(0);
        };
        let AccumulatedBlock::Text { text: previous, .. } = &self.blocks[slot] else {
            unreachable!("正文下标只引用正文块");
        };
        if !text.starts_with(previous) {
            return Err(LlmError::Stream("完整正文与已收到的文本增量不一致".into()));
        }
        Ok(previous.len())
    }

    /// 找到或新建 `(kind, index)` 对应的块并就地修改。
    fn with_block(
        &mut self,
        kind: BlockKind,
        index: usize,
        edit: impl FnOnce(&mut AccumulatedBlock),
    ) {
        let slot = match self.slots.get(&(kind, index)) {
            Some(slot) => *slot,
            None => {
                let block = match kind {
                    BlockKind::Text => AccumulatedBlock::Text {
                        text: String::new(),
                        phase: None,
                    },
                    BlockKind::Reasoning => AccumulatedBlock::Reasoning {
                        text: String::new(),
                        signature: None,
                        item: None,
                        redacted: None,
                    },
                    BlockKind::ToolCall => AccumulatedBlock::ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    },
                };
                self.blocks.push(block);
                self.slots.insert((kind, index), self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        edit(&mut self.blocks[slot]);
    }

    /// 折叠为完整响应。
    fn fold(&self, protocol: Protocol, interrupted: bool) -> ChatResponse {
        ChatResponse {
            id: self.id.clone(),
            model: self.model.clone(),
            reasoning_effort: self.effort.clone(),
            message: Message {
                role: Role::Assistant,
                content: self.content(protocol, interrupted).into(),
            },
            finish_reason: if interrupted {
                FinishReason::Interrupted
            } else {
                self.finish_reason.clone().unwrap_or_default()
            },
            usage: self.usage,
            raw: None,
        }
    }

    /// 产出内容块；`interrupted` 为真时按协议做可回灌修剪。
    fn content(&self, protocol: Protocol, interrupted: bool) -> Vec<ContentPart> {
        // 参数不是合法 JSON 对象的工具调用回灌后必然被上游拒绝（中断产生的是
        // JSON 前缀），因此先确定哪些工具调用能保留，再决定孤立 reasoning item
        // 的去留。
        let keeps_tool_call = |block: &AccumulatedBlock| match block {
            AccumulatedBlock::ToolCall { arguments, .. } => is_json_object(arguments),
            _ => false,
        };
        let has_tool_call = interrupted && self.blocks.iter().any(keeps_tool_call);

        let mut content = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            match block {
                AccumulatedBlock::Text { text, phase } => {
                    if !text.is_empty() {
                        if let Some(phase) = phase {
                            content.push(ContentPart::ResponsePhase(phase.clone()));
                        }
                        content.push(ContentPart::Text(text.clone()));
                    }
                }
                AccumulatedBlock::Reasoning {
                    text,
                    signature,
                    item,
                    redacted,
                } => {
                    // 编码器硬拒绝的形态一律不留（留着会让产物落库后整条会话在
                    // 下一轮以不可重试的 Encode 失败）：Anthropic 只接受
                    // `redacted_thinking` 与带签名的 `thinking`。
                    if protocol == Protocol::AnthropicMessages && item.is_some() {
                        continue;
                    }
                    // 仅有摘要也要持久化用于展示；Responses 回传策略会剔除 Text，
                    // 因此不需要伪造可回传的 reasoning item。
                    if interrupted {
                        // Responses 的 reasoning item 与其后的 function_call 成对
                        // 使用，孤立 item 无意义。
                        if item.is_some() && protocol == Protocol::OpenAiResponses && !has_tool_call
                        {
                            if !text.is_empty() {
                                content.push(ContentPart::Reasoning(ReasoningBlock::text(
                                    text.clone(),
                                )));
                            }
                            continue;
                        }
                        // Anthropic 的 thinking 块缺签名会被上游拒绝（签名在块末尾
                        // 的 signature_delta 中到达）；`redacted_thinking` 在
                        // content_block_start 里就是完整的，不受此限。
                        if redacted.is_none()
                            && item.is_none()
                            && protocol == Protocol::AnthropicMessages
                            && signature.as_deref().unwrap_or_default().is_empty()
                        {
                            continue;
                        }
                    }
                    if let Some(data) = redacted {
                        content.push(ContentPart::Reasoning(ReasoningBlock::Redacted {
                            data: data.clone(),
                        }));
                    } else if let Some(item) = item {
                        let mut saved = crate::ReasoningItem::from((**item).clone());
                        if !text.is_empty() && *text != crate::ReasoningItem::visible_text(item) {
                            saved.display_text = Some(text.clone());
                        }
                        content.push(ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                            saved,
                        ))));
                    } else if !text.is_empty() || signature.is_some() {
                        content.push(ContentPart::Reasoning(ReasoningBlock::Text {
                            text: text.clone(),
                            signature: signature.clone(),
                        }));
                    }
                }
                AccumulatedBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } => {
                    if interrupted && !is_json_object(arguments) {
                        continue;
                    }
                    content.push(ContentPart::ToolCall(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }));
                }
            }
        }
        content
    }
}

/// 参数是否为合法 JSON 对象。
fn is_json_object(arguments: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(arguments)
        .map(|value| value.is_object())
        .unwrap_or(false)
}

/// 从 [`StreamEvent::ReasoningItem`] 里取出 Anthropic 的不透明思维链数据。
///
/// `redacted_thinking` 是唯一带此 `type` 的块：它对应 [`ReasoningBlock::Redacted`]，
/// 而不是 Responses 那种需要原样回传的 reasoning item。
fn redacted_data(item: &serde_json::Value) -> Option<String> {
    if item.get("type").and_then(serde_json::Value::as_str) != Some("redacted_thinking") {
        return None;
    }
    item.get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// 流式响应。
///
/// 被 drop 时 `reqwest` 的字节流随之释放、连接关闭，不需要额外的取消机制。
pub struct ChatStream {
    /// 响应体字节流。
    body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
    /// SSE 帧解码器。
    sse: SseDecoder,
    /// 协议状态机。
    decoder: Box<dyn StreamDecoder>,
    /// 待交付事件。
    queue: VecDeque<Result<StreamEvent, LlmError>>,
    /// 解码暂存缓冲，跨帧复用以免每帧分配。
    pending: Vec<Result<StreamEvent, LlmError>>,
    /// 事件累加器。
    accumulator: Accumulator,
    /// 首个错误，供 [`ChatStream::finish`] 返回。
    error: Option<LlmError>,
    /// 是否已停止读取（终态或 body 耗尽）。
    finished: bool,
    /// 协议是否给出过明确终态。EOF 时解码器会补一个 `Done`，那不算完整响应。
    terminal_seen: bool,
    /// 协议，决定中断时的修剪规则。
    protocol: Protocol,
    guard: StreamGuard,
}

impl ChatStream {
    /// 由客户端构造。
    pub(crate) fn new(
        body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
        cfg: &LlmConfig,
        req: &ChatRequest,
    ) -> ChatStream {
        ChatStream {
            body,
            sse: SseDecoder::new(),
            decoder: crate::codec::stream_decoder(cfg, req),
            queue: VecDeque::new(),
            pending: Vec::new(),
            accumulator: Accumulator::default(),
            error: None,
            finished: false,
            terminal_seen: false,
            protocol: cfg.protocol,
            guard: StreamGuard::new(req.options.stream_guard),
        }
    }

    /// 取出下一个事件；流结束或出错后返回 `None`。
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, LlmError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// 读完剩余事件并折叠为完整响应；流中发生过错误则返回该错误。
    ///
    /// 上游没有给出协议终态就关闭连接时，解码器补出的 `Done` 不能当成完整响应：
    /// 那样的内容既无法证明完整，也可能带着半截工具参数，落库后整条会话再发不出请求。
    pub async fn finish(mut self) -> Result<ChatResponse, LlmError> {
        while self.next_event().await.is_some() {}
        match self.error {
            Some(error) => Err(error),
            None if !self.terminal_seen => Err(LlmError::Stream(
                "上游响应在协议终态之前结束，本次响应不完整".into(),
            )),
            None => Ok(self.accumulator.fold(self.protocol, false)),
        }
    }

    /// 中止读取并返回修剪过的部分响应。
    ///
    /// 产物是「协议感知地修剪过、可安全回灌历史」的形式：
    ///
    /// | 修剪动作 | 原因 |
    /// |---|---|
    /// | 丢弃参数不是合法 JSON 对象的工具调用 | 中断产生的是 JSON 前缀，回灌后上游必然 400；参数完整的调用保留，core 已执行的结果仍能配对 |
    /// | Anthropic 下丢弃无签名的思维链块 | 缺签名会被上游拒绝（签名在块末尾到达）；`redacted_thinking` 随开始块一次性给出，保留 |
    /// | Responses 下没有保留工具调用时丢弃 reasoning item | reasoning item 与其后的 function_call 成对使用，孤立 item 无意义 |
    /// | 丢弃编码器不接受的思维链形态 | 例如 Anthropic 会话里的 Responses reasoning item，否则落库后整条会话再发不出请求 |
    /// | 文本与已累积的思维链文本保留 | 用户看到的内容不丢 |
    pub fn interrupt(self) -> ChatResponse {
        if matches!(&self.error, Some(LlmError::Guard(_))) {
            return self.discard(FinishReason::LoopDetected);
        }
        self.accumulator.fold(self.protocol, true)
    }

    /// 丢弃本次失败生成的正文、工具和不透明思考，只保留已知用量。
    pub fn discard(self, reason: FinishReason) -> ChatResponse {
        ChatResponse {
            id: self.accumulator.id,
            model: self.accumulator.model,
            reasoning_effort: self.accumulator.effort,
            message: Message::default(),
            finish_reason: reason,
            usage: self.accumulator.usage,
            raw: None,
        }
    }

    /// 读一块字节并把解出的事件推入队列；返回 `Pending` 表示网络暂无数据。
    fn pump(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self.body.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                self.sse.push(&chunk);
                self.drain_frames();
                Poll::Ready(())
            }
            Poll::Ready(Some(Err(err))) => {
                self.stop_with(LlmError::Stream(format!("读取响应体失败: {err}")));
                Poll::Ready(())
            }
            Poll::Ready(None) => {
                self.finished = true;
                self.decoder.finish(&mut self.pending);
                self.flush_pending();
                Poll::Ready(())
            }
        }
    }

    /// 把缓冲区里所有完整 SSE 帧喂给协议状态机。
    fn drain_frames(&mut self) {
        while let Some(frame) = self.sse.next_event() {
            self.decoder.decode(frame, &mut self.pending);
            self.flush_pending();
            if self.finished {
                return;
            }
            if self.decoder.is_terminated() {
                // 终态：不再读网络，连接随 drop 关闭。
                self.terminal_seen = true;
                self.finished = true;
                return;
            }
        }
    }

    /// 把暂存事件交给累加器与调用方。
    fn flush_pending(&mut self) {
        let mut pending = std::mem::take(&mut self.pending);
        for event in pending.drain(..) {
            let event = match event {
                Ok(StreamEvent::TextSnapshot { index, mut text }) => {
                    match self.accumulator.text_suffix(index, &text) {
                        Ok(prefix) if prefix == text.len() => continue,
                        Ok(prefix) => {
                            text.drain(..prefix);
                            Ok(StreamEvent::TextDelta { index, text })
                        }
                        Err(error) => {
                            self.stop_with(error);
                            break;
                        }
                    }
                }
                Ok(StreamEvent::ReasoningItem { index, item }) => {
                    let mut visible = crate::ReasoningItem::visible_text(&item);
                    let previous = self
                        .accumulator
                        .slots
                        .get(&(BlockKind::Reasoning, index))
                        .and_then(|slot| match &self.accumulator.blocks[*slot] {
                            AccumulatedBlock::Reasoning { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if visible.starts_with(previous) && visible.len() > previous.len() {
                        visible.drain(..previous.len());
                        if !self.enqueue(Ok(StreamEvent::ReasoningDelta {
                            index,
                            text: visible,
                        })) {
                            break;
                        }
                    }
                    Ok(StreamEvent::ReasoningItem { index, item })
                }
                Ok(StreamEvent::ToolCallSnapshot {
                    index,
                    id,
                    name,
                    mut arguments,
                }) => {
                    if let Some(&slot) = self.accumulator.slots.get(&(BlockKind::ToolCall, index))
                        && let AccumulatedBlock::ToolCall {
                            arguments: previous,
                            ..
                        } = &self.accumulator.blocks[slot]
                        && arguments.as_ref() == Some(previous)
                    {
                        arguments = None;
                    }
                    Ok(StreamEvent::ToolCallSnapshot {
                        index,
                        id,
                        name,
                        arguments,
                    })
                }
                event => event,
            };
            if !self.enqueue(event) {
                break;
            }
        }
        self.pending = pending;
    }

    fn enqueue(&mut self, event: Result<StreamEvent, LlmError>) -> bool {
        if let Ok(stream_event) = &event
            && let Err(violation) = self.guard.observe(stream_event)
        {
            self.stop_with(violation.into());
            return false;
        }
        match &event {
            Ok(stream_event) => self.accumulator.apply(stream_event),
            Err(error) => self.error = Some(error.clone()),
        }
        self.queue.push_back(event);
        true
    }

    /// 以错误结束流。
    fn stop_with(&mut self, error: LlmError) {
        self.finished = true;
        self.error = Some(error.clone());
        // 立即释放上游；即使调用方保留 ChatStream 也不继续下载。
        self.body = Box::pin(crate::stream::EmptyBody);
        if matches!(self.error, Some(LlmError::Guard(_))) {
            self.accumulator.finish_reason = Some(FinishReason::LoopDetected);
            self.accumulator.blocks.clear();
            self.accumulator.slots.clear();
            self.queue.clear();
            self.sse = SseDecoder::new();
        }
        self.queue.push_back(Err(error));
    }
}

impl Stream for ChatStream {
    type Item = Result<StreamEvent, LlmError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(event) = this.queue.pop_front() {
                return Poll::Ready(Some(event));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            if this.pump(cx).is_pending() {
                return Poll::Pending;
            }
        }
    }
}

impl std::fmt::Debug for ChatStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatStream")
            .field("protocol", &self.protocol)
            .field("finished", &self.finished)
            .field("queued", &self.queue.len())
            .field("blocks", &self.accumulator.blocks.len())
            .finish()
    }
}

struct EmptyBody;

impl Stream for EmptyBody {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Auth, EndpointStyle};
    use crate::types::ReasoningEcho;

    fn config(protocol: Protocol) -> LlmConfig {
        LlmConfig {
            protocol,
            base_url: "https://example.com/v1".into(),
            model: "test".into(),
            auth: Auth::None,
            endpoint: EndpointStyle::Default,
            ..Default::default()
        }
    }

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user_text("hi")],
            options: crate::types::RequestOptions {
                echo_reasoning: ReasoningEcho::Protocol,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn accumulate(protocol: Protocol, events: &[StreamEvent]) -> Accumulator {
        let mut accumulator = Accumulator::default();
        for event in events {
            accumulator.apply(event);
        }
        let _ = protocol;
        accumulator
    }

    #[test]
    fn accumulates_text_and_tool_calls() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::Start {
                    id: Some("chatcmpl-1".into()),
                    model: Some("test".into()),
                    effort: None,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "答案".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "是 42".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":\"北京\"}".into(),
                },
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: Some(15),
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiChat, false);
        assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(response.model.as_deref(), Some("test"));
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.usage.total_tokens(), 15);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Text("答案是 42".into()),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
    }

    #[test]
    fn accumulates_reasoning_with_signature() {
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "先".into(),
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "想一想".into(),
                },
                StreamEvent::ReasoningSignature {
                    index: 0,
                    signature: "sig-".into(),
                },
                StreamEvent::ReasoningSignature {
                    index: 0,
                    signature: "1".into(),
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "答案".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        );
        let response = accumulator.fold(Protocol::AnthropicMessages, false);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning(ReasoningBlock::Text {
                    text: "先想一想".into(),
                    signature: Some("sig-1".into()),
                }),
                ContentPart::Text("答案".into()),
            ]
        );
    }

    #[test]
    fn keeps_reasoning_item_from_event() {
        let item = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "blob",
            "summary": [{"type": "summary_text", "text": "想一下"}],
        });
        let accumulator = accumulate(
            Protocol::OpenAiResponses,
            &[
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "想一下".into(),
                },
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(item.clone()),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiResponses, false);
        assert_eq!(
            response.message.content,
            vec![ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                item.into()
            )))]
        );
    }

    #[test]
    fn interleaved_tool_calls_by_index() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_2".into(),
                    name: "get_date".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{}".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "\"北京\"}".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiChat, false);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
                ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
            ]
        );
    }

    #[test]
    fn partial_arguments_preserved_raw() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiChat, false);
        let ContentPart::ToolCall(call) = &response.message.content[0] else {
            panic!("期望工具调用块");
        };
        assert_eq!(call.arguments, "{\"city\":");
        assert!(call.arguments_json().is_err());
    }

    #[test]
    fn interrupt_keeps_text_and_complete_tool_call() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::TextDelta {
                    index: 0,
                    text: "正在查".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":\"北京\"}".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_2".into(),
                    name: "get_date".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{\"day\":".into(),
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiChat, true);
        assert_eq!(response.finish_reason, FinishReason::Interrupted);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Text("正在查".into()),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
    }

    #[test]
    fn interrupt_drops_unsigned_thinking() {
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "半截思考".into(),
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "正文".into(),
                },
            ],
        );
        let response = accumulator.fold(Protocol::AnthropicMessages, true);
        assert_eq!(
            response.message.content,
            vec![ContentPart::Text("正文".into())]
        );

        // 带签名时保留。
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "完整思考".into(),
                },
                StreamEvent::ReasoningSignature {
                    index: 0,
                    signature: "sig".into(),
                },
            ],
        );
        let response = accumulator.fold(Protocol::AnthropicMessages, true);
        assert_eq!(
            response.message.content,
            vec![ContentPart::Reasoning(ReasoningBlock::Text {
                text: "完整思考".into(),
                signature: Some("sig".into()),
            })]
        );
    }

    #[test]
    fn interrupt_drops_orphan_reasoning_item() {
        let item = serde_json::json!({"type": "reasoning", "id": "rs_1"});
        let accumulator = accumulate(
            Protocol::OpenAiResponses,
            &[
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(item.clone()),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{\"city\":".into(),
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiResponses, true);
        assert!(
            response.message.content.is_empty(),
            "{:?}",
            response.message.content
        );

        // 有完整工具调用与之配对时保留。
        let accumulator = accumulate(
            Protocol::OpenAiResponses,
            &[
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(item.clone()),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{\"city\":\"北京\"}".into(),
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiResponses, true);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning(ReasoningBlock::Item(Box::new(item.into()))),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
    }

    #[test]
    fn redacted_thinking_folds_into_redacted_block() {
        // Anthropic 的 `redacted_thinking` 折成 `ReasoningBlock::Item` 时编码器
        // 硬拒绝：产物一旦落库，该会话后续所有请求都以不可重试的 Encode 失败。
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(serde_json::json!({
                        "type": "redacted_thinking",
                        "data": "opaque",
                    })),
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "答案".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        );
        for interrupted in [false, true] {
            let response = accumulator.fold(Protocol::AnthropicMessages, interrupted);
            assert_eq!(
                response.message.content,
                vec![
                    ContentPart::Reasoning(ReasoningBlock::Redacted {
                        data: "opaque".into(),
                    }),
                    ContentPart::Text("答案".into()),
                ],
                "interrupted={interrupted}"
            );
        }

        // 与非流式一致地可再编码：同一条消息放回历史，下一个请求必须能构建。
        let response = accumulator.fold(Protocol::AnthropicMessages, true);
        let request = ChatRequest {
            messages: vec![
                Message::user_text("查天气"),
                response.message,
                Message::user_text("继续"),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let encoded =
            crate::codec::encode(&config(Protocol::AnthropicMessages), &request, false).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&encoded.body).unwrap();
        assert_eq!(
            body["messages"][1]["content"][0],
            serde_json::json!({"type": "redacted_thinking", "data": "opaque"})
        );

        // 与协议不匹配的 reasoning item 同样不能留下：Anthropic 编码器对它硬拒绝。
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(serde_json::json!({"type": "reasoning", "id": "rs_1"})),
            }],
        );
        assert!(
            accumulator
                .fold(Protocol::AnthropicMessages, false)
                .message
                .content
                .is_empty()
        );
    }

    #[tokio::test]
    async fn interrupted_summary_is_visible_but_not_replayed_as_a_reasoning_item() {
        let mut stream = stream_from_chunks(
            &config(Protocol::OpenAiResponses),
            &request(),
            vec![
                "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"想了一半\"}\n\n",
            ],
        );
        assert!(matches!(
            stream.next_event().await.unwrap().unwrap(),
            StreamEvent::ReasoningDelta { .. }
        ));
        let response = stream.interrupt();
        let ContentPart::Reasoning(block) = &response.message.content[0] else {
            panic!("缺少摘要")
        };
        assert_eq!(block.display_text().as_deref(), Some("想了一半"));
        let request = ChatRequest {
            messages: vec![response.message],
            ..Default::default()
        };
        let wire =
            crate::inspect_request(&config(Protocol::OpenAiResponses), &request, false).unwrap();
        assert_eq!(wire.body["input"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn responses_replay_preserves_phase_and_interleaved_text_order() {
        use serde_json::json;
        let output = [
            json!({"type":"message", "phase":"commentary", "role":"assistant", "content":[{"type":"output_text","text":"先查"}]}),
            json!({"type":"function_call", "call_id":"call_1", "name":"read", "arguments":"{}"}),
            json!({"type":"message", "phase":"final_answer", "role":"assistant", "content":[{"type":"output_text","text":"结论"}]}),
        ];
        let response = decode_frames(Protocol::OpenAiResponses, &[
            json!({"type":"response.completed", "response":{"status":"completed", "output":output}}),
        ]).await;
        assert_eq!(response.message.text(), "先查结论");
        let restored =
            serde_json::from_str(&serde_json::to_string(&response.message).unwrap()).unwrap();
        let request = ChatRequest {
            messages: vec![restored, Message::tool_result("call_1", "结果", false)],
            ..Default::default()
        };
        let wire =
            crate::inspect_request(&config(Protocol::OpenAiResponses), &request, false).unwrap();
        assert_eq!(wire.body["input"][0], output[0]);
        assert_eq!(wire.body["input"][1], output[1]);
        assert_eq!(wire.body["input"][2], output[2]);
    }

    #[test]
    fn interrupt_then_history_is_valid() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::TextDelta {
                    index: 0,
                    text: "先查天气".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":\"北京\"}".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_2".into(),
                    name: "get_date".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{\"day\":".into(),
                },
            ],
        );
        let partial = accumulator.fold(Protocol::OpenAiChat, true);

        // core 为保留的工具调用补结果、追加用户插话，然后重新编码：不应触发
        // 历史完整性校验。
        let request = ChatRequest {
            messages: vec![
                Message::user_text("查天气"),
                partial.message,
                Message::tool_result("call_1", "晴", false),
                Message::user_text("改用另一种方案"),
            ],
            ..Default::default()
        };
        let encoded = crate::codec::encode(&config(Protocol::OpenAiChat), &request, false).unwrap();
        let body: serde_json::Value = serde_json::from_slice(&encoded.body).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            body["messages"][1]["tool_calls"].as_array().unwrap().len(),
            1
        );
    }

    /// 测试替身：先吐固定字节，然后永远 `Pending`。
    struct StalledStream {
        chunks: VecDeque<Bytes>,
    }

    impl Stream for StalledStream {
        type Item = reqwest::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.chunks.pop_front() {
                Some(chunk) => Poll::Ready(Some(Ok(chunk))),
                None => Poll::Pending,
            }
        }
    }

    fn stream_from_chunks(
        cfg: &LlmConfig,
        req: &ChatRequest,
        chunks: Vec<&'static str>,
    ) -> ChatStream {
        let body = StalledStream {
            chunks: chunks
                .into_iter()
                .map(|chunk| Bytes::from_static(chunk.as_bytes()))
                .collect(),
        };
        ChatStream::new(Box::pin(body), cfg, req)
    }

    /// 测试替身：吐完固定字节后正常 EOF（上游没给终态就关连接）。
    struct EndedStream {
        chunks: VecDeque<Bytes>,
    }

    impl Stream for EndedStream {
        type Item = reqwest::Result<Bytes>;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.chunks.pop_front().map(Ok))
        }
    }

    fn stream_ending_after(
        cfg: &LlmConfig,
        req: &ChatRequest,
        chunks: Vec<&'static str>,
    ) -> ChatStream {
        let body = EndedStream {
            chunks: chunks
                .into_iter()
                .map(|chunk| Bytes::from_static(chunk.as_bytes()))
                .collect(),
        };
        ChatStream::new(Box::pin(body), cfg, req)
    }

    #[tokio::test]
    async fn chat_keeps_choice_zero_and_complete_fragmented_tool_name() {
        let stream = stream_ending_after(
            &config(Protocol::OpenAiChat),
            &request(),
            vec![
                "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"不能混入\",\"tool_calls\":[{\"index\":0,\"id\":\"other\",\"function\":{\"name\":\"wrong\",\"arguments\":\"{}\"}}]}},{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"get\",\"arguments\":\"{\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"_weather\",\"arguments\":\"\\\"city\\\":\\\"北京\\\"\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"_now\",\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"},{\"index\":1,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        let response = stream.finish().await.unwrap();
        assert_eq!(
            response.message.content.as_slice(),
            &[ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather_now",
                r#"{"city":"北京"}"#
            ))]
        );
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.usage.total_tokens(), 15);
    }

    async fn decode_frames(protocol: Protocol, frames: &[serde_json::Value]) -> ChatResponse {
        let body = EndedStream {
            chunks: frames
                .iter()
                .map(|frame| Bytes::from(format!("data: {frame}\n\n")))
                .collect(),
        };
        ChatStream::new(Box::pin(body), &config(protocol), &request())
            .finish()
            .await
            .expect("夹具包含协议终态")
    }

    #[tokio::test]
    async fn responses_tool_snapshots_preserve_arguments_without_deltas() {
        use serde_json::json;
        let arguments = r#"{"path":"README.md"}"#;
        let call = json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"read", "arguments":arguments});
        for carrier in ["added", "arguments_done", "item_done", "completed"] {
            let mut added = call.clone();
            added["arguments"] = json!(if carrier == "added" { arguments } else { "" });
            let mut frames =
                vec![json!({"type":"response.output_item.added", "output_index":0, "item":added})];
            match carrier {
                "arguments_done" => frames.push(json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":arguments})),
                "item_done" => frames.push(json!({"type":"response.output_item.done", "output_index":0, "item":call})),
                _ => {}
            }
            frames.push(json!({"type":"response.completed", "response":{"status":"completed", "output": if carrier == "completed" { vec![call.clone()] } else { Vec::new() }}}));
            let response = decode_frames(Protocol::OpenAiResponses, &frames).await;
            assert_eq!(
                response.message.tool_calls().collect::<Vec<_>>(),
                vec![&ToolCall::new("call_1", "read", arguments)],
                "{carrier}"
            );
        }
    }

    #[tokio::test]
    async fn responses_final_items_replace_partial_arguments_without_duplicates() {
        use serde_json::json;
        let calls = [
            json!({"type":"function_call", "call_id":"call_read", "name":"read", "arguments":"{\"path\":\"README.md\"}"}),
            json!({"type":"function_call", "call_id":"call_find", "name":"find", "arguments":"{\"glob\":[\"*.rs\"]}"}),
            json!({"type":"function_call", "call_id":"call_bash", "name":"bash", "arguments":"{\"command\":\"pwd\"}"}),
        ];
        let mut frames = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            let mut added = call.clone();
            added["arguments"] = json!("");
            frames.push(
                json!({"type":"response.output_item.added", "output_index":index, "item":added}),
            );
            frames.push(json!({"type":"response.function_call_arguments.delta", "output_index":index, "delta":"{"}));
        }
        for (index, call) in calls.iter().enumerate().rev() {
            frames.push(json!({"type":"response.function_call_arguments.done", "output_index":index, "arguments":call["arguments"]}));
            frames.push(
                json!({"type":"response.output_item.done", "output_index":index, "item":call}),
            );
        }
        frames.push(json!({"type":"response.completed", "response":{"id":"resp_1", "model":"test", "status":"completed", "output":calls}}));
        let response = decode_frames(Protocol::OpenAiResponses, &frames).await;
        assert_eq!(response.id.as_deref(), Some("resp_1"));
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.message.tool_calls().cloned().collect::<Vec<_>>(),
            vec![
                ToolCall::new("call_read", "read", r#"{"path":"README.md"}"#),
                ToolCall::new("call_find", "find", r#"{"glob":["*.rs"]}"#),
                ToolCall::new("call_bash", "bash", r#"{"command":"pwd"}"#),
            ]
        );
    }

    #[tokio::test]
    async fn responses_final_message_preserves_text_refusal_and_reasoning() {
        use serde_json::json;
        let reasoning =
            json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]});
        let message = json!({"type":"message", "content":[{"type":"output_text", "text":"完整正文"}, {"type":"refusal", "refusal":"拒绝内容"}]});
        let response = decode_frames(Protocol::OpenAiResponses, &[
            json!({"type":"response.output_item.done", "output_index":0, "item":reasoning}),
            json!({"type":"response.output_text.delta", "output_index":1, "content_index":0, "delta":"完整"}),
            json!({"type":"response.output_item.done", "output_index":1, "item":message}),
            json!({"type":"response.completed", "response":{"status":"completed", "output":[reasoning, message]}}),
        ]).await;
        assert_eq!(response.message.text(), "完整正文拒绝内容");
        assert_eq!(response.message.content.len(), 2);
        assert_eq!(
            response.message.content[0],
            ContentPart::Reasoning(ReasoningBlock::Item(Box::new(reasoning.into())))
        );
    }

    #[tokio::test]
    async fn responses_summary_survives_snapshots_persistence_and_replay() {
        use serde_json::json;
        for (delta, summary) in [
            ("摘要", json!([])),
            ("", json!([{"type":"summary_text", "text":"摘要"}])),
            ("摘", json!([{"type":"summary_text", "text":"摘要"}])),
        ] {
            let item = json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":summary});
            let frames = [
                json!({"type":"response.reasoning_summary_text.delta", "output_index":0, "delta":delta}),
                json!({"type":"response.output_item.done", "output_index":0, "item":item}),
                json!({"type":"response.completed", "response":{"status":"completed", "output":[item]}}),
            ];
            let body = EndedStream {
                chunks: frames
                    .iter()
                    .map(|frame| Bytes::from(format!("data: {frame}\n\n")))
                    .collect(),
            };
            let cfg = config(Protocol::OpenAiResponses);
            let mut stream = ChatStream::new(Box::pin(body), &cfg, &request());
            let mut visible = String::new();
            while let Some(event) = stream.next_event().await {
                if let StreamEvent::ReasoningDelta { text, .. } = event.unwrap() {
                    visible.push_str(&text);
                }
            }
            assert_eq!(visible, "摘要", "实时摘要必须完整且不重复");
            let response = stream.finish().await.unwrap();
            let persisted = serde_json::to_string(&response.message).unwrap();
            let restored: Message = serde_json::from_str(&persisted).unwrap();
            let ContentPart::Reasoning(block) = &restored.content[0] else {
                panic!("缺少思考块")
            };
            assert_eq!(block.display_text().as_deref(), Some("摘要"));
            let request = ChatRequest {
                messages: vec![restored],
                ..Default::default()
            };
            let wire = crate::inspect_request(&cfg, &request, false).unwrap();
            assert_eq!(wire.body["input"][0], item, "本地摘要不能改写原始回传对象");
        }
    }

    #[tokio::test]
    async fn responses_incomplete_preserves_refusal_usage_and_stop_reason() {
        use serde_json::json;
        let response = decode_frames(Protocol::OpenAiResponses, &[
            json!({"type":"response.refusal.delta", "output_index":0, "delta":"不能回答"}),
            json!({"type":"response.incomplete", "response":{"status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"}, "usage":{"input_tokens":3,"output_tokens":1}}}),
        ]).await;
        assert_eq!(response.message.text(), "不能回答");
        assert_eq!(response.finish_reason, FinishReason::Length);
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 1);
    }

    #[tokio::test]
    async fn anthropic_tool_input_survives_without_json_deltas() {
        use serde_json::json;
        for input in [json!({}), json!({"path":"README.md"})] {
            let response = decode_frames(Protocol::AnthropicMessages, &[
                json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"toolu_1", "name":"read", "input":input}}),
                json!({"type":"content_block_stop", "index":0}),
                json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}),
                json!({"type":"message_stop"}),
            ]).await;
            let call = response.message.tool_calls().next().expect("工具调用存在");
            assert_eq!(call.arguments_json().unwrap(), input);
        }
    }

    #[tokio::test]
    async fn anthropic_json_deltas_replace_initial_input() {
        use serde_json::json;
        let response = decode_frames(Protocol::AnthropicMessages, &[
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"toolu_1", "name":"read", "input":{}}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"\"README.md\"}"}}),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ]).await;
        assert_eq!(
            response.message.tool_calls().next().unwrap().arguments,
            r#"{"path":"README.md"}"#
        );
    }

    #[tokio::test]
    async fn finish_rejects_a_stream_that_ends_without_a_terminal_event() {
        let cfg = config(Protocol::OpenAiChat);
        let req = request();
        let stream = stream_ending_after(
            &cfg,
            &req,
            vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"半截\"},\"finish_reason\":null}]}\n\n",
            ],
        );
        let error = stream.finish().await.unwrap_err();
        assert!(matches!(error, LlmError::Stream(_)), "{error:?}");
    }

    #[tokio::test]
    async fn finish_accepts_a_stream_that_reaches_the_terminal_event() {
        let cfg = config(Protocol::OpenAiChat);
        let req = request();
        let stream = stream_ending_after(
            &cfg,
            &req,
            vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"完整\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        let response = stream.finish().await.expect("有终态的响应");
        assert!(matches!(
            &response.message.content[0],
            crate::ContentPart::Text(text) if text == "完整"
        ));
    }

    #[tokio::test]
    async fn error_event_terminates_stream() {
        let cfg = config(Protocol::OpenAiChat);
        let req = request();
        let mut stream = stream_from_chunks(
            &cfg,
            &req,
            vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"前半\"},\"finish_reason\":null}]}\n\n",
                "data: {oops\n\n",
                "data: [DONE]\n\n",
            ],
        );
        let mut seen = Vec::new();
        while let Some(event) = stream.next_event().await {
            seen.push(event);
        }
        assert_eq!(seen.len(), 3, "Start、文本、错误，之后不再有事件");
        assert!(
            matches!(seen[0], Ok(StreamEvent::Start { .. })),
            "{:?}",
            seen[0]
        );
        assert!(
            matches!(seen[1], Ok(StreamEvent::TextDelta { .. })),
            "{:?}",
            seen[1]
        );
        assert!(matches!(seen[2], Err(LlmError::Stream(_))), "{:?}", seen[2]);

        let error = stream.finish().await.unwrap_err();
        assert!(matches!(error, LlmError::Stream(_)), "{error:?}");
    }

    #[tokio::test]
    async fn finish_folds_all_events() {
        let cfg = config(Protocol::OpenAiChat);
        let req = request();
        let mut stream = stream_from_chunks(
            &cfg,
            &req,
            vec![
                "data: {\"id\":\"chatcmpl-1\",\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ],
        );
        // 消费一个事件后再 finish：剩余事件由 finish 读尽。
        let first = stream.next_event().await.unwrap().unwrap();
        assert!(matches!(first, StreamEvent::Start { .. }), "{first:?}");
        let response = stream.finish().await.unwrap();
        assert_eq!(response.message.text(), "你好");
        assert_eq!(response.finish_reason, FinishReason::Stop);
        assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    }

    #[tokio::test]
    async fn interrupt_returns_partial_response_without_await() {
        let cfg = config(Protocol::AnthropicMessages);
        let req = request();
        let mut stream = stream_from_chunks(
            &cfg,
            &req,
            vec![
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"test\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"写到一半\"}}\n\n",
            ],
        );
        // 只读到中途就打断：这正是用户插话的时序。
        assert!(stream.next_event().await.is_some());
        assert!(stream.next_event().await.is_some());
        let partial = stream.interrupt();
        assert_eq!(partial.finish_reason, FinishReason::Interrupted);
        assert_eq!(partial.message.text(), "写到一半");
        assert_eq!(partial.id.as_deref(), Some("msg_1"));
    }

    #[test]
    fn usage_is_merged_across_events() {
        // Anthropic 的用量分两次给出：message_start 只有输入，message_delta 只有输出。
        let accumulator = accumulate(
            Protocol::AnthropicMessages,
            &[
                StreamEvent::Usage(Usage {
                    input_tokens: 9,
                    output_tokens: 1,
                    cached_input_tokens: Some(4),
                    ..Default::default()
                }),
                StreamEvent::Usage(Usage {
                    output_tokens: 12,
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        );
        let response = accumulator.fold(Protocol::AnthropicMessages, false);
        assert_eq!(response.usage.input_tokens, 9);
        assert_eq!(response.usage.output_tokens, 12);
        assert_eq!(response.usage.cached_input_tokens, Some(4));
    }

    #[test]
    fn accumulator_ignores_empty_text_blocks() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::TextDelta {
                    index: 0,
                    text: String::new(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ],
        );
        assert!(
            accumulator
                .fold(Protocol::OpenAiChat, false)
                .message
                .content
                .is_empty()
        );
    }

    #[test]
    fn chat_stream_is_send_and_unpin() {
        // 契约：ChatStream 可以 `tokio::spawn` 到独立任务中消费。
        fn assert_send_unpin<T: Send + Unpin>() {}
        assert_send_unpin::<ChatStream>();
    }

    #[tokio::test]
    async fn guard_drops_upstream_and_discards_reasoning_and_tools_in_same_chunk() {
        struct DroppedBody {
            data: Option<Bytes>,
            dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Stream for DroppedBody {
            type Item = reqwest::Result<Bytes>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                match self.data.take() {
                    Some(data) => Poll::Ready(Some(Ok(data))),
                    None => Poll::Pending,
                }
            }
        }
        impl Drop for DroppedBody {
            fn drop(&mut self) {
                self.dropped
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frame = serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":"再次检查结果并确认当前判断。".repeat(100)},"finish_reason":null}]});
        let body = DroppedBody {
            data: Some(Bytes::from(format!("data: {frame}\n\ndata: [DONE]\n\n"))),
            dropped: std::sync::Arc::clone(&dropped),
        };
        let mut stream = ChatStream::new(Box::pin(body), &config(Protocol::OpenAiChat), &request());
        let mut events = Vec::new();
        while let Some(event) = stream.next_event().await {
            events.push(event);
        }
        assert!(dropped.load(std::sync::atomic::Ordering::Relaxed));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Err(LlmError::Guard(_))))
        );
        let partial = stream.interrupt();
        assert!(partial.message.content.is_empty());
        assert_eq!(partial.finish_reason, FinishReason::LoopDetected);
    }
}
