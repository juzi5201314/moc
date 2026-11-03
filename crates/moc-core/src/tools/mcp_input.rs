//! MCP MRTR（SEP-2322）输入履行：把 `inputRequests` 落到 `session/request_ask`。
//!
//! server 在 `tools/call` 中返回 `input_required` 时，`moc-mcp` 的调用回路
//! 逐键把请求交给这里的 fulfiller：elicitation 表单/URL 模式经现有 ask 卡片
//! 问用户（选项题用 enum/布尔字段，纯文本题承载 string/number/integer），
//! roots 直接回工作区根，sampling 不支持（未声明 capability）。
//!
//! 答复形态遵循规范：`{"action":"accept","content":{…}}` / `decline` / `cancel`。
//! 无法按声明类型转换的字段直接省略——server 会按规范以新一轮
//! `input_required` 重新索要缺失字段，表单天然自愈。

use std::sync::Arc;

use moc_mcp::{InputFulfiller, McpError, McpToolInfo, PendingInput};
use moc_protocol::{AskAnswer, AskOption, AskParams, AskQuestion, SessionRequestAsk};
use serde_json::{Map, Value, json};

use crate::session::state::SessionState;

use super::{ToolCtx, ask::ask_settled};

/// 给单次工具调用注入的 MRTR 履行端。
pub(super) fn fulfiller(ctx: &ToolCtx, info: &McpToolInfo) -> InputFulfiller {
    let requester = ctx.request.requester();
    let session_id = ctx.session_id.clone();
    let prompt_id = ctx.prompt_id.clone();
    let cwd = ctx.cwd.clone();
    let state = ctx.state.clone();
    let ask_supported = ctx.core.client_supports_ask(ctx.request.connection_id());
    let server = info.server.clone();
    let tool = info.name.clone();
    Arc::new(move |pending| {
        let requester = requester.clone();
        let session_id = session_id.clone();
        let prompt_id = prompt_id.clone();
        let cwd = cwd.clone();
        let state = state.clone();
        let server = server.clone();
        let tool = tool.clone();
        Box::pin(async move {
            match pending {
                PendingInput::ElicitForm { message, schema } => {
                    elicit_form(
                        &requester,
                        &session_id,
                        &prompt_id,
                        &state,
                        ask_supported,
                        &server,
                        &tool,
                        &message,
                        &schema,
                    )
                    .await
                }
                PendingInput::ElicitUrl { message, url } => {
                    elicit_url(
                        &requester,
                        &session_id,
                        &prompt_id,
                        &state,
                        ask_supported,
                        &server,
                        &tool,
                        &message,
                        &url,
                    )
                    .await
                }
                PendingInput::ListRoots => Ok(json!({
                    "roots": [{
                        "uri": format!("file://{}", cwd.display()),
                        "name": cwd
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| cwd.display().to_string()),
                    }]
                })),
                PendingInput::CreateMessage { .. } => Err(McpError::Tool(format!(
                    "server `{server}` requested sampling/createMessage, which moc does not support"
                ))),
            }
        })
    })
}

fn decline_result() -> Value {
    json!({"action": "decline"})
}

/// 是否需要把问题送到前端：自主程度不打扰用户，或前端不支持 ask，都按 decline 收场。
async fn user_is_reachable(
    state: &Arc<tokio::sync::Mutex<SessionState>>,
    ask_supported: bool,
) -> bool {
    ask_supported && state.lock().await.session.autonomy < moc_protocol::SessionAutonomy::DontAsk
}

/// 表单模式：每个 schema 字段一道题，收齐后按声明类型转换回 content。
#[allow(clippy::too_many_arguments)]
async fn elicit_form(
    requester: &moc_protocol::Requester,
    session_id: &str,
    prompt_id: &str,
    state: &Arc<tokio::sync::Mutex<SessionState>>,
    ask_supported: bool,
    server: &str,
    tool: &str,
    message: &str,
    schema: &Value,
) -> Result<Value, McpError> {
    if !user_is_reachable(state, ask_supported).await {
        return Ok(decline_result());
    }
    let fields = fields_of(schema);
    if fields.is_empty() {
        // 没有字段的表单只需用户确认；空内容 + accept 即可。
        return Ok(json!({"action": "accept", "content": {}}));
    }
    let header = format!("{server} · {tool}: {message}");
    let questions: Vec<AskQuestion> = fields
        .iter()
        .map(|field| question_for(field, &header))
        .collect();
    let params = AskParams {
        session_id: session_id.to_owned(),
        prompt_id: prompt_id.to_owned(),
        request_id: crate::ids::entry_id(),
        questions,
    };
    let result = ask_settled(requester.request::<SessionRequestAsk>(params).await)
        .map_err(|error| McpError::Tool(error.to_string()))?;
    Ok(form_result(&fields, &result.answers))
}

