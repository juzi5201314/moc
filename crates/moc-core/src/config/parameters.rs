//! 三种文本／图片／客户端函数 API 的可配置请求参数。
//!
//! 字段直接落在既有 extra_body / headers 中；目录、类型校验与请求默认值共用此处。
//! 不列会话生成的 model、messages/input、system/instructions、tools、stream，
//! 也不提供音频、后台任务、服务端工具和服务端会话管理等未实现流程的入口。

use moc_llm::{Protocol, ReasoningSummary, ThinkingWire};
use moc_protocol::{ChoiceOption, ConfigField, FieldKind};
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

struct Parameter(&'static str, &'static str, &'static str, Kind);

fn parameters(api: Protocol) -> Vec<Parameter> {
    use Kind::*;
    let mut fields = vec![
        Parameter(
            "temperature",
            "采样温度",
            "越低越稳定，越高越随机；仅支持此参数的模型可用，通常与 top_p 二选一。",
            Number(
                0.0,
                if api == Protocol::AnthropicMessages {
                    1.0
                } else {
                    2.0
                },
            ),
        ),
        Parameter(
            "top_p",
            "核采样比例",
            "仅从累计概率达到此比例的候选中采样；通常与 temperature 二选一。",
            Number(0.0, 1.0),
        ),
    ];
    if api != Protocol::AnthropicMessages {
        fields.extend([
            Parameter("store", "上游保存响应", "控制上游是否保存请求与响应；可能涉及数据留存。Chat 默认 false，Responses 默认 true。", Bool),
            Parameter("metadata", "请求元数据", "附加键值信息；最多 16 项，键不超过 64 字符，字符串值不超过 512 字符。不要填写敏感信息。", Object),
            Parameter("user", "旧版用户标识", "已弃用；优先使用 safety_identifier 和 prompt_cache_key，不要填写姓名或邮箱。", Text),
            Parameter("safety_identifier", "安全用户标识", "用于上游滥用检测的稳定标识，最长 128 字符；建议使用不可逆哈希。", Text),
            Parameter("service_tier", "服务档位", "选择处理速度与计费档位；需要模型和账号支持。会话 Fast Mode 开启时优先使用 fast。", Choice(&["auto", "default", "flex", "priority", "fast", "ultrafast"])),
            Parameter("parallel_tool_calls", "并行工具调用", "允许一次请求调用多个函数；协议兼容开关关闭时不发送此字段。", Bool),
            Parameter("prompt_cache_key", "提示缓存路由键", "覆盖 moc 默认的会话缓存键，让相同前缀使用同一缓存路由。", Text),
            Parameter("prompt_cache_options.mode", "缓存匹配模式", "implicit 自动选择缓存前缀；explicit 只使用显式断点，没有断点时不使用提示缓存。", Choice(&["implicit", "explicit"])),
            Parameter("prompt_cache_options.ttl", "缓存最短存活时间", "30m 为 30 分钟；Responses 还支持 12h（12 小时），具体需要上游支持。", Choice(if api == Protocol::OpenAiChat { &["30m"] } else { &["30m", "12h"] })),
            Parameter("prompt_cache_retention", "旧版缓存保留策略", "已弃用；in_memory 使用内存缓存，24h 延长保留时间，优先使用 prompt_cache_options.ttl。", Choice(&["in_memory", "24h"])),
            Parameter("stream_options.include_obfuscation", "流式传输混淆", "增加随机填充以减少侧信道信息；关闭可节省带宽，但会降低此保护。", Bool),
            Parameter("top_logprobs", "候选 token 概率数量", "返回每个位置最多 0–20 个候选概率；Chat 还需开启 logprobs。moc 不单独展示概率表。", Integer(0, 20)),
            Parameter("moderation.model", "内容审核模型", "审核输入与输出使用的模型，例如 omni-moderation-latest；需要上游支持。", Text),
            Parameter("moderation.policy.input.mode", "输入审核策略", "score 只评分，block 拦截违规输入。", Choice(&["score", "block"])),
            Parameter("moderation.policy.output.mode", "输出审核策略", "score 只评分，block 拦截违规输出。", Choice(&["score", "block"])),
            Parameter("tool_choice", "工具选择策略", "JSON：\"auto\" 自动选择、\"none\" 禁用、\"required\" 必须调用；指定函数时 Chat 使用 {\"type\":\"function\",\"function\":{\"name\":\"read\"}}，Responses 使用 {\"type\":\"function\",\"name\":\"read\"}。仅选择 moc 已提供的函数。", Json),
        ]);
    }
    match api {
        Protocol::OpenAiChat => fields.extend([
            Parameter("frequency_penalty", "重复频率惩罚", "范围 -2–2；正值减少重复词句，负值增加重复倾向。", Number(-2.0, 2.0)),
            Parameter("presence_penalty", "重复主题惩罚", "范围 -2–2；正值鼓励新话题，负值更倾向已有话题。", Number(-2.0, 2.0)),
            Parameter("logit_bias", "token 概率偏置", "JSON 对象：token ID 映射到 -100–100 的偏置；例如 {\"123\":-10}。", Object),
            Parameter("logprobs", "返回 token 概率", "请求输出 token 的对数概率；moc 不单独展示概率表。", Bool),
            Parameter("n", "生成候选数量", "范围 1–128；moc 只消费第一个候选，其余候选仍会产生费用，通常保持 1。", Integer(1, 128)),
            Parameter("seed", "采样随机种子", "有符号整数；相同种子仅尽力保持一致，并不保证确定性。Web 编辑器仅接受精确可表示的整数。", Integer(i64::MIN, i64::MAX)),
            Parameter("reasoning_effort", "默认思考强度", "none 关闭，或填写 minimal、low、medium、high、xhigh、max／自定义等级；会话显式选择优先，需包含在模型的可选思考等级中。", Text),
            Parameter("verbosity", "回复详细程度", "low 更简洁，medium 适中，high 更详细；仅支持此参数的模型可用。", Choice(&["low", "medium", "high"])),
            Parameter("stop", "停止序列", "JSON 字符串或字符串数组，最多 4 项；保留逗号和换行，命中后停止生成。部分推理模型不支持。", Json),
            Parameter("stream_options.include_usage", "流式用量统计", "moc 默认 true；关闭后可能缺少准确的 token 用量和费用统计。", Bool),
            Parameter("prediction.type", "预期输出类型", "预期输出内容的类型，固定为 content；仅支持 Predicted Outputs 的模型可用。", Choice(&["content"])),
            Parameter("prediction.content", "预期输出内容", "JSON 字符串或文本内容块数组，用于大部分输出已知的场景；会发送给上游。", Json),
            Parameter("response_format.type", "输出格式", "text 普通文本、json_object JSON 对象、json_schema 按指定结构输出；JSON 模式需要提示词说明。", Choice(&["text", "json_object", "json_schema"])),
            Parameter("response_format.json_schema.name", "输出结构名称", "JSON Schema 的名称，最长 64 字符，仅使用字母、数字、下划线和连字符。", Text),
            Parameter("response_format.json_schema.description", "输出结构说明", "解释此结构的用途，帮助模型理解期望输出。", Text),
            Parameter("response_format.json_schema.schema", "输出 JSON Schema", "JSON 对象，定义期望的输出结构；例如 {\"type\":\"object\",\"properties\":{}}。", Object),
            Parameter("response_format.json_schema.strict", "严格遵循输出结构", "开启后严格遵循 schema；只支持上游允许的 JSON Schema 子集。", Bool),
        ]),
        Protocol::OpenAiResponses => fields.extend([
            Parameter("reasoning.summary", "思考摘要详细程度", "auto 由模型决定，concise 简洁，detailed 详细；moc 默认 auto，关闭思考时不发送摘要请求。", Choice(&["auto", "concise", "detailed"])),
            Parameter("reasoning.generate_summary", "旧版思考摘要", "已弃用，优先使用 reasoning.summary；未配置 summary 时使用此值。", Choice(&["auto", "concise", "detailed"])),
            Parameter("reasoning.context", "历史思考范围", "auto 由模型决定，current_turn 仅当前轮，all_turns 包含全部轮次。", Choice(&["auto", "current_turn", "all_turns"])),
            Parameter("reasoning.mode", "思考执行模式", "例如 standard 或 pro；原样发送，是否支持由模型与上游决定。", Text),
            Parameter("reasoning.effort", "默认思考强度", "none 关闭，或填写 minimal、low、medium、high、xhigh、max／自定义等级；会话显式选择优先，需包含在模型的可选思考等级中。", Text),
            Parameter("text.verbosity", "回复详细程度", "low 更简洁，medium 适中，high 更详细；与 reasoning.summary 的思考摘要粒度无关。", Choice(&["low", "medium", "high"])),
            Parameter("text.format.type", "输出格式", "text 普通文本、json_object JSON 对象、json_schema 按指定结构输出；JSON 模式需要提示词说明。", Choice(&["text", "json_object", "json_schema"])),
            Parameter("text.format.name", "输出结构名称", "JSON Schema 的名称，最长 64 字符，仅使用字母、数字、下划线和连字符。", Text),
            Parameter("text.format.description", "输出结构说明", "解释此结构的用途，帮助模型理解期望输出。", Text),
            Parameter("text.format.schema", "输出 JSON Schema", "JSON 对象，定义期望的输出结构。", Object),
            Parameter("text.format.strict", "严格遵循输出结构", "开启后严格遵循 schema；只支持上游允许的 JSON Schema 子集。", Bool),
            Parameter("include", "附加返回数据", "字符串列表，例如 message.output_text.logprobs、message.input_image.image_url；moc 会按思考回传策略追加 reasoning.encrypted_content。", Strings),
            Parameter("truncation", "上游超长输入处理", "disabled 超长时报错，auto 允许上游丢弃开头的历史；auto 可能丢失早期指令。", Choice(&["disabled", "auto"])),
            Parameter("prompt.id", "上游提示模板 ID", "使用已经在上游创建的提示模板；模板可能改变请求指令。", Text),
            Parameter("prompt.version", "提示模板版本", "指定已创建模板的版本；留空使用上游默认版本。", Text),
            Parameter("prompt.variables", "提示模板变量", "JSON 对象：变量名映射到字符串或上游支持的文本／图片输入对象。", Object),
            Parameter("prompt_cache_options.comparison_response_id", "缓存对比响应 ID", "填写已有响应 ID，请求上游诊断与该响应的缓存差异；moc 不自动维护此 ID。", Text),
            Parameter("image.context_compression", "上游图片上下文压缩", "auto 自动决定、enabled 要求支持压缩、disabled 关闭；不改变 moc 本地图片预算。", Choice(&["auto", "enabled", "disabled"])),
            Parameter("access_programs.cyber", "Cyber 访问计划", "standard、daybreak_blue 或 daybreak_red；需账号与模型授权，否则上游会拒绝。", Choice(&["standard", "daybreak_blue", "daybreak_red"])),
        ]),
        Protocol::AnthropicMessages => fields.extend([
            Parameter("top_k", "采样候选数量", "仅从概率最高的 K 个候选中采样；只对支持此参数的模型发送。", Integer(1, i64::MAX)),
            Parameter("stop_sequences", "停止序列", "字符串列表；JSON 数组可精确保留逗号和换行，命中后停止生成。", Json),
            Parameter("thinking.type", "思考模式", "adaptive 自适应、enabled 使用预算、disabled 关闭、between_tools 在工具间思考；会话 off 优先，取值需模型支持。", Choice(&["adaptive", "enabled", "disabled", "between_tools"])),
            Parameter("thinking.budget_tokens", "思考 token 预算", "enabled 模式下至少 1024，且小于回复 token 上限；预算计入 max_tokens。", Integer(1024, i64::MAX)),
            Parameter("thinking.display", "思考返回方式", "summarized 返回思考摘要，omitted 隐藏内容但保留签名；moc 默认 summarized。", Choice(&["summarized", "omitted"])),
            Parameter("output_config.effort", "默认思考强度", "low、medium、high、xhigh、max／自定义等级；会话显式选择优先。enabled + budget_tokens 模式使用此值，不自动附加会话强度。", Text),
            Parameter("output_config.format.type", "输出格式", "结构化输出类型，固定为 json_schema。", Choice(&["json_schema"])),
            Parameter("output_config.format.schema", "输出 JSON Schema", "JSON 对象，定义期望的输出结构。", Object),
            Parameter("metadata.user_id", "外部用户标识", "使用 UUID、哈希或其他不透明标识；不要发送姓名、邮箱、电话号码等个人信息。", Text),
            Parameter("service_tier", "服务容量档位", "auto 可使用优先容量，standard_only 只使用标准容量；需账号支持。", Choice(&["auto", "standard_only"])),
            Parameter("speed", "推理速度模式", "standard 标准、fast 高速；部分模型需要 anthropic-beta 中的 fast-mode-2026-02-01。", Choice(&["standard", "fast"])),
            Parameter("inference_geo", "推理处理区域", "指定上游推理区域；留空使用工作区默认区域，需要账号支持。", Text),
            Parameter("cache_control.type", "自动缓存类型", "固定为 ephemeral，让上游在最后一个可缓存块设置断点；与 moc 显式缓存断点可同时存在。", Choice(&["ephemeral"])),
            Parameter("cache_control.ttl", "自动缓存存活时间", "5m 为 5 分钟，1h 为 1 小时；长缓存可能改变费用。", Choice(&["5m", "1h"])),
            Parameter("diagnostics.previous_message_id", "缓存对比消息 ID", "填写已有 msg_… ID，诊断与该消息的缓存差异；moc 不自动维护此 ID。", Text),
            Parameter("tool_choice.type", "工具选择策略", "auto 自动、none 禁用、any 必须调用、tool 指定函数；仅选择 moc 已提供的函数。", Choice(&["auto", "none", "any", "tool"])),
            Parameter("tool_choice.name", "指定函数名称", "tool_choice.type 为 tool 时填写，例如 read；函数必须在本轮可用工具中。", Text),
            Parameter("tool_choice.disable_parallel_tool_use", "禁用并行工具调用", "true 每次最多调用一个工具，false 允许并行；并行兼容开关关闭时不发送此字段。", Bool),
        ]),
    }
    fields
}

fn headers(api: Protocol) -> Vec<Parameter> {
    use Kind::Text;
    match api {
        Protocol::AnthropicMessages => vec![
            Parameter(
                "anthropic-version",
                "Anthropic API 版本",
                "使用的 API 版本，例如 2023-06-01；留空使用 moc 的协议版本。",
                Text,
            ),
            Parameter(
                "anthropic-beta",
                "Anthropic 实验功能",
                "逗号分隔的 beta 功能名，例如 fast-mode-2026-02-01；需模型和账号支持。",
                Text,
            ),
            Parameter(
                "anthropic-workspace-id",
                "Anthropic 工作区 ID",
                "多工作区凭据使用的工作区，例如 wrkspc_…；必须与凭据权限匹配。",
                Text,
            ),
            Parameter(
                "anthropic-user-profile-id",
                "Anthropic 用户档案 ID",
                "代其他主体请求时用于归属标识；不要填写不必要的个人信息。",
                Text,
            ),
        ],
        _ => vec![
            Parameter(
                "OpenAI-Organization",
                "OpenAI 组织 ID",
                "为多组织凭据选择组织；必须与密钥权限匹配。",
                Text,
            ),
            Parameter(
                "OpenAI-Project",
                "OpenAI 项目 ID",
                "为凭据选择上游项目；必须与密钥权限匹配。",
                Text,
            ),
        ],
    }
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
        append(&["providers", id, "extra_body"], &params, "供应商", output);
        append(
            &["providers", id, "headers"],
            &headers(api),
            "供应商",
            output,
        );
        if let Some(models) = provider.get("models").and_then(Value::as_object) {
            for model in models.keys() {
                append(
                    &["providers", id, "models", model, "compat", "extra_body"],
                    &params,
                    "模型",
                    output,
                );
                append(
                    &["providers", id, "models", model, "compat", "headers"],
                    &headers(api),
                    "模型",
                    output,
                );
            }
        }
    }
}

