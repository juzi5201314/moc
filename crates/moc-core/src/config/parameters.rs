//! 三种文本／图片／客户端函数 API 的可配置请求参数。
//!
//! 字段直接落在既有 extra_body / headers 中；目录、类型校验与请求默认值共用此处。
//! 不列会话生成的 model、messages/input、system/instructions、tools、stream，
//! 也不提供音频、后台任务、服务端工具和服务端会话管理等未实现流程的入口。
//! 参数文案在 locales/core.toml 的 `core.params.<name_下划线>` 下，缺键退回参数名。

use std::borrow::Cow;

use moc_llm::{Protocol, ReasoningSummary, ThinkingWire};
use moc_protocol::{ChoiceOption, ConfigField, FieldKind};
use moc_i18n::t;
use serde_json::{Map, Value};

use super::model::ThinkingLevel;
use crate::CoreError;

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Bool,
    Integer(i64, i64),
    Number(f64, f64),
    Choice(&'static [&'static str]),
    Object,
    Json,
    Strings,
}

struct Parameter(&'static str, Kind);

fn parameters(api: Protocol) -> Vec<Parameter> {
    use Kind::*;
    let mut fields = vec![
        Parameter(
            "temperature",
            Number(
                0.0,
                if api == Protocol::AnthropicMessages {
                    1.0
                } else {
                    2.0
                },
            ),
        ),
        Parameter("top_p", Number(0.0, 1.0)),
    ];
    if api != Protocol::AnthropicMessages {
        fields.extend([
            Parameter("store", Bool),
            Parameter("metadata", Object),
            Parameter("user", Text),
            Parameter("safety_identifier", Text),
            Parameter(
                "service_tier",
                Choice(&["auto", "default", "flex", "priority", "fast", "ultrafast"]),
            ),
            Parameter("parallel_tool_calls", Bool),
            Parameter("prompt_cache_key", Text),
            Parameter(
                "prompt_cache_options.mode",
                Choice(&["implicit", "explicit"]),
            ),
            Parameter(
                "prompt_cache_options.ttl",
                Choice(if api == Protocol::OpenAiChat {
                    &["30m"]
                } else {
                    &["30m", "12h"]
                }),
            ),
            Parameter("prompt_cache_retention", Choice(&["in_memory", "24h"])),
            Parameter("stream_options.include_obfuscation", Bool),
            Parameter("top_logprobs", Integer(0, 20)),
            Parameter("moderation.model", Text),
            Parameter("moderation.policy.input.mode", Choice(&["score", "block"])),
            Parameter("moderation.policy.output.mode", Choice(&["score", "block"])),
            Parameter("tool_choice", Json),
        ]);
    }
    match api {
        Protocol::OpenAiChat => fields.extend([
            Parameter("frequency_penalty", Number(-2.0, 2.0)),
            Parameter("presence_penalty", Number(-2.0, 2.0)),
            Parameter("logit_bias", Object),
            Parameter("logprobs", Bool),
            Parameter("n", Integer(1, 128)),
            Parameter("seed", Integer(i64::MIN, i64::MAX)),
            Parameter("reasoning_effort", Text),
            Parameter("verbosity", Choice(&["low", "medium", "high"])),
            Parameter("stop", Json),
            Parameter("stream_options.include_usage", Bool),
            Parameter("prediction.type", Choice(&["content"])),
            Parameter("prediction.content", Json),
            Parameter(
                "response_format.type",
                Choice(&["text", "json_object", "json_schema"]),
            ),
            Parameter("response_format.json_schema.name", Text),
            Parameter("response_format.json_schema.description", Text),
            Parameter("response_format.json_schema.schema", Object),
            Parameter("response_format.json_schema.strict", Bool),
        ]),
        Protocol::OpenAiResponses => fields.extend([
            Parameter(
                "reasoning.summary",
                Choice(&["auto", "concise", "detailed"]),
            ),
            Parameter(
                "reasoning.generate_summary",
                Choice(&["auto", "concise", "detailed"]),
            ),
            Parameter(
                "reasoning.context",
                Choice(&["auto", "current_turn", "all_turns"]),
            ),
            Parameter("reasoning.mode", Text),
            Parameter("reasoning.effort", Text),
            Parameter("text.verbosity", Choice(&["low", "medium", "high"])),
            Parameter(
                "text.format.type",
                Choice(&["text", "json_object", "json_schema"]),
            ),
            Parameter("text.format.name", Text),
            Parameter("text.format.description", Text),
            Parameter("text.format.schema", Object),
            Parameter("text.format.strict", Bool),
            Parameter("include", Strings),
            Parameter("truncation", Choice(&["disabled", "auto"])),
            Parameter("prompt.id", Text),
            Parameter("prompt.version", Text),
            Parameter("prompt.variables", Object),
            Parameter("prompt_cache_options.comparison_response_id", Text),
            Parameter(
                "image.context_compression",
                Choice(&["auto", "enabled", "disabled"]),
            ),
            Parameter(
                "access_programs.cyber",
                Choice(&["standard", "daybreak_blue", "daybreak_red"]),
            ),
        ]),
        Protocol::AnthropicMessages => fields.extend([
            Parameter("top_k", Integer(1, i64::MAX)),
            Parameter("stop_sequences", Json),
            Parameter(
                "thinking.type",
                Choice(&["adaptive", "enabled", "disabled", "between_tools"]),
            ),
            Parameter("thinking.budget_tokens", Integer(1024, i64::MAX)),
            Parameter("thinking.display", Choice(&["summarized", "omitted"])),
            Parameter("output_config.effort", Text),
            Parameter("output_config.format.type", Choice(&["json_schema"])),
            Parameter("output_config.format.schema", Object),
            Parameter("metadata.user_id", Text),
            Parameter("service_tier", Choice(&["auto", "standard_only"])),
            Parameter("speed", Choice(&["standard", "fast"])),
            Parameter("inference_geo", Text),
            Parameter("cache_control.type", Choice(&["ephemeral"])),
            Parameter("cache_control.ttl", Choice(&["5m", "1h"])),
            Parameter("diagnostics.previous_message_id", Text),
            Parameter("tool_choice.type", Choice(&["auto", "none", "any", "tool"])),
            Parameter("tool_choice.name", Text),
            Parameter("tool_choice.disable_parallel_tool_use", Bool),
        ]),
    }
    fields
}

fn headers(api: Protocol) -> Vec<Parameter> {
    use Kind::Text;
    match api {
        Protocol::AnthropicMessages => vec![
            Parameter("anthropic-version", Text),
            Parameter("anthropic-beta", Text),
            Parameter("anthropic-workspace-id", Text),
            Parameter("anthropic-user-profile-id", Text),
        ],
        _ => vec![
            Parameter("OpenAI-Organization", Text),
            Parameter("OpenAI-Project", Text),
        ],
    }
}

/// 参数的 label/desc：label 直接使用精确 wire 键名；desc 先查 API 专属键
/// `core.params.<api>.<stem>.desc`，再查通用键 `core.params.<name_下划线>.desc`。
fn describe(api: Protocol, name: &'static str) -> (Cow<'static, str>, Cow<'static, str>) {
    let stem = name.replace('.', "_");
    let api_stem = match api {
        Protocol::OpenAiChat => "openai_chat",
        Protocol::OpenAiResponses => "openai_responses",
        Protocol::AnthropicMessages => "anthropic",
    };
    let desc = [
        format!("core.params.{api_stem}.{stem}.desc"),
        format!("core.params.{stem}.desc"),
    ]
    .into_iter()
    .find_map(|key| crate::i18n::try_t(&key))
    .unwrap_or_else(|| t!("core.config.fallback_desc"));
    (Cow::Borrowed(name), desc)
}

impl Kind {
    fn field(self) -> FieldKind {
        match self {
            Self::Text => FieldKind::Text,
            Self::Bool => FieldKind::OptionalBool,
            Self::Integer(min, _) if min < 0 => FieldKind::SignedInteger,
            Self::Integer(..) => FieldKind::Integer,
            Self::Number(min, _) if min < 0.0 => FieldKind::SignedNumber,
            Self::Number(..) => FieldKind::Number,
            Self::Choice(values) => FieldKind::Choice(
                values
                    .iter()
                    .map(|value| ChoiceOption::new(value, value))
                    .collect(),
            ),
            Self::Object => FieldKind::Object,
            Self::Json => FieldKind::Json,
            Self::Strings => FieldKind::Strings,
        }
    }

    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Text => value.is_string(),
            Self::Bool => value.is_boolean(),
            Self::Integer(min, max) => value.as_i64().is_some_and(|n| n >= min && n <= max),
            Self::Number(min, max) => value
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= min && n <= max),
            Self::Choice(values) => value.as_str().is_some_and(|s| values.contains(&s)),
            Self::Object => value.is_object(),
            Self::Json => !value.is_null(),
            Self::Strings => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
        }
    }
}

