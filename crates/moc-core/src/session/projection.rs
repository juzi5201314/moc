use super::cache_budget::serialized_bytes;
use crate::error::CoreError;
use rust_i18n::t;
use moc_llm::ContentPart;
use moc_llm::Message;
use moc_llm::ReasoningBlock;
use moc_llm::Role;
use moc_llm::ToolCall;
use moc_llm::ToolResult;

/// 下标只用于当前计划投影，创建顺序稳定，状态更新不移动元素。
pub(super) fn page_plans(
    plans: &[moc_protocol::PlanInfo],
    page: &moc_protocol::methods::session::PlanPageParams,
) -> Result<(Vec<moc_protocol::PlanInfo>, Option<u32>), CoreError> {
    if page.max_entries == 0 {
        return Ok((Vec::new(), None));
    }
    let end = page.before.map_or(plans.len(), |index| index as usize);
    if end > plans.len() {
        return Err(CoreError::Session(
            "INVALID_PARAMS: 计划分页游标已失效".into(),
        ));
    }
    let limit = (page.max_entries as usize).min(32);
    let mut start = end;
    let mut bytes = 2usize;
    while start > 0 && end - start < limit {
        let size = serialized_bytes(&plans[start - 1]).saturating_add(1);
        if start < end && bytes.saturating_add(size) > 4 * 1024 * 1024 {
            break;
        }
        // 单版本正文通常至多512KiB；旧日志的超大版本仍完整返回，避免静默丢失批注。
        bytes = bytes.saturating_add(size);
        start -= 1;
    }
    let before = if start == 0 {
        None
    } else {
        Some(
            u32::try_from(start)
                .map_err(|_| CoreError::Session(t!("core.err.plans_cursor").into_owned()))?,
        )
    };
    Ok((plans[start..end].to_vec(), before))
}

pub(super) fn project_message(message: &Message, budget: usize) -> (Message, bool) {
    message
        .content
        .with_parts(|parts| project_parts(message.role, parts, budget))
}

pub(super) fn project_parts(role: Role, parts: &[ContentPart], budget: usize) -> (Message, bool) {
    let mut remaining = budget;
    let mut truncated = false;
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        let projected = match part {
            ContentPart::ResponsePhase(phase) => ContentPart::ResponsePhase(phase.clone()),
            ContentPart::Text(text) => {
                let (text, was_truncated) = truncate_text(text, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(text.len());
                ContentPart::Text(text)
            }
            ContentPart::Image { .. } => {
                truncated = true;
                ContentPart::Text("[图片内容已省略]".into())
            }
            ContentPart::Reasoning(ReasoningBlock::Text { text, .. }) => {
                let (text, was_truncated) = truncate_text(text, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(text.len());
                ContentPart::Reasoning(ReasoningBlock::text(text))
            }
            ContentPart::Reasoning(ReasoningBlock::Redacted { data }) => {
                let (data, was_truncated) = truncate_text(data, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(data.len());
                ContentPart::Reasoning(ReasoningBlock::Redacted { data })
            }
            ContentPart::Reasoning(reasoning @ ReasoningBlock::Item(_)) => {
                let display = reasoning.display_text().unwrap_or_default();
                let (text, was_truncated) = truncate_text(&display, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(text.len());
                ContentPart::Reasoning(ReasoningBlock::text(text))
            }
            ContentPart::ToolCall(call) => {
                let (arguments, was_truncated) = truncate_text(&call.arguments, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(arguments.len());
                ContentPart::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments,
                })
            }
            ContentPart::ToolResult(result) => {
                let (content, was_truncated) = truncate_text(&result.content, remaining);
                truncated |= was_truncated || !result.images.is_empty();
                remaining = remaining.saturating_sub(content.len());
                ContentPart::ToolResult(ToolResult {
                    tool_call_id: result.tool_call_id.clone(),
                    content,
                    is_error: result.is_error,
                    images: Vec::new(),
                })
            }
        };
        content.push(projected);
    }
    (
        Message {
            role,
            content: content.into(),
        },
        truncated,
    )
}

pub(super) fn truncate_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    const MARKER: &str = "…[内容已截断]…";
    if limit <= MARKER.len() {
        let mut end = limit;
        while !MARKER.is_char_boundary(end) {
            end -= 1;
        }
        return (MARKER[..end].to_owned(), true);
    }
    let available = limit - MARKER.len();
    let mut head = available / 2;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = available - head;
    while tail > 0 && !text.is_char_boundary(text.len() - tail) {
        tail -= 1;
    }
    let mut output = String::with_capacity(limit);
    output.push_str(&text[..head]);
    output.push_str(MARKER);
    output.push_str(&text[text.len() - tail..]);
    (output, true)
}
