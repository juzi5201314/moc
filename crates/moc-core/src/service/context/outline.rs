//! 来源目录：只保留模型实际编码的内容；隐藏策略在正文进入快照之前执行。
use super::{Snapshot, view::View};
use crate::compact;
use crate::compact::replay::ContextOrigin;
use moc_llm::{ChatRequest, ContentPart, Message, RequestInspection, Role};
use moc_protocol::methods::{ContextContentSection, ContextUsageResult};
use serde_json::Value;

pub(super) fn build(
    snapshot: Snapshot,
    request: ChatRequest,
    inspection: Result<RequestInspection, moc_llm::LlmError>,
    usage: ContextUsageResult,
) -> View {
    let mut view = View::new(usage, snapshot.model.selector.clone());
    let inspection = match inspection {
        Ok(inspection) => inspection,
        Err(error) => {
            view.set_error(error);
            return view;
        }
    };
    for piece in snapshot.pieces {
        if piece.name == crate::system_prompt::PIECE_REPO_RULES {
            let first_file = piece
                .text
                .find("\n<file path=\"")
                .expect("规则片段包含已加载文件");
            add_text(
                &mut view,
                piece.title,
                "规则加载说明".into(),
                None,
                "markdown",
                format!("{}\n</repo-rules>", &piece.text[..first_file]),
            );
            for file in snapshot.context_files.iter() {
                let scope = match file.scope {
                    crate::system_prompt::context_files::ContextFileScope::Project => "项目",
                    crate::system_prompt::context_files::ContextFileScope::Global => "全局",
                };
                let file_name = file.path.file_name().unwrap_or_default().to_string_lossy();
                let newline = if file.content.ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                add_text(
                    &mut view,
                    piece.title,
                    format!("{scope} · {file_name}"),
                    Some(file.path.display().to_string()),
                    "markdown",
                    format!(
                        "<file path=\"{}\">\n{}{newline}</file>",
                        file.path.display(),
                        file.content
                    ),
                );
            }
        } else {
            add_text(
                &mut view,
                piece.title,
                piece.name.into(),
                None,
                "markdown",
                piece.text,
            );
        }
    }
    if let Some(tools) = inspection.body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            let name = tool
                .pointer("/function/name")
                .or_else(|| tool.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("工具定义");
            add_text(
                &mut view,
                "tools",
                name.into(),
                Some("工具定义与参数 schema".into()),
                "json",
                serde_json::to_string_pretty(tool).expect("JSON 值总能序列化"),
            );
        }
    }
    // 来源按请求消息顺序生成，逐段消费，避免对每条历史消息重扫整张来源表。
    let mut sources = inspection.sources.as_slice();
    for (index, message) in request
        .messages
        .iter()
        .enumerate()
        .skip(snapshot.system.messages.len())
    {
        let start = sources.partition_point(|source| source.message_index < index);
        sources = &sources[start..];
        let end = sources.partition_point(|source| source.message_index == index);
        let message_sources = &sources[..end];
        sources = &sources[end..];
        let source = &snapshot.sources[index];
        let name = source.label.split(" · ").next().unwrap_or(&source.label);
        let hidden = hidden_reason(message, source.origin);
        let mut meta = ContextContentSection {
            name: format!("{} · {name}", index + 1 - snapshot.system.messages.len()),
            kind: "markdown".into(),
            source: Some(source.label.clone()),
            tokens: compact::messages_tokens(std::slice::from_ref(message)),
            hidden: hidden.map(str::to_owned),
            ..Default::default()
        };
        let text = if hidden.is_some() {
            None
        } else {
            // 一个 IR 消息可能映射为多个线上条目；只取最外层坐标，避免父子重复。
            let mut selected: Vec<&moc_llm::RequestSource> = Vec::new();
            for item in message_sources {
                if selected.iter().any(|parent| {
                    parent.text_range.is_none()
                        && (item.pointer == parent.pointer
                            || item
                                .pointer
                                .strip_prefix(&parent.pointer)
                                .is_some_and(|rest| rest.starts_with('/')))
                }) {
                    continue;
                }
                if item.text_range.is_none() {
                    selected.retain(|child| {
                        child.pointer != item.pointer
                            && !child
                                .pointer
                                .strip_prefix(&item.pointer)
                                .is_some_and(|rest| rest.starts_with('/'))
                    });
                }
                selected.push(item);
            }
            let mut body = String::new();
            for item in selected {
                if let Some(value) = inspection.body.pointer(&item.pointer) {
                    if let Some(range) = &item.text_range {
                        if let Some(text) = value.as_str().and_then(|text| text.get(range.clone()))
                        {
                            if !body.is_empty() {
                                body.push_str("\n\n");
                            }
                            body.push_str(text);
                        }
                    } else {
                        render_content(value, &mut body);
                    }
                }
            }
            if body.is_empty() {
                meta.hidden = Some("当前请求未包含可读正文".into());
                None
            } else {
                Some(body)
            }
        };
        view.add("messages", meta, text);
    }
    view
}

