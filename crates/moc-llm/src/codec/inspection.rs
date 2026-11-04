//! 实际请求体与输入消息的来源坐标；只在检查时收集，不改变发送内容。
use std::ops::Range;
#[cfg(test)]
mod tests;
use crate::{ChatRequest, ContentPart, LlmConfig, LlmError, Message, Protocol, Role};
use serde_json::Value;

#[derive(Debug)]
pub struct RequestInspection {
    pub body: Value,
    pub sources: Vec<RequestSource>,
}

#[derive(Debug, Clone)]
pub struct RequestSource {
    pub message_index: usize,
    pub part_index: usize,
    /// 最终请求体中的 RFC 6901 路径。
    pub pointer: String,
    /// 指向字符串时，原内容在该字符串中的 UTF-8 字节范围。
    pub text_range: Option<Range<usize>>,
}

/// 与网络发送复用编码器；不联网，不暴露请求头。
pub fn inspect_request(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<RequestInspection, LlmError> {
    super::validate_tool_history(&req.messages)?;
    let mut sources = Vec::new();
    let encoded = match cfg.protocol {
        Protocol::OpenAiChat => super::chat::encode_inspected(cfg, req, stream, Some(&mut sources)),
        Protocol::OpenAiResponses => {
            super::responses::encode_inspected(cfg, req, stream, Some(&mut sources))
        }
        Protocol::AnthropicMessages => {
            super::anthropic::encode_inspected(cfg, req, stream, Some(&mut sources))
        }
    }?;
    let body = serde_json::from_slice(&encoded.body)
        .map_err(|error| LlmError::Encode(error.to_string()))?;
    Ok(RequestInspection { body, sources })
}

pub(super) fn source(
    out: &mut Vec<RequestSource>,
    message: usize,
    part: usize,
    pointer: String,
    text_range: Option<Range<usize>>,
) {
    out.push(RequestSource {
        message_index: message,
        part_index: part,
        pointer,
        text_range,
    });
}

/// 指令编码器用两个换行连接文本块。这里仅记录坐标，不再次编码正文。
pub(super) fn instruction_sources(
    out: &mut Vec<RequestSource>,
    message_index: usize,
    message: &Message,
    pointer: &str,
) {
    let mut start = 0;
    for (part_index, part) in message.content.iter().enumerate() {
        if let ContentPart::Text(text) = part {
            source(
                out,
                message_index,
                part_index,
                pointer.to_owned(),
                Some(start..start + text.len()),
            );
            start += text.len() + 2;
        }
    }
}

/// Chat/Responses 已编码条目的来源。相邻消息的结构由编码循环决定，此处只附加标签坐标。
pub(super) fn openai_sources(
    out: &mut Vec<RequestSource>,
    message_index: usize,
    message: &Message,
    items: &[Value],
    first: usize,
    root: &str,
) {
    for (offset, item) in items.iter().enumerate() {
        let path = format!("/{root}/{}", first + offset);
        source(out, message_index, 0, path.clone(), None);
        if offset != 0 || message.role == Role::Assistant {
            continue;
        }
        if matches!(message.role, Role::System | Role::Developer | Role::Custom) {
            let pointer = if item["content"].is_string() {
                format!("{path}/content")
            } else if item["content"].is_array() {
                format!("{path}/content/0/text")
            } else {
                continue;
            };
            instruction_sources(out, message_index, message, &pointer);
        } else if message.role == Role::User {
            for (part, content) in message.content.iter().enumerate() {
                let pointer = if item["content"].is_string() {
                    format!("{path}/content")
                } else {
                    format!("{path}/content/{part}")
                };
                match content {
                    ContentPart::Text(text) => {
                        let pointer = if item["content"].is_array() {
                            format!("{pointer}/text")
                        } else {
                            pointer
                        };
                        source(out, message_index, part, pointer, Some(0..text.len()));
                    }
                    _ => source(out, message_index, part, pointer, None),
                }
            }
        }
    }
}
