use serde_json::Value;

/// Chat 正文允许字符串或内容块；场景断言只依赖工具语义，不固定缓存编码形态。
pub fn content_text(value: &Value) -> std::borrow::Cow<'_, str> {
    match value {
        Value::String(text) => std::borrow::Cow::Borrowed(text),
        Value::Array(parts) => std::borrow::Cow::Owned(
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
        ),
        _ => panic!("工具正文不是标准 Chat 文本：{value}"),
    }
}
