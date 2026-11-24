//! 来源目录：只保留模型实际编码的内容；隐藏策略在正文进入快照之前执行。
use super::{Snapshot, view::View};
use crate::compact;
use crate::compact::replay::ContextOrigin;
use moc_i18n::t;
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
                t!("core.outline.rules_note").into_owned(),
                None,
                "markdown",
                format!("{}\n</repo-rules>", &piece.text[..first_file]),
            );
            for file in snapshot.context_files.iter() {
                let scope = match file.scope {
                    crate::system_prompt::context_files::ContextFileScope::Project => {
                        t!("core.outline.scope_project")
                    }
                    crate::system_prompt::context_files::ContextFileScope::Global => {
                        t!("core.outline.scope_global")
                    }
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
                .map(str::to_owned)
                .unwrap_or_else(|| t!("core.outline.tool_def").into_owned());
            add_text(
                &mut view,
                "tools",
                name,
                Some(t!("core.outline.tool_def_schema").into_owned()),
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
        let (head, rest) = match source.label.split_once(" · ") {
            Some((head, rest)) => (head, Some(rest)),
            None => (source.label.as_str(), None),
        };
        let (name, origin) = normalize_origin_label(head);
        let display_source = match rest {
            Some(rest) => format!("{name} · {rest}"),
            None => name.to_owned(),
        };
        let hidden = hidden_reason(message, source.origin);
        let mut meta = ContextContentSection {
            name: format!("{} · {name}", index + 1 - snapshot.system.messages.len()),
            kind: "markdown".into(),
            source: Some(display_source),
            tokens: compact::messages_tokens(std::slice::from_ref(message)),
            hidden: hidden.as_ref().map(|h| h.to_string()),
            role: Some(wire_role(&inspection.body, message_sources, message)),
            origin: Some(origin.into()),
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
                meta.hidden = Some(t!("core.outline.no_readable").into_owned());
                None
            } else {
                Some(body)
            }
        };
        view.add("messages", meta, text);
    }
    view
}

/// 将回放器写入的内部来源标签规范为英文工作区标签与来源分类；其余标签是合成的会话状态消息。
fn normalize_origin_label(label: &str) -> (&str, &'static str) {
    match label {
        "系统提示" => ("System prompt", "system_prompt"),
        "CLM 上下文" => ("CLM context", "clm_context"),
        "会话状态生成" => ("Session state", "session_state"),
        "代理消息" => ("Agent message", "agent_message"),
        "手动命令" => ("Manual command", "manual_command"),
        "目标续跑" => ("Goal round", "goal_round"),
        "运行时注入" => ("Runtime injection", "runtime_injection"),
        "用户输入" => ("User input", "user_input"),
        "模型输出" => ("Assistant output", "assistant_output"),
        "工具结果" => ("Tool result", "tool_result"),
        "指令" => ("Instruction", "instruction"),
        other => (other, "session_state"),
    }
}

/// 消息编码进请求后的线上角色：取首个可定位条目的 `role`；Responses 的工具条目只有 `type`。
/// 定位不到线上条目时退回 IR 角色。
fn wire_role(body: &Value, sources: &[moc_llm::RequestSource], message: &Message) -> String {
    sources
        .iter()
        .find_map(|source| {
            let item = source
                .pointer
                .match_indices('/')
                .nth(2)
                .map_or(source.pointer.as_str(), |(end, _)| &source.pointer[..end]);
            let value = body.pointer(item)?;
            if let Some(role) = value.get("role").and_then(Value::as_str) {
                return Some(role.to_owned());
            }
            match value.get("type")?.as_str()? {
                "function_call_output" => Some("tool".to_owned()),
                "function_call" | "reasoning" => Some("assistant".to_owned()),
                _ => None,
            }
        })
        .unwrap_or_else(|| message.role.as_str().to_owned())
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

fn hidden_reason(
    message: &Message,
    origin: ContextOrigin,
) -> Option<std::borrow::Cow<'static, str>> {
    if message.role == Role::Tool
        || origin == ContextOrigin::Tool
        || message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolResult(_)))
    {
        return Some(t!("core.outline.tool_hidden"));
    }
    if origin == ContextOrigin::User {
        let mut chars = 0;
        let mut lines = 0;
        for part in message.content.iter() {
            if let ContentPart::Text(text) = part {
                chars += text.chars().take(2001).count();
                lines += text.lines().take(81).count();
                if chars > 2000 || lines > 80 {
                    return Some(t!("core.outline.user_hidden"));
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
            if render_tool_call(object, out) {
                return;
            }
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
            if let Some(calls) = object.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    match call.as_object() {
                        Some(call) if render_tool_call(call, out) => {}
                        _ => render_json(call, out),
                    }
                }
                readable = true;
            }
            if !readable {
                render_json(value, out);
            }
        }
        _ => render_json(value, out),
    }
}

/// 三种协议的工具调用（Chat `tool_calls[]`、Responses `function_call`、Anthropic `tool_use`）
/// 统一写成 `tool-call` 代码块：`{name, id, arguments}`，字符串参数先按 JSON 解析，前端据此渲染卡片。
fn render_tool_call(object: &serde_json::Map<String, Value>, out: &mut String) -> bool {
    let (id, name, arguments) = match object.get("type").and_then(Value::as_str) {
        Some("function_call") => (
            object.get("call_id"),
            object.get("name"),
            object.get("arguments"),
        ),
        Some("tool_use") => (object.get("id"), object.get("name"), object.get("input")),
        _ => match object.get("function") {
            Some(function) => (
                object.get("id"),
                function.get("name"),
                function.get("arguments"),
            ),
            None => return false,
        },
    };
    let Some(name) = name.and_then(Value::as_str) else {
        return false;
    };
    let arguments = match arguments {
        Some(Value::String(text)) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Some(value) => value.clone(),
        None => Value::Null,
    };
    let card = serde_json::json!({
        "name": name,
        "id": id.cloned().unwrap_or(Value::Null),
        "arguments": arguments,
    });
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str("```tool-call\n");
    out.push_str(&serde_json::to_string_pretty(&card).expect("JSON 值总能序列化"));
    out.push_str("\n```");
    true
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
    fn tool_calls_render_as_one_card_shape_across_protocols() {
        let expected = "```tool-call\n{\n  \"name\": \"read\",\n  \"id\": \"call_1\",\n  \"arguments\": {\n    \"path\": \"a.rs\"\n  }\n}\n```";
        for wire in [
            serde_json::json!({"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.rs\"}"}),
            serde_json::json!({"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.rs"}}),
            serde_json::json!({"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}}]}),
        ] {
            let mut out = String::new();
            render_content(&wire, &mut out);
            assert_eq!(out, expected);
        }
    }

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