fn add_text(
    view: &mut View,
    category: &str,
    name: String,
    source: Option<String>,
    kind: &str,
    text: String,
) {
    let tokens = compact::estimate_tokens(&text);
    view.add(
        category,
        ContextContentSection {
            name,
            source,
            kind: kind.into(),
            tokens,
            ..Default::default()
        },
        Some(text),
    );
}

fn hidden_reason(message: &Message, origin: ContextOrigin) -> Option<&'static str> {
    if message.role == Role::Tool
        || origin == ContextOrigin::Tool
        || message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolResult(_)))
    {
        return Some("工具输出已隐藏");
    }
    if origin == ContextOrigin::User {
        let mut chars = 0;
        let mut lines = 0;
        for part in message.content.iter() {
            if let ContentPart::Text(text) = part {
                chars += text.chars().take(2001).count();
                lines += text.lines().take(81).count();
                if chars > 2000 || lines > 80 {
                    return Some("用户输入超过 2,000 字符或 80 行，正文已隐藏");
                }
            }
        }
    }
    None
}

fn render_content(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
        Value::Array(items) => {
            for item in items {
                render_content(item, out);
            }
        }
        Value::Object(object) => {
            let mut readable = false;
            for key in [
                "content",
                "text",
                "thinking",
                "reasoning_content",
                "summary",
            ] {
                if let Some(content) = object.get(key).filter(|value| !value.is_null()) {
                    render_content(content, out);
                    readable = true;
                }
            }
            if let Some(calls) = object.get("tool_calls") {
                render_json(calls, out);
                readable = true;
            }
            if !readable {
                render_json(value, out);
            }
        }
        _ => render_json(value, out),
    }
}

fn render_json(value: &Value, out: &mut String) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("```json\n");
    out.push_str(&serde_json::to_string_pretty(value).expect("JSON 值总能序列化"));
    out.push_str("\n```");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_limits_use_unicode_characters_and_either_boundary() {
        for (text, hidden) in [
            ("中".repeat(2000), false),
            ("🙂".repeat(2001), true),
            (format!("{}末行", "行\n".repeat(79)), false),
            (format!("{}末行", "行\n".repeat(80)), true),
        ] {
            let message = Message::user_text(&text);
            assert_eq!(
                hidden_reason(&message, ContextOrigin::User).is_some(),
                hidden
            );
            assert!(hidden_reason(&message, ContextOrigin::Other).is_none());
        }
        let manual = Message::user_text("<bash>输出</bash>");
        assert!(hidden_reason(&manual, ContextOrigin::Tool).is_some());
        assert!(
            hidden_reason(&manual, ContextOrigin::User).is_none(),
            "文本不能冒充来源"
        );
        let tool = Message {
            role: Role::Tool,
            content: vec![ContentPart::Text("短输出".into())].into(),
        };
        assert!(hidden_reason(&tool, ContextOrigin::Tool).is_some());
    }
}
