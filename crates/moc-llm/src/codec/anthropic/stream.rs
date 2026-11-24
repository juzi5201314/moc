use moc_i18n::t;

use super::super::StreamDecoder;
use super::super::sse::SseEvent;
use super::AnthropicStreamDecoder;
use super::response::decode_usage;
use super::response::index_of;
use super::response::kind_from_error_type;
use super::response::map_stop_reason;
use super::response::optional_string_at;
use super::response::string_at;
use crate::config::LlmConfig;
use crate::error::ApiError;
use crate::error::LlmError;
use crate::types::ChatRequest;
use crate::types::StreamEvent;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

impl AnthropicStreamDecoder {
    /// 构造。
    pub fn new(_cfg: &LlmConfig, _req: &ChatRequest) -> Self {
        Self {
            started: false,
            terminated: false,
            stop_reason: None,
            server_tool_indices: BTreeSet::new(),
            tool_inputs: BTreeMap::new(),
            usage: serde_json::Map::new(),
        }
    }

    /// 把一次 usage 快照的非空字段并入累计原始用量，返回归一化后的完整用量。
    fn merge_usage(&mut self, usage: &Value) -> StreamEvent {
        if let Some(fields) = usage.as_object() {
            for (key, value) in fields {
                if !value.is_null() {
                    self.usage.insert(key.clone(), value.clone());
                }
            }
        }
        StreamEvent::Usage(decode_usage(Some(&Value::Object(self.usage.clone()))))
    }

    pub(super) fn decode_payload(
        &mut self,
        value: &Value,
        out: &mut Vec<Result<StreamEvent, LlmError>>,
    ) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                if !self.started {
                    self.started = true;
                    out.push(Ok(StreamEvent::Start {
                        id: optional_string_at(value, "/message/id"),
                        model: optional_string_at(value, "/message/model"),
                        effort: super::super::reported_effort(
                            value.get("message").unwrap_or(value),
                        ),
                    }));
                }
                if let Some(usage) = value.pointer("/message/usage").filter(|u| !u.is_null()) {
                    out.push(Ok(self.merge_usage(usage)));
                }
            }
            "content_block_start" => {
                let index = index_of(value);
                let Some(block) = value.get("content_block") else {
                    return;
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        out.push(Ok(StreamEvent::ToolCallStart {
                            index,
                            id: string_at(block, "/id"),
                            name: string_at(block, "/name"),
                        }));
                        if let Some(input) = block.get("input") {
                            self.tool_inputs.insert(index, input.clone());
                        }
                    }
                    Some("redacted_thinking") => {
                        out.push(Ok(StreamEvent::ReasoningItem {
                            index,
                            item: Box::new(json!({
                                "type": "redacted_thinking",
                                "data": string_at(block, "/data"),
                            })),
                        }));
                    }
                    // 服务端工具由上游自己执行：它的入参不是客户端工具调用，
                    // 记下下标后由 `input_json_delta` 分支整段跳过。
                    Some("server_tool_use" | "mcp_tool_use") => {
                        self.server_tool_indices.insert(index);
                    }
                    Some("text") => {
                        if let Some(text) = block
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            out.push(Ok(StreamEvent::TextDelta {
                                index,
                                text: text.to_owned(),
                            }));
                        }
                    }
                    Some("thinking") => {
                        if let Some(text) = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                        {
                            out.push(Ok(StreamEvent::ReasoningDelta {
                                index,
                                text: text.to_owned(),
                            }));
                        }
                        if let Some(signature) = block
                            .get("signature")
                            .and_then(Value::as_str)
                            .filter(|signature| !signature.is_empty())
                        {
                            out.push(Ok(StreamEvent::ReasoningSignature {
                                index,
                                signature: signature.to_owned(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = index_of(value);
                let Some(delta) = value.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => out.push(Ok(StreamEvent::TextDelta {
                        index,
                        text: string_at(delta, "/text"),
                    })),
                    Some("thinking_delta") => out.push(Ok(StreamEvent::ReasoningDelta {
                        index,
                        text: string_at(delta, "/thinking"),
                    })),
                    Some("signature_delta") => out.push(Ok(StreamEvent::ReasoningSignature {
                        index,
                        signature: string_at(delta, "/signature"),
                    })),
                    Some("input_json_delta") => {
                        // 服务端工具的入参不是客户端工具调用：折进消息会得到
                        // id/name 全空的 ToolCall，下一轮回灌必被上游 400 拒绝。
                        if self.server_tool_indices.contains(&index) {
                            return;
                        }
                        let json = string_at(delta, "/partial_json");
                        if !json.is_empty() {
                            self.tool_inputs.remove(&index);
                            out.push(Ok(StreamEvent::ToolCallArgumentsDelta { index, json }));
                        }
                    }
                    // 引用信息不由本 crate 承载。
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = index_of(value);
                if let Some(input) = self.tool_inputs.remove(&index) {
                    out.push(Ok(StreamEvent::ToolCallSnapshot {
                        index,
                        id: None,
                        name: None,
                        arguments: Some(input.to_string()),
                    }));
                }
                self.server_tool_indices.remove(&index);
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
                    out.push(Ok(self.merge_usage(usage)));
                }
            }
            "message_stop" => {
                self.terminated = true;
                out.push(Ok(StreamEvent::Done {
                    finish_reason: self.stop_reason.take().unwrap_or_default(),
                }));
            }
            "error" => {
                self.terminated = true;
                let error_type = string_at(value, "/error/type");
                let message = string_at(value, "/error/message");
                let mut error = ApiError::new(
                    None,
                    kind_from_error_type(&error_type, &message),
                    message,
                    value.to_string(),
                );
                if !error_type.is_empty() {
                    error.code = Some(error_type);
                }
                out.push(Err(LlmError::Api(Box::new(error))));
            }
            // ping 与未知扩展事件不改变已知内容。
            _ => {}
        }
    }
}

impl StreamDecoder for AnthropicStreamDecoder {
    fn decode(&mut self, event: SseEvent, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        let data = event.data.trim();
        if data.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(value) => self.decode_payload(&value, out),
            Err(err) => {
                self.terminated = true;
                out.push(Err(LlmError::Stream(
                    t!("llm.err.anthropic_stream_json", err = err).into_owned(),
                )));
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        out.push(Ok(StreamEvent::Done {
            finish_reason: self.stop_reason.take().unwrap_or_default(),
        }));
    }

    fn is_terminated(&self) -> bool {
        self.terminated
    }
}
