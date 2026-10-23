//! 插件工具参数的 JSON Schema 子集校验。
//!
//! 只实现声明里真正用得上的关键字：`type`、`required`、`properties`、
//! `additionalProperties`、`items`、`enum`。其余关键字按 JSON Schema 的注解语义忽略，
//! 不猜它们的行为，也不把不认识的关键字当成错误。

use serde_json::Value;

/// 校验 `value` 是否满足 `schema`；返回第一处不满足的说明。
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if let Some(types) = schema.get("type")
        && !type_matches(types, value)
    {
        return Err(format!(
            "类型应为 {}，实际是 {}",
            type_names(types),
            kind(value)
        ));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("取值必须是 {allowed:?} 之一，实际是 {value}"));
    }
    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, item) in values.iter().enumerate() {
            validate(items, item).map_err(|error| format!("第 {index} 项 {error}"))?;
        }
    }
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("缺少必填字段 `{name}`"));
            }
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
    for (name, item) in object {
        match properties.and_then(|properties| properties.get(name)) {
            Some(child) => validate(child, item).map_err(|error| format!("`{name}` {error}"))?,
            None if closed => return Err(format!("不允许字段 `{name}`")),
            None => {}
        }
    }
    Ok(())
}

fn type_matches(types: &Value, value: &Value) -> bool {
    match types {
        Value::String(name) => matches_type(name, value),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| matches_type(name, value)),
        _ => true,
    }
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some()
                || value.as_u64().is_some()
                || value.as_f64().is_some_and(|number| number.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        // 不认识的类型名不拦：声明侧不认识的语义交给插件自己判断。
        _ => true,
    }
}

fn type_names(types: &Value) -> String {
    match types {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" | "),
        _ => "未声明".to_owned(),
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "count": { "type": "integer", "minimum": 1 },
                "tags": { "type": "array", "items": { "type": "string" } },
                "mode": { "enum": ["fast", "slow"] }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    #[test]
    fn accepts_a_matching_object() {
        assert!(validate(&schema(), &json!({"name": "moc", "count": 2})).is_ok());
        assert!(validate(&schema(), &json!({"name": "moc", "tags": ["a"]})).is_ok());
    }

    #[test]
    fn rejects_missing_required_and_wrong_types() {
        assert!(
            validate(&schema(), &json!({}))
                .unwrap_err()
                .contains("name")
        );
        assert!(
            validate(&schema(), &json!({"name": 1}))
                .unwrap_err()
                .contains("string")
        );
        assert!(
            validate(&schema(), &json!({"name": "moc", "count": 1.5}))
                .unwrap_err()
                .contains("integer")
        );
        assert!(
            validate(&schema(), &json!({"name": "moc", "tags": [1]}))
                .unwrap_err()
                .contains("第 0 项")
        );
    }

    #[test]
    fn rejects_extra_fields_only_when_closed() {
        assert!(
            validate(&schema(), &json!({"name": "moc", "extra": true}))
                .unwrap_err()
                .contains("extra")
        );
        let open = json!({"type": "object", "properties": {"name": {"type": "string"}}});
        assert!(validate(&open, &json!({"name": "moc", "extra": true})).is_ok());
    }

    #[test]
    fn enforces_enum_and_type_unions() {
        assert!(validate(&schema(), &json!({"name": "moc", "mode": "fast"})).is_ok());
        let error = validate(&schema(), &json!({"name": "moc", "mode": "other"})).unwrap_err();
        assert!(error.contains("fast"), "{error}");
        let optional = json!({"type": ["string", "null"]});
        assert!(validate(&optional, &json!("x")).is_ok());
        assert!(validate(&optional, &Value::Null).is_ok());
        assert!(validate(&optional, &json!(1)).is_err());
    }

    #[test]
    fn ignores_unknown_keywords() {
        let schema = json!({"type": "object", "patternProperties": {"^x": {}}, "minimum": 3});
        assert!(validate(&schema, &json!({"anything": true})).is_ok());
    }
}