pub(super) fn protocol(value: &Value) -> Option<Protocol> {
    serde_json::from_value(value.clone()).ok()
}

/// 不补值到文档：未配置的字段仍有独立入口，清空后回到供应商／上层默认。
pub(super) fn append_fields(effective: &Value, output: &mut Vec<ConfigField>) {
    let Some(providers) = effective.get("providers").and_then(Value::as_object) else {
        return;
    };
    for (id, provider) in providers {
        let Some(api) = provider.get("api").and_then(protocol) else {
            continue;
        };
        let params = parameters(api);
        append(
            &["providers", id, "extra_body"],
            api,
            &params,
            t!("core.config.tabs.providers"),
            output,
        );
        append(
            &["providers", id, "headers"],
            api,
            &headers(api),
            t!("core.config.tabs.providers"),
            output,
        );
        if let Some(models) = provider.get("models").and_then(Value::as_object) {
            for (model, spec) in models {
                if spec.get("image_gen").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                append(
                    &["providers", id, "models", model, "compat", "extra_body"],
                    api,
                    &params,
                    t!("core.config.tabs.models"),
                    output,
                );
                append(
                    &["providers", id, "models", model, "compat", "headers"],
                    api,
                    &headers(api),
                    t!("core.config.tabs.models"),
                    output,
                );
            }
        }
    }
}

