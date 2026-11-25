//! 常见供应商预设与已知模型能力推断。
//!
//! 预设仅作为新建与拉取模型时的默认辅助值，用户可随时自由覆写。

use moc_protocol::CatalogModel;
use serde_json::{Map, Value, json};

/// 常见上游服务商的快速新建模板。
#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub key: &'static str,
    pub label: &'static str,
    pub id: &'static str,
    pub api: &'static str,
    pub base_url: &'static str,
    pub api_key_hint: &'static str,
    pub azure_api_version: Option<&'static str>,
}

/// 内置供应商预设顺序：Custom 置首便于完全自定义，常用服务紧随其后。
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        key: "custom",
        label: "Custom",
        id: "",
        api: "openai_chat",
        base_url: "",
        api_key_hint: "",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "openai",
        label: "OpenAI",
        id: "openai",
        api: "openai_responses",
        base_url: "https://api.openai.com/v1",
        api_key_hint: "env:OPENAI_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "anthropic",
        label: "Anthropic",
        id: "anthropic",
        api: "anthropic_messages",
        base_url: "https://api.anthropic.com",
        api_key_hint: "env:ANTHROPIC_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "deepseek",
        label: "DeepSeek",
        id: "deepseek",
        api: "openai_chat",
        base_url: "https://api.deepseek.com/v1",
        api_key_hint: "env:DEEPSEEK_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "openrouter",
        label: "OpenRouter",
        id: "openrouter",
        api: "openai_chat",
        base_url: "https://openrouter.ai/api/v1",
        api_key_hint: "env:OPENROUTER_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "gemini",
        label: "Google Gemini",
        id: "gemini",
        api: "openai_chat",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        api_key_hint: "env:GEMINI_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "xai",
        label: "xAI (Grok)",
        id: "xai",
        api: "openai_responses",
        base_url: "https://api.x.ai/v1",
        api_key_hint: "env:XAI_API_KEY",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "ollama",
        label: "Ollama (Local)",
        id: "ollama",
        api: "openai_chat",
        base_url: "http://127.0.0.1:11434/v1",
        api_key_hint: "",
        azure_api_version: None,
    },
    ProviderPreset {
        key: "azure",
        label: "Azure OpenAI",
        id: "azure",
        api: "openai_responses",
        base_url: "https://YOUR-RESOURCE.openai.azure.com",
        api_key_hint: "env:AZURE_OPENAI_API_KEY",
        azure_api_version: Some("2025-04-01-preview"),
    },
];

/// 按模型 ID 推断的常见默认能力。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelHint {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<String>>,
    pub image_gen: Option<bool>,
}

fn text_only() -> Vec<String> {
    vec!["text".to_owned()]
}

fn text_and_image() -> Vec<String> {
    vec!["text".to_owned(), "image".to_owned()]
}

/// 剥离网关前缀（如 `openai/gpt-5.4` → `gpt-5.4`）与常见版本后缀以便归类。
fn bare_model_name(model_id: &str) -> String {
    let trimmed = model_id.trim().to_ascii_lowercase();
    let tail = trimmed.rsplit('/').next().unwrap_or(&trimmed);
    tail.split(':').next().unwrap_or(tail).to_owned()
}

