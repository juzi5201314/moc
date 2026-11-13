use super::InjectedMessage;
use super::MAX_INJECTED_MESSAGES;
use super::ToolResult;
use serde_json::Value;

pub(super) fn prompt_text_of(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .or_else(|| item.get("text")?.as_str().map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(super) fn interpret_tool_result(value: Value) -> ToolResult {
    match value {
        Value::String(content) => ToolResult {
            content,
            structured: None,
            is_error: false,
        },
        Value::Object(object) if object.contains_key("content") => ToolResult {
            content: object
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            structured: object.get("structuredContent").cloned(),
            is_error: object
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        Value::Null => ToolResult {
            content: String::new(),
            structured: None,
            is_error: false,
        },
        other => ToolResult {
            content: other.to_string(),
            structured: Some(other),
            is_error: false,
        },
    }
}

/// 把 `sessionStart` 返回值里的 `messages` 解析成注入消息，非法条目只告警。
pub(super) fn collect_injected(
    plugin: &str,
    value: &Value,
    out: &mut Vec<InjectedMessage>,
    budget: &mut usize,
) {
    let Some(items) = value.get("messages").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if out.len() >= MAX_INJECTED_MESSAGES {
            tracing::warn!(plugin, "sessionStart 注入的消息条数超过上限，剩余项已忽略");
            break;
        }
        let Some(text) = item.get("text").and_then(Value::as_str) else {
            tracing::warn!(plugin, "sessionStart 注入的消息缺少 text，已忽略");
            continue;
        };
        let role = match item.get("role").and_then(Value::as_str) {
            None | Some("user") => "user",
            Some("system") => "system",
            Some(other) => {
                tracing::warn!(
                    plugin,
                    role = other,
                    "注入消息的 role 只能是 system 或 user，已忽略"
                );
                continue;
            }
        };
        let text: String = text.trim().chars().take(*budget).collect();
        if text.is_empty() {
            continue;
        }
        *budget = budget.saturating_sub(text.chars().count());
        out.push(InjectedMessage {
            role: role.to_owned(),
            text,
        });
        if *budget == 0 {
            tracing::warn!(
                plugin,
                "sessionStart 注入消息的字符预算已用尽，剩余项已忽略"
            );
            break;
        }
    }
}
