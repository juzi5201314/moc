use super::short_bytes;
use crate::settings::render::ELLIPSIS;
use moc_protocol::ConfigField;
use moc_protocol::FieldKind;
use serde_json::Value;

/// 数字千分位格式化（200000 -> 200,000）。
pub(crate) fn format_number(value: u64) -> String {
    let s = value.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// 没有专属控件摘要的值（文本、数值、模型、对象）的展示形式。
pub(in super::super) fn display_value(value: &Value, field: &ConfigField) -> String {
    if value.is_null() {
        return "未设置".into();
    }
    if matches!(field.kind, FieldKind::Secret) {
        return if value.as_str().is_some_and(|text| !text.is_empty()) {
            "••••••"
        } else {
            "未设置"
        }
        .into();
    }
    // 字节预算按 KB/MB 显示，避免一串数字看不出量级。
    if let Some(bytes) = value.as_u64()
        && field.path.last().is_some_and(|key| key.ends_with("_bytes"))
    {
        return short_bytes(bytes);
    }
    // 上下文与回复上限等 Token 数显示千分位与缩写（200,000 (200k)）。
    if let Some(num) = value.as_u64()
        && field.path.last().is_some_and(|key| {
            matches!(
                key.as_str(),
                "context_window" | "max_output_tokens" | "max_tokens"
            )
        })
    {
        let compact = crate::render::format_tokens(num);
        let plain = format_number(num);
        // 不足 1000 时两者相同，没必要写成「512 (512)」。
        return if compact == plain {
            plain
        } else {
            format!("{plain} ({compact})")
        };
    }
    // 大于等于一万的整数加上千分位。
    if let Some(num) = value.as_u64()
        && num >= 10_000
    {
        return format_number(num);
    }
    match value {
        Value::Bool(true) => "● 开启".into(),
        Value::Bool(false) => "○ 关闭".into(),
        Value::String(text) if text.is_empty() => "未设置".into(),
        // 多行文本只露首行，并注明总行数，免得被当成值就这一行。
        Value::String(text) => match text.lines().count() {
            0 | 1 => text.trim_end().to_owned(),
            count => format!(
                "{}{ELLIPSIS}（共 {count} 行）",
                text.lines().next().unwrap_or_default()
            ),
        },
        Value::Object(values) if values.is_empty() => "无".into(),
        Value::Object(values) => format!("{{ {} 项 }}", values.len()),
        Value::Array(values) if values.is_empty() => "无".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("、"),
        // 整数值的浮点（20.0）与整数同样显示，免得同一页混排 `20.0` 与 `10`。
        Value::Number(number)
            if number.as_f64().is_some_and(|value| value.fract() == 0.0)
                && number.as_i64().is_none()
                && number.as_u64().is_none() =>
        {
            format!("{:.0}", number.as_f64().unwrap_or_default())
        }
        _ => value.to_string(),
    }
}
