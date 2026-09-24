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
use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use tokio::time::Sleep;

use crate::codec::StreamDecoder;
use crate::codec::sse::SseDecoder;
use crate::config::{LlmConfig, Protocol};
use crate::error::LlmError;
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
    },
    Reasoning {
        text: String,
        signature: Option<String>,
        /// Responses 的完整 reasoning item（`output_item.done` 给出）。
        item: Option<Box<serde_json::Value>>,
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
    blocks: Vec<AccumulatedBlock>,
    slots: HashMap<(BlockKind, usize), usize>,
    usage: Usage,
    finish_reason: Option<FinishReason>,
}

impl Accumulator {
    /// 消费一个事件。
    fn apply(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Start { id, model } => {
                if self.id.is_none() {
                    self.id = id.clone();
                }
                if self.model.is_none() {
                    self.model = model.clone();
                }
            }
            StreamEvent::TextDelta { index, text } => {
                self.with_block(BlockKind::Text, *index, |slot| match slot {
                    AccumulatedBlock::Text { text: buffer, .. } => buffer.push_str(text),
                    _ => unreachable!("同一种类同一 index 的块结构固定"),
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
                self.with_block(BlockKind::Reasoning, *index, |slot| match slot {
                    // 完整 item 是权威形态（含 encrypted_content），
                    // 覆盖增量阶段积累的摘要文本。
                    AccumulatedBlock::Reasoning { item: target, .. } => {
                        *target = Some(item.clone());
                    }
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
                        if id_buffer.is_empty() && !id.is_empty() {
                            id_buffer.push_str(id);
                        }
                        // 少数网关把工具名拆到多个 chunk，此时拼接。
                        name_buffer.push_str(name);
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
            StreamEvent::Usage(usage) => self.usage.merge(*usage),
            StreamEvent::Done { finish_reason } => {
                self.finish_reason = Some(finish_reason.clone());
            }
        }
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
                    },
                    BlockKind::Reasoning => AccumulatedBlock::Reasoning {
                        text: String::new(),
                        signature: None,
                        item: None,
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
            message: Message {
                role: Role::Assistant,
                content: self.content(protocol, interrupted),
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
                AccumulatedBlock::Text { text, .. } => {
                    if !text.is_empty() {
                        content.push(ContentPart::Text(text.clone()));
                    }
                }
                AccumulatedBlock::Reasoning {
                    text,
                    signature,
                    item,
                    ..
                } => {
                    if interrupted {
                        // Responses 的 reasoning item 与其后的 function_call 成对
                        // 使用，孤立 item 无意义。
                        if item.is_some() && protocol == Protocol::OpenAiResponses && !has_tool_call
                        {
                            continue;
                        }
                        // Anthropic 的 thinking 块缺签名会被上游拒绝（签名在块末尾
                        // 的 signature_delta 中到达）。
                        if item.is_none()
                            && protocol == Protocol::AnthropicMessages
                            && signature.as_deref().unwrap_or_default().is_empty()
                        {
                            continue;
                        }
                    }
                    match item {
                        Some(item) => {
                            content
                                .push(ContentPart::Reasoning(ReasoningBlock::Item(item.clone())));
                        }
                        None => {
                            if !text.is_empty() || signature.is_some() {
                                content.push(ContentPart::Reasoning(ReasoningBlock::Text {
                                    text: text.clone(),
                                    signature: signature.clone(),
                                }));
                            }
                        }
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
    /// 协议，决定中断时的修剪规则。
    protocol: Protocol,
    /// 空闲超时：两次字节到达之间的最长间隔。
    idle_timeout: Option<Duration>,
    /// 空闲计时器。
    idle: Option<Pin<Box<Sleep>>>,
}

impl ChatStream {
    /// 由客户端构造。
    pub(crate) fn new(
        body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
        cfg: &LlmConfig,
        req: &ChatRequest,
    ) -> ChatStream {
        let mut stream = ChatStream {
            body,
            sse: SseDecoder::new(),
            decoder: crate::codec::stream_decoder(cfg, req),
            queue: VecDeque::new(),
            pending: Vec::new(),
            accumulator: Accumulator::default(),
            error: None,
            finished: false,
            protocol: cfg.protocol,
            idle_timeout: cfg.timeout_secs.map(Duration::from_secs_f64),
            idle: None,
        };
        // 从响应头到达开始计时：首个字节同样受空闲上限约束。
        stream.reset_idle();
        stream
    }

    /// 取出下一个事件；流结束或出错后返回 `None`。
    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, LlmError>> {
        std::future::poll_fn(|cx| Pin::new(&mut *self).poll_next(cx)).await
    }

    /// 读完剩余事件并折叠为完整响应；流中发生过错误则返回该错误。
    pub async fn finish(mut self) -> Result<ChatResponse, LlmError> {
        while self.next_event().await.is_some() {}
        match self.error {
            Some(error) => Err(error),
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
    /// | Anthropic 下丢弃无签名的思维链块 | 缺签名会被上游拒绝（签名在块末尾到达） |
    /// | Responses 下没有保留工具调用时丢弃 reasoning item | reasoning item 与其后的 function_call 成对使用，孤立 item 无意义 |
    /// | 文本与已累积的思维链文本保留 | 用户看到的内容不丢 |
    pub fn interrupt(self) -> ChatResponse {
        self.accumulator.fold(self.protocol, true)
    }

    /// 读一块字节并把解出的事件推入队列；返回 `Pending` 表示网络暂无数据。
    fn pump(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self.body.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                self.sse.push(&chunk);
                self.reset_idle();
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
            if self.decoder.is_terminated() {
                // 终态：不再读网络，连接随 drop 关闭。
                self.finished = true;
                return;
            }
        }
    }

    /// 把暂存事件交给累加器与调用方。
    fn flush_pending(&mut self) {
        for event in self.pending.drain(..) {
            match &event {
                Ok(stream_event) => self.accumulator.apply(stream_event),
                Err(error) => self.error = Some(error.clone()),
            }
            self.queue.push_back(event);
        }
    }

    /// 以错误结束流。
    fn stop_with(&mut self, error: LlmError) {
        self.finished = true;
        self.error = Some(error.clone());
        self.queue.push_back(Err(error));
    }

    /// 重新开始空闲计时。
    ///
    /// 计时器对象只创建一次，之后原地 `reset`：每收到一块字节就新建一个 `Sleep`
    /// 会让流式热路径带上逐块分配。
    fn reset_idle(&mut self) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        let deadline = tokio::time::Instant::now() + timeout;
        match self.idle.as_mut() {
            Some(idle) => idle.as_mut().reset(deadline),
            None => self.idle = Some(Box::pin(tokio::time::sleep_until(deadline))),
        }
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
            // 空闲超时：长时间收不到任何字节就报错，而不是无限等待。
            if let Some(idle) = this.idle.as_mut()
                && idle.as_mut().poll(cx).is_ready()
            {
                this.stop_with(LlmError::Stream(format!(
                    "流空闲超过 {:?}",
                    this.idle_timeout.unwrap_or_default()
                )));
                continue;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Auth, EndpointStyle};
    use crate::types::{Effort, ReasoningEcho};

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
            vec![ContentPart::Reasoning(ReasoningBlock::Item(Box::new(item)))]
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
    fn tool_call_name_fragments_concatenated() {
        let accumulator = accumulate(
            Protocol::OpenAiChat,
            &[
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{}".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
        );
        let response = accumulator.fold(Protocol::OpenAiChat, false);
        assert_eq!(
            response.message.content,
            vec![ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{}"
            ))]
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
                ContentPart::Reasoning(ReasoningBlock::Item(Box::new(item))),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
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
    async fn drop_cancels_stream_without_panic() {
        let cfg = config(Protocol::OpenAiChat);
        let req = request();
        let stream = stream_from_chunks(
            &cfg,
            &req,
            vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
            ],
        );
        drop(stream);
    }

    #[tokio::test]
    async fn idle_timeout_errors_instead_of_hanging() {
        let cfg = LlmConfig {
            timeout_secs: Some(0.05),
            ..config(Protocol::OpenAiChat)
        };
        let req = request();
        let mut stream = stream_from_chunks(
            &cfg,
            &req,
            vec![
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
            ],
        );
        let mut error = None;
        while let Some(event) = stream.next_event().await {
            if let Err(err) = event {
                error = Some(err);
                break;
            }
        }
        let error = error.expect("空闲超时应当以错误结束流");
        assert!(error.to_string().contains("流空闲超过"), "{error}");
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
    fn effort_reexport_is_usable() {
        // 该断言只是保证测试模块引用的类型确实来自公开面。
        assert_eq!(Effort::High.to_string(), "high");
    }

    #[test]
    fn chat_stream_is_send_and_unpin() {
        // 契约：ChatStream 可以 `tokio::spawn` 到独立任务中消费。
        fn assert_send_unpin<T: Send + Unpin>() {}
        assert_send_unpin::<ChatStream>();
    }
}