fn append(prefix: &[&str], params: &[Parameter], tab: &str, output: &mut Vec<ConfigField>) {
    for Parameter(name, label, description, kind) in params {
        let path = prefix
            .iter()
            .copied()
            .chain(name.split('.'))
            .map(str::to_owned)
            .collect();
        output.push(ConfigField {
            path,
            tab: tab.into(),
            label: (*label).into(),
            description: format!("`{name}`：{description}"),
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
    for Parameter(name, _, description, kind) in parameters(api) {
        if let Some(value) = get(body, name)
            && !value.is_null()
            && !kind.accepts(value)
        {
            return Err(CoreError::Config(format!(
                "{label}.{name} 的值或类型不正确：{description}"
            )));
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
        return Err(CoreError::Config(format!(
            "{label}.{effort} 需要小写思考等级名"
        )));
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
            return Err(CoreError::Config(format!(
                "{label}.{name} 需要字符串数组；stop 也允许单个字符串，最多 4 项"
            )));
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
    use super::*;
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
            assert!(field.description.contains(&format!("`{name}`")));
            assert!(field.advanced, "请求参数默认收纳到高级选项");
        }
        assert!(
            !fields
                .iter()
                .any(|field| field.path
                    == ["providers", "chat", "extra_body", "reasoning", "summary"])
        );
        for api in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            let params = parameters(api);
            for Parameter(name, label, description, _) in &params {
                assert!(!label.is_empty() && !description.is_empty());
                assert_eq!(params.iter().filter(|param| param.0 == *name).count(), 1);
            }
        }
    }

    #[test]
    fn invalid_known_parameters_are_rejected_without_restricting_vendor_extensions() {
        for body in [
            json!({"frequency_penalty":-3}),
            json!({"seed":1.5}),
            json!({"stop":["a","b","c","d","e"]}),
        ] {
            assert!(
                validate(
                    Protocol::OpenAiChat,
                    body.as_object().expect("对象"),
                    "p.extra_body"
                )
                .is_err()
            );
        }
        assert!(
            validate(
                Protocol::OpenAiResponses,
                json!({"reasoning":{"summary":"invalid"}})
                    .as_object()
                    .expect("对象"),
                "p.extra_body"
            )
            .is_err()
        );
        assert!(
            validate(
                Protocol::OpenAiChat,
                json!({"frequency_penalty":-0.5,"seed":-42,"vendor_option":true})
                    .as_object()
                    .expect("对象"),
                "p.extra_body"
            )
            .is_ok()
        );
    }
}