/// URL 模式：展示目标地址，用户自行完成后确认；不按 `Done` 一律 decline。
#[allow(clippy::too_many_arguments)]
async fn elicit_url(
    requester: &moc_protocol::Requester,
    session_id: &str,
    prompt_id: &str,
    state: &Arc<tokio::sync::Mutex<SessionState>>,
    ask_supported: bool,
    server: &str,
    tool: &str,
    message: &str,
    url: &str,
) -> Result<Value, McpError> {
    if !user_is_reachable(state, ask_supported).await {
        return Ok(decline_result());
    }
    let question = AskQuestion {
        id: "url".into(),
        question: format!("{message}\n{url}"),
        header: Some(format!("{server} · {tool}")),
        multi: false,
        recommended: Vec::new(),
        options: vec![
            AskOption {
                label: "Done".into(),
                description: Some("I opened the URL and completed the interaction".into()),
                preview: None,
            },
            AskOption {
                label: "Decline".into(),
                description: Some("Do not provide the requested information".into()),
                preview: None,
            },
        ],
    };
    let params = AskParams {
        session_id: session_id.to_owned(),
        prompt_id: prompt_id.to_owned(),
        request_id: crate::ids::entry_id(),
        questions: vec![question],
    };
    let result = ask_settled(requester.request::<SessionRequestAsk>(params).await)
        .map_err(|error| McpError::Tool(error.to_string()))?;
    let done = result
        .answers
        .iter()
        .any(|answer| answer.selected.iter().any(|label| label == "Done"));
    Ok(if done {
        json!({"action": "accept"})
    } else {
        decline_result()
    })
}

/// `requestedSchema` 的一个字段：名字、属性 JSON、是否必填。
struct Field<'a> {
    name: &'a str,
    schema: &'a Value,
    required: bool,
}

