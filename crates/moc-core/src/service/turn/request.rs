//! 输入投影、推理档位与稳定缓存断点。

use moc_llm::{
    CacheConfig, ChatRequest, ContentPart, Effort, Message, ReasoningConfig, ReasoningEcho,
    RequestOptions, Role,
};
use moc_protocol::methods::InputPart;
use serde_json::Value;
pub(super) fn project_input(input: Vec<InputPart>, accepts_image: bool) -> Message {
    let mut content = Vec::with_capacity(input.len());
    for part in input {
        match part {
            InputPart::Text { text } => content.push(ContentPart::Text(text)),
            InputPart::Image { source, detail } if accepts_image => {
                content.push(ContentPart::Image { source, detail })
            }
            InputPart::Image { .. } => content.push(ContentPart::Text(
                "[用户提供了图片（当前模型不支持图片输入）]".into(),
            )),
        }
    }
    Message {
        role: Role::User,
        content,
    }
}

pub(super) fn build_request(
    model: &crate::ResolvedModel,
    config: &crate::ResolvedConfig,
    messages: Vec<Message>,
    tools: Vec<moc_llm::ToolDef>,
    native_input: Option<Vec<Value>>,
    session_id: &str,
) -> ChatRequest {
    let echo_reasoning = if model.requires_reasoning_content_for_tool_calls
        || model.requires_reasoning_content_for_all_assistant_turns
    {
        ReasoningEcho::Always
    } else {
        model.echo_reasoning
    };
    let mut breakpoints = vec![0];
    if messages.len() > 1 {
        breakpoints.push(messages.len() - 1);
    }
    ChatRequest {
        messages,
        native_input,
        tools,
        tool_choice: None,
        max_output_tokens: Some(config.limits.max_output_tokens.min(model.max_output_tokens)),
        temperature: None,
        top_p: None,
        top_k: None,
        stop: Vec::new(),
        reasoning: Some(ReasoningConfig {
            enabled: model.capabilities.reasoning
                && model.thinking_level != crate::ThinkingLevel::Off,
            effort: model.thinking_level.effort(),
            budget_tokens: anthropic_budget(model),
            summary: None,
        }),
        response_format: None,
        parallel_tool_calls: Some(model.llm.wire.parallel_tool_calls),
        options: RequestOptions {
            echo_reasoning,
            cache: (model.supports_prompt_cache_key
                || model.llm.protocol == moc_llm::Protocol::AnthropicMessages)
                .then(|| CacheConfig {
                    key: Some(session_id.to_owned()),
                    breakpoints,
                    ttl: None,
                }),
            ..RequestOptions::default()
        },
    }
}

fn anthropic_budget(model: &crate::ResolvedModel) -> Option<u32> {
    if model.llm.protocol != moc_llm::Protocol::AnthropicMessages
        || model.llm.wire.anthropic_thinking != moc_llm::AnthropicThinkingDialect::ManualBudget
        || !model
            .thinking_level
            .effort()
            .is_some_and(|effort| effort != Effort::None)
    {
        return None;
    }
    Some(match model.thinking_level {
        crate::ThinkingLevel::Minimal => 1024,
        crate::ThinkingLevel::Low => 2048,
        crate::ThinkingLevel::Medium => 8192,
        crate::ThinkingLevel::High => 16384,
        crate::ThinkingLevel::Xhigh | crate::ThinkingLevel::Max => 32768,
        crate::ThinkingLevel::Off | crate::ThinkingLevel::Auto => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::ImageSource;

    #[test]
    fn unsupported_images_become_explicit_text_and_supported_images_remain_images() {
        let image = || InputPart::Image {
            source: ImageSource::Url("https://example.invalid/image.png".into()),
            detail: None,
        };
        let unsupported = project_input(vec![image()], false);
        assert_eq!(
            unsupported.text(),
            "[用户提供了图片（当前模型不支持图片输入）]"
        );
        let supported = project_input(vec![image()], true);
        assert!(matches!(supported.content[0], ContentPart::Image { .. }));
    }
}