fn append(
    prefix: &[&str],
    api: Protocol,
    params: &[Parameter],
    tab: Cow<str>,
    output: &mut Vec<ConfigField>,
) {
    for Parameter(name, kind) in params {
        let path = prefix
            .iter()
            .copied()
            .chain(name.split('.'))
            .map(str::to_owned)
            .collect();
        let (label, desc) = describe(api, name);
        output.push(ConfigField {
            path,
            tab: tab.to_string(),
            label: label.into(),
            description: desc.into_owned(),
            kind: kind.field(),
            advanced: true,
        });
    }
}

pub(crate) fn get<'a>(body: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let mut value = body.get(segments.next()?)?;
    for segment in segments {
        value = value.get(segment)?;
    }
    Some(value)
}

pub(super) fn validate(
    api: Protocol,
    body: &Map<String, Value>,
    label: &str,
) -> Result<(), CoreError> {
    for Parameter(name, kind) in parameters(api) {
        if let Some(value) = get(body, name)
            && !value.is_null()
            && !kind.accepts(value)
        {
            return Err(CoreError::Config(
                t!(
                    "core.params.invalid_value",
                    label = label,
                    name = name,
                    desc = describe(api, name).1.as_ref()
                )
                .into(),
            ));
        }
    }
    let effort = match api {
        Protocol::OpenAiChat => "reasoning_effort",
        Protocol::OpenAiResponses => "reasoning.effort",
        Protocol::AnthropicMessages => "output_config.effort",
    };
    if let Some(value) = get(body, effort).and_then(Value::as_str)
        && value != "none"
        && value.parse::<ThinkingLevel>().is_err()
    {
        return Err(CoreError::Config(
            t!("core.params.invalid_effort", label = label, effort = effort).into(),
        ));
    }
    // 停止序列是字符串／数组联合类型，不能按逗号拆分丢失内容。
    let stop = match api {
        Protocol::OpenAiChat => Some("stop"),
        Protocol::AnthropicMessages => Some("stop_sequences"),
        Protocol::OpenAiResponses => None,
    };
    if let Some(name) = stop
        && let Some(value) = get(body, name)
    {
        let valid = (name == "stop" && value.is_string())
            || value.as_array().is_some_and(|items| {
                items.iter().all(Value::is_string) && (name != "stop" || items.len() <= 4)
            });
        if !valid {
            return Err(CoreError::Config(
                t!("core.params.invalid_stop", label = label, name = name).into(),
            ));
        }
    }
    Ok(())
}