/// 取出 `properties` 下的字段列表（schema 顺序即 JSON 对象顺序）。
fn fields_of(schema: &Value) -> Vec<Field<'_>> {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    schema["properties"]
        .as_object()
        .map(|properties| {
            properties
                .iter()
                .map(|(name, prop)| Field {
                    name,
                    schema: prop,
                    required: required.contains(&name.as_str()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 字段 → ask 题目。enum/多选 enum/布尔映射选项；其余类型用纯文本输入。
fn question_for(field: &Field<'_>, header: &str) -> AskQuestion {
    let prop = field.schema;
    let mut text = prop["description"]
        .as_str()
        .or_else(|| prop["title"].as_str())
        .unwrap_or(field.name)
        .to_owned();
    if field.required {
        text.push_str(" (required)");
    }
    let multi = prop["type"].as_str() == Some("array");
    let enum_values = if multi {
        prop["items"]["enum"].as_array()
    } else {
        prop["enum"].as_array()
    };
    let enum_names = if multi {
        prop["items"]["enumNames"].as_array()
    } else {
        prop["enumNames"].as_array()
    };
    let mut options = Vec::new();
    let mut recommended = Vec::new();
    if let Some(values) = enum_values {
        for (index, value) in values.iter().enumerate() {
            let label = value.as_str().unwrap_or_default().to_owned();
            let description = enum_names
                .and_then(|names| names.get(index))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(label.clone()));
            options.push(AskOption {
                label,
                description,
                preview: None,
            });
        }
        if let Some(default) = prop["default"].as_str() {
            recommended = values
                .iter()
                .position(|value| value.as_str() == Some(default))
                .map(|index| vec![index as u32])
                .unwrap_or_default();
        }
    } else if prop["type"].as_str() == Some("boolean") {
        options = vec![
            AskOption {
                label: "Yes".into(),
                description: None,
                preview: None,
            },
            AskOption {
                label: "No".into(),
                description: None,
                preview: None,
            },
        ];
        if let Some(default) = prop["default"].as_bool() {
            recommended = vec![(!default) as u32];
        }
    } else {
        // 纯文本字段：提示类型/格式，让用户知道要输什么。
        let hint = prop["format"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| prop["type"].as_str().map(str::to_owned));
        if let Some(hint) = hint {
            text.push_str(&format!(" [{hint}]"));
        }
        if let Some(default) = prop["default"].as_str() {
            text.push_str(&format!(" (default: {default})"));
        }
    }
    AskQuestion {
        id: field.name.to_owned(),
        question: text,
        header: Some(header.to_owned()),
        multi,
        recommended,
        options,
    }
}

/// 把 ask 答案按字段类型组装成 `ElicitResult` JSON；全空即 decline。
fn form_result(fields: &[Field<'_>], answers: &[AskAnswer]) -> Value {
    let mut content = Map::new();
    for field in fields {
        let Some(answer) = answers.iter().find(|answer| answer.id == field.name) else {
            continue;
        };
        if let Some(value) = coerce(field, answer) {
            content.insert(field.name.to_owned(), value);
        }
    }
    if content.is_empty() {
        decline_result()
    } else {
        json!({"action": "accept", "content": Value::Object(content)})
    }
}

/// 单个答案 → 该字段类型的 JSON；无法转换返回 `None`（字段视为未答）。
fn coerce(field: &Field<'_>, answer: &AskAnswer) -> Option<Value> {
    let prop = field.schema;
    let raw_text = || {
        answer
            .custom
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .or_else(|| answer.selected.first().cloned())
    };
    match prop["type"].as_str() {
        Some("array") => {
            let allowed: Vec<&str> = prop["items"]["enum"]
                .as_array()
                .map(|values| values.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let picked: Vec<Value> = answer
                .selected
                .iter()
                .filter(|label| allowed.is_empty() || allowed.contains(&label.as_str()))
                .map(|label| Value::String(label.clone()))
                .collect();
            (!picked.is_empty()).then_some(Value::Array(picked))
        }
        Some("boolean") => {
            let raw = raw_text()?.to_ascii_lowercase();
            match raw.as_str() {
                "yes" | "true" => Some(Value::Bool(true)),
                "no" | "false" => Some(Value::Bool(false)),
                _ => None,
            }
        }
        Some("integer") => raw_text()?.parse::<i64>().ok().map(Value::from),
        Some("number") => raw_text()?
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(Value::from),
        _ => {
            let raw = raw_text()?;
            // 单选 enum 的答案必须命中合法值；未声明 enum 的字段原样收。
            if let Some(values) = prop["enum"].as_array()
                && !values
                    .iter()
                    .any(|value| value.as_str() == Some(raw.as_str()))
            {
                return None;
            }
            Some(Value::String(raw))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Your name"},
                "age": {"type": "integer", "description": "Age"},
                "weight": {"type": "number"},
                "color": {"type": "string", "enum": ["red", "green"], "enumNames": ["红", "绿"]},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b", "c"]}},
                "newsletter": {"type": "boolean", "default": true}
            },
            "required": ["name"]
        })
    }

    #[test]
    fn questions_cover_all_field_kinds() {
        let schema = form_schema();
        let fields = fields_of(&schema);
        assert_eq!(fields.len(), 6);
        assert!(
            fields.iter().find(|f| f.name == "name").unwrap().required
                && !fields.iter().find(|f| f.name == "age").unwrap().required
        );

        let by_name = |name: &str| {
            let field = fields.iter().find(|f| f.name == name).unwrap();
            question_for(field, "srv · tool: msg")
        };
        let color = by_name("color");
        assert_eq!(color.options.len(), 2);
        assert_eq!(color.options[0].label, "red");
        assert_eq!(color.options[0].description.as_deref(), Some("红"));
        assert!(!color.multi);

        let tags = by_name("tags");
        assert!(tags.multi);
        assert_eq!(tags.options.len(), 3);

        let newsletter = by_name("newsletter");
        assert_eq!(newsletter.options.len(), 2);
        assert_eq!(newsletter.recommended, vec![0], "default=true 应预选 Yes");

        let age = by_name("age");
        assert!(age.options.is_empty(), "数字字段应为纯文本输入");
        assert!(age.question.contains("[integer]"));

        let name = by_name("name");
        assert!(name.question.contains("required"));
        assert_eq!(name.header.as_deref(), Some("srv · tool: msg"));
    }

    fn answers(pairs: &[(&str, &[&str], Option<&str>)]) -> Vec<AskAnswer> {
        pairs
            .iter()
            .map(|(id, selected, custom)| AskAnswer {
                id: id.to_string(),
                selected: selected.iter().map(|s| s.to_string()).collect(),
                custom: custom.map(|s| s.to_string()),
            })
            .collect()
    }

    #[test]
    fn coerce_produces_typed_content_and_decline() {
        let schema = form_schema();
        let fields = fields_of(&schema);

        // 全空 → decline。
        assert_eq!(
            form_result(&fields, &answers(&[("name", &[], None)])),
            json!({"action": "decline"})
        );

        // 正常作答：文本优先 custom，enum 命中合法值，多选收数组。
        let result = form_result(
            &fields,
            &answers(&[
                ("name", &[], Some("小明")),
                ("age", &[], Some("30")),
                ("weight", &[], Some("55.5")),
                ("color", &["green"], None),
                ("tags", &["a", "c"], None),
                ("newsletter", &["No"], None),
            ]),
        );
        assert_eq!(result["action"], json!("accept"));
        let content = &result["content"];
        assert_eq!(content["name"], json!("小明"));
        assert_eq!(content["age"], json!(30));
        assert_eq!(content["weight"], json!(55.5));
        assert_eq!(content["color"], json!("green"));
        assert_eq!(content["tags"], json!(["a", "c"]));
        assert_eq!(content["newsletter"], json!(false));
    }

    #[test]
    fn uncoercible_and_invalid_values_are_omitted() {
        let schema = form_schema();
        let fields = fields_of(&schema);
        let result = form_result(
            &fields,
            &answers(&[
                ("name", &[], Some("ok")),
                ("age", &[], Some("不是数字")),
                ("color", &["blue"], None),          // 不在 enum 内
                ("newsletter", &[], Some("可能吧")), // 无法识别
            ]),
        );
        assert_eq!(result["action"], json!("accept"));
        assert_eq!(result["content"], json!({"name": "ok"}));
    }

    #[test]
    fn selected_fallback_works_without_custom() {
        let schema = form_schema();
        let fields = fields_of(&schema);
        let result = form_result(&fields, &answers(&[("name", &["label-pick"], None)]));
        assert_eq!(result["content"]["name"], json!("label-pick"));
    }
}
