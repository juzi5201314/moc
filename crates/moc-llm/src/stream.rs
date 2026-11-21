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

use rust_i18n::t;

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

    /// 已交付文本不可撤回；仅当快照以已交付增量为前缀时补发缺失后缀，兼容网关不一致时保留已收增量。
    fn text_suffix(&self, index: usize, text: &str) -> Option<usize> {
        let Some(&slot) = self.slots.get(&(BlockKind::Text, index)) else {
            return Some(0);
        };
        let AccumulatedBlock::Text { text: previous, .. } = &self.blocks[slot] else {
            unreachable!("正文下标只引用正文块");
        };
        text.starts_with(previous).then_some(previous.len())
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
    /// 流内错误沿用握手响应的 Retry-After 和请求 ID。
    retry_after: Option<std::time::Duration>,
    request_id: Option<String>,
    gateway_id: Option<String>,
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
            retry_after: None,
            request_id: None,
            gateway_id: None,
        }
    }

    /// 当前尝试已知的用量；失败时也需记账。
    pub fn usage(&self) -> Usage {
        self.accumulator.usage
    }

    /// 已收工具块的权威身份；index 只在当前流内有效，不复制参数正文。
    pub fn tool_call_identities(&self) -> Vec<(usize, String)> {
        self.accumulator
            .slots
            .iter()
            .filter_map(|((kind, index), slot)| {
                if *kind != BlockKind::ToolCall {
                    return None;
                }
                match &self.accumulator.blocks[*slot] {
                    AccumulatedBlock::ToolCall { id, .. } => Some((*index, id.clone())),
                    _ => unreachable!("工具索引只指向工具块"),
                }
            })
            .collect()
    }

    pub(crate) fn set_response_headers(&mut self, headers: &reqwest::header::HeaderMap) {
        let mut metadata = crate::ApiError::new(None, crate::ApiErrorKind::Unknown, "", "");
        crate::codec::error_metadata(&mut metadata, headers);
        self.retry_after = metadata.retry_after;
        self.request_id = metadata.request_id;
        self.gateway_id = metadata.gateway_id;
    }

    /// 初始化事件不算有效输出；内容仍留在队列中，调用方不会丢失首个增量。
    pub(crate) async fn prime(&mut self) -> Result<(), LlmError> {
        std::future::poll_fn(|cx| {
            loop {
                if self
                    .queue
                    .iter()
                    .any(|event| event.as_ref().is_ok_and(StreamEvent::starts_output))
                {
                    return Poll::Ready(Ok(()));
                }
                if let Some(error) = &self.error {
                    // 本地守卫交回流消费者，由会话层执行专用纠偏，不能混入传输重试。
                    return Poll::Ready(if matches!(error, LlmError::Guard(_)) {
                        Ok(())
                    } else {
                        Err(error.clone())
                    });
                }
                if self.finished {
                    return Poll::Ready(if self.terminal_seen {
                        Ok(())
                    } else {
                        Err(LlmError::Stream(t!("llm.err.incomplete").into_owned()))
                    });
                }
                if self.pump(cx).is_pending() {
                    return Poll::Pending;
                }
            }
        })
        .await
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
            None if !self.terminal_seen => {
                Err(LlmError::Stream(t!("llm.err.incomplete").into_owned()))
            }
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
                self.stop_with(LlmError::Stream(
                    t!("llm.err.read_body_failed", err = err).into_owned(),
                ));
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
        if self.sse.is_overflowed() {
            self.stop_with(LlmError::Stream(format!(
                "SSE 缓冲区超过 {} 字节上限",
                crate::codec::sse::MAX_BUFFERED_BYTES
            )));
        }
    }

    /// 把暂存事件交给累加器与调用方。
    fn flush_pending(&mut self) {
        let mut pending = std::mem::take(&mut self.pending);
        for event in pending.drain(..) {
            let event = match event {
                Ok(StreamEvent::TextSnapshot { index, mut text }) => {
                    let Some(prefix) = self.accumulator.text_suffix(index, &text) else {
                        continue;
                    };
                    if prefix == text.len() {
                        continue;
                    }
                    text.drain(..prefix);
                    Ok(StreamEvent::TextDelta { index, text })
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

    fn enqueue(&mut self, mut event: Result<StreamEvent, LlmError>) -> bool {
        if let Err(LlmError::Api(api)) = &mut event {
            api.retry_after = self.retry_after.or(api.retry_after);
            if self.request_id.is_some() {
                api.request_id.clone_from(&self.request_id);
            }
            api.gateway_id.clone_from(&self.gateway_id);
        }
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
mod tests;