/// 根据常见模型家族推断默认上下文、输出上限、推理与多模态能力；未知模型返回 `None`。
pub fn infer_model_hint(model_id: &str) -> Option<ModelHint> {
    let name = bare_model_name(model_id);
    if name.is_empty() {
        return None;
    }
    if name.starts_with("gpt-image-")
        || name.starts_with("dall-e-")
        || name.starts_with("imagen-")
        || name.starts_with("flux-")
        || name.contains("image-generation")
    {
        return Some(ModelHint {
            image_gen: Some(true),
            ..Default::default()
        });
    }
    if name.starts_with("gpt-5") {
        return Some(ModelHint {
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
            reasoning: Some(true),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("gpt-4.1") {
        return Some(ModelHint {
            context_window: Some(1_000_000),
            max_output_tokens: Some(32_768),
            reasoning: Some(false),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("gpt-4o") {
        return Some(ModelHint {
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            reasoning: Some(false),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name == "o1"
        || name == "o3"
        || name == "o4"
        || name.starts_with("o1-")
        || name.starts_with("o3-")
        || name.starts_with("o4-")
    {
        return Some(ModelHint {
            context_window: Some(200_000),
            max_output_tokens: Some(100_000),
            reasoning: Some(true),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("claude-") {
        let is_legacy = name.contains("claude-3-5") || name.contains("claude-3.5");
        let is_haiku = name.contains("haiku");
        return Some(ModelHint {
            context_window: Some(200_000),
            max_output_tokens: Some(if is_legacy {
                8_192
            } else if is_haiku {
                32_000
            } else {
                64_000
            }),
            reasoning: Some(!is_legacy),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("deepseek-reasoner") || name.starts_with("deepseek-r1") {
        return Some(ModelHint {
            context_window: Some(128_000),
            max_output_tokens: Some(64_000),
            reasoning: Some(true),
            input: Some(text_only()),
            image_gen: None,
        });
    }
    if name.starts_with("deepseek-chat") || name.starts_with("deepseek-v3") {
        return Some(ModelHint {
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            reasoning: Some(false),
            input: Some(text_only()),
            image_gen: None,
        });
    }
    if name.starts_with("gemini-2.5") || name.starts_with("gemini-3") {
        return Some(ModelHint {
            context_window: Some(1_048_576),
            max_output_tokens: Some(65_536),
            reasoning: Some(true),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("gemini-2.0") || name.starts_with("gemini-1.5") {
        return Some(ModelHint {
            context_window: Some(1_048_576),
            max_output_tokens: Some(8_192),
            reasoning: Some(false),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("grok-4") || name.starts_with("grok-3") {
        let reasoning = name.starts_with("grok-4") || name.contains("mini");
        return Some(ModelHint {
            context_window: Some(if name.starts_with("grok-4") {
                256_000
            } else {
                131_072
            }),
            max_output_tokens: Some(32_768),
            reasoning: Some(reasoning),
            input: Some(text_and_image()),
            image_gen: None,
        });
    }
    if name.starts_with("qwen3") || name.starts_with("qwq") {
        let coder = name.contains("coder");
        let vision = name.contains("vl") || name.contains("omni");
        return Some(ModelHint {
            context_window: Some(if coder { 262_144 } else { 131_072 }),
            max_output_tokens: Some(32_768),
            reasoning: Some(!coder || name.contains("thinking")),
            input: Some(if vision {
                text_and_image()
            } else {
                text_only()
            }),
            image_gen: None,
        });
    }
    None
}

/// 将推断/发现到的模型元数据转为写入配置表的精简对象（只写有别于默认模板的字段）。
pub fn model_entry_from_catalog(model: &CatalogModel) -> Value {
    let mut entry = Map::new();
    if model.image_gen == Some(true) {
        entry.insert("image_gen".into(), json!(true));
        return Value::Object(entry);
    }
    if let Some(ctx) = model.context_window
        && ctx > 0
        && ctx != 200_000
    {
        entry.insert("context_window".into(), json!(ctx));
    }
    if let Some(out) = model.max_output_tokens
        && out > 0
        && out != 32_000
    {
        entry.insert("max_output_tokens".into(), json!(out));
    }
    if model.reasoning == Some(true) {
        entry.insert("reasoning".into(), json!(true));
    }
    if let Some(input) = &model.input
        && input.iter().any(|item| item == "image")
    {
        entry.insert("input".into(), json!(["text", "image"]));
    }
    if model.fast_mode == Some(true) {
        entry.insert("fast_mode".into(), json!(true));
    }
    if let Some(pricing) = model.pricing {
        let mut p = Map::new();
        p.insert("input".into(), json!(pricing.input));
        p.insert("output".into(), json!(pricing.output));
        if let Some(cr) = pricing.cache_read {
            p.insert("cache_read".into(), json!(cr));
        }
        if let Some(cw) = pricing.cache_write {
            p.insert("cache_write".into(), json!(cw));
        }
        entry.insert("pricing".into(), Value::Object(p));
    }
    Value::Object(entry)
}

/// 手动按 ID 添加模型时，按常见模型家族补全初始字段；未知 ID 返回 `{}`。
pub fn model_entry_for_id(model_id: &str) -> Value {
    let Some(hint) = infer_model_hint(model_id) else {
        return json!({});
    };
    model_entry_from_catalog(&CatalogModel {
        id: model_id.to_owned(),
        name: model_id.to_owned(),
        context_window: hint.context_window,
        max_output_tokens: hint.max_output_tokens,
        reasoning: hint.reasoning,
        input: hint.input,
        pricing: None,
        fast_mode: None,
        image_gen: hint.image_gen,
    })
}

/// 将已知模型提示合并到上游发现的条目上（上游显式返回的字段优先）。
pub fn enrich_catalog_model(mut model: CatalogModel) -> CatalogModel {
    if let Some(hint) = infer_model_hint(&model.id) {
        if model.image_gen.is_none() {
            model.image_gen = hint.image_gen;
        }
        if model.image_gen != Some(true) {
            if model.context_window.is_none() {
                model.context_window = hint.context_window;
            }
            if model.max_output_tokens.is_none() {
                model.max_output_tokens = hint.max_output_tokens;
            }
            if model.reasoning.is_none() {
                model.reasoning = hint.reasoning;
            }
            if model.input.is_none() {
                model.input = hint.input;
            }
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_known_families_and_respects_upstream_overrides() {
        let gpt5 = model_entry_for_id("openai/gpt-5.4");
        assert_eq!(gpt5["context_window"], 400_000);
        assert_eq!(gpt5["max_output_tokens"], 128_000);
        assert_eq!(gpt5["reasoning"], true);
        assert_eq!(gpt5["input"], json!(["text", "image"]));

        let img = model_entry_for_id("gpt-image-1");
        assert_eq!(img, json!({"image_gen": true}));

        assert_eq!(model_entry_for_id("custom/unknown-model"), json!({}));

        let upstream = enrich_catalog_model(CatalogModel {
            id: "claude-sonnet-4-5".into(),
            name: "Claude Sonnet 4.5".into(),
            context_window: Some(1_000_000),
            ..Default::default()
        });
        assert_eq!(upstream.context_window, Some(1_000_000));
        assert_eq!(upstream.max_output_tokens, Some(64_000));
        assert_eq!(upstream.reasoning, Some(true));
    }
}