/// 模型覆盖逐层合并对象、整体替换数组，保留供应商中未覆盖的同级字段。
pub(super) fn merge_body(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = Value::Object(provider.clone());
    super::editor::merge_json(&mut merged, Value::Object(model.clone()));
    match merged {
        Value::Object(object) => object,
        _ => unreachable!("合并两个对象仍为对象"),
    }
}

pub(crate) fn summary(api: Protocol, body: &Map<String, Value>) -> ReasoningSummary {
    let name = match api {
        Protocol::AnthropicMessages => "thinking.display",
        _ => "reasoning.summary",
    };
    get(body, name)
        .or_else(|| {
            (api == Protocol::OpenAiResponses)
                .then(|| get(body, "reasoning.generate_summary"))
                .flatten()
        })
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .unwrap_or(ReasoningSummary::Auto)
}

pub(super) fn default_effort(
    api: Protocol,
    body: &Map<String, Value>,
    wire: &ThinkingWire,
) -> Option<ThinkingLevel> {
    if api == Protocol::AnthropicMessages
        && get(body, "thinking.type").and_then(Value::as_str) == Some("disabled")
    {
        return Some(ThinkingLevel::Off);
    }
    let field = match api {
        Protocol::OpenAiChat => wire.effort_field.as_deref()?,
        Protocol::OpenAiResponses => "reasoning.effort",
        Protocol::AnthropicMessages => "output_config.effort",
    };
    get(body, field).and_then(Value::as_str).and_then(|value| {
        if value == "none" {
            Some(ThinkingLevel::Off)
        } else {
            value.parse().ok()
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn field_catalog_covers_each_api_and_preserves_wire_names() {
        let mut effective = json!({"providers":{
            "chat":{"api":"openai_chat","models":{"m":{}}},
            "responses":{"api":"openai_responses","models":{"m":{}}},
            "anthropic":{"api":"anthropic_messages","models":{"m":{}}}
        }});
        super::super::catalog::complete_view(&mut effective);
        let fields = super::super::catalog::fields(&effective);
        for (id, name) in [
            ("chat", "frequency_penalty"),
            ("responses", "reasoning.summary"),
            ("anthropic", "thinking.display"),
        ] {
            let path: Vec<_> = ["providers", id, "models", "m", "compat", "extra_body"]
                .into_iter()
                .chain(name.split('.'))
                .map(str::to_owned)
                .collect();
            let field = fields
                .iter()
                .find(|field| field.path == path)
                .expect("每个模型都有参数入口");
            assert_eq!(field.label, name, "API 请求参数 label 应保持精确 wire 键名");
            assert!(
                !field.description.is_empty() && !field.description.starts_with(&format!("`{name}`")),
                "API 请求参数描述应直接提供说明，不再重复键名前缀"
            );
            assert!(field.advanced, "请求参数默认收纳到高级选项");
        }
        assert!(
            !fields
                .iter()
                .any(|field| field.path
                    == ["providers", "chat", "extra_body", "reasoning", "summary"])
        );
    }
}
