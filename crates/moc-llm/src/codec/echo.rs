//! 思维链回传规则（三个协议共用）。
//!
//! 需求的核心：部分上游要求把上一轮的思维链原样回传，否则请求会被拒绝——
//! DeepSeek 在携带 `tools` 的思考模式请求里必须完整回传 `reasoning_content`
//! （缺失时报 400 `The reasoning_content in the thinking mode must be passed
//! back to the API.`）；Anthropic 的工具续跑请求必须回传 `thinking`（含
//! `signature`，逐字节不变）与 `redacted_thinking`，否则报 400
//! `thinking or redacted_thinking blocks in the latest assistant message cannot
//! be modified`；Responses 在手动管理上下文（`store:false` / ZDR）时必须把
//! reasoning item 放回后续 `input`。
//!
//! 回传范围由 [`crate::ReasoningEcho`] 决定，协议编码器只调用本模块，不各自
//! 实现过滤。

use crate::config::{LlmConfig, Protocol};
use crate::types::{ChatRequest, ReasoningBlock, ReasoningEcho};

/// 协议在编码 assistant 历史时对思维链块的处置方案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningEchoPlan {
    /// 不回传任何思维链块。
    Strip,
    /// 只回传文本块（Chat 的 `reasoning_content`）。
    TextOnly,
    /// 回传文本块与不透明块（Anthropic 的 `thinking` / `redacted_thinking`）。
    TextAndOpaque,
    /// 只回传不透明条目（Responses 的 reasoning item）。
    ItemsOnly,
}

impl ReasoningEchoPlan {
    /// 该思维链块是否参与本次回传。
    pub fn keeps(self, block: &ReasoningBlock) -> bool {
        match self {
            ReasoningEchoPlan::Strip => false,
            ReasoningEchoPlan::TextOnly => matches!(block, ReasoningBlock::Text { .. }),
            ReasoningEchoPlan::TextAndOpaque => matches!(
                block,
                ReasoningBlock::Text { .. } | ReasoningBlock::Redacted { .. }
            ),
            ReasoningEchoPlan::ItemsOnly => matches!(block, ReasoningBlock::Item(_)),
        }
    }
}

/// 本次请求是否回传思维链。
///
/// [`ReasoningEcho::Protocol`] 下的协议默认规则：
///
/// | 协议 | 默认行为 |
/// |---|---|
/// | Chat | 请求携带 `tools` 时回传，否则丢弃 |
/// | Anthropic | 总是回传 |
/// | Responses | 总是回传 |
pub(crate) fn echoes_reasoning(cfg: &LlmConfig, req: &ChatRequest) -> bool {
    match req.options.echo_reasoning {
        ReasoningEcho::Always => true,
        ReasoningEcho::Never => false,
        ReasoningEcho::Protocol => match cfg.protocol {
            Protocol::OpenAiChat => !req.tools.is_empty(),
            Protocol::OpenAiResponses | Protocol::AnthropicMessages => true,
        },
    }
}

/// 本次请求的回传方案。
pub(crate) fn plan(cfg: &LlmConfig, req: &ChatRequest) -> ReasoningEchoPlan {
    if !echoes_reasoning(cfg, req) {
        return ReasoningEchoPlan::Strip;
    }
    match cfg.protocol {
        Protocol::OpenAiChat => ReasoningEchoPlan::TextOnly,
        Protocol::AnthropicMessages => ReasoningEchoPlan::TextAndOpaque,
        Protocol::OpenAiResponses => ReasoningEchoPlan::ItemsOnly,
    }
}

/// Chat 回传思维链使用的字段名。
pub(crate) fn reasoning_field(cfg: &LlmConfig) -> &str {
    &cfg.wire.reasoning_field
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WireConfig;
    use crate::types::{Message, ReasoningBlock, ToolDef};

    fn config(protocol: Protocol) -> LlmConfig {
        LlmConfig {
            protocol,
            ..Default::default()
        }
    }

    fn request(echo: ReasoningEcho, tools: usize) -> ChatRequest {
        ChatRequest {
            tools: (0..tools)
                .map(|index| ToolDef::new(format!("tool_{index}"), serde_json::json!({})))
                .collect(),
            options: crate::types::RequestOptions {
                echo_reasoning: echo,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn text_block() -> ReasoningBlock {
        ReasoningBlock::Text {
            text: "思路".into(),
            signature: Some("sig".into()),
        }
    }

    #[test]
    fn chat_protocol_mode_echoes_only_with_tools() {
        let cfg = config(Protocol::OpenAiChat);
        assert_eq!(
            plan(&cfg, &request(ReasoningEcho::Protocol, 1)),
            ReasoningEchoPlan::TextOnly
        );
        assert_eq!(
            plan(&cfg, &request(ReasoningEcho::Protocol, 0)),
            ReasoningEchoPlan::Strip
        );
    }

    #[test]
    fn chat_always_and_never_override_protocol_rule() {
        let cfg = config(Protocol::OpenAiChat);
        assert_eq!(
            plan(&cfg, &request(ReasoningEcho::Always, 0)),
            ReasoningEchoPlan::TextOnly
        );
        assert_eq!(
            plan(&cfg, &request(ReasoningEcho::Never, 3)),
            ReasoningEchoPlan::Strip
        );
    }

    #[test]
    fn chat_custom_field_name() {
        let cfg = LlmConfig {
            protocol: Protocol::OpenAiChat,
            wire: WireConfig {
                reasoning_field: "reasoning".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(reasoning_field(&cfg), "reasoning");
    }

    #[test]
    fn anthropic_protocol_mode_echoes_thinking_and_redacted() {
        let cfg = config(Protocol::AnthropicMessages);
        let plan = plan(&cfg, &request(ReasoningEcho::Protocol, 0));
        assert_eq!(plan, ReasoningEchoPlan::TextAndOpaque);
        // 按变体白名单过滤：redacted_thinking 与 thinking 同等对待，
        // 不能写成 `type == "thinking"` 的单类型判断。
        assert!(plan.keeps(&text_block()));
        assert!(plan.keeps(&ReasoningBlock::Redacted {
            data: "opaque".into()
        }));
        assert!(!plan.keeps(&ReasoningBlock::Item(Box::new(
            serde_json::json!({"type": "reasoning"})
        ))));
    }

    #[test]
    fn anthropic_never_strips_thinking_blocks() {
        let cfg = config(Protocol::AnthropicMessages);
        let plan = plan(&cfg, &request(ReasoningEcho::Never, 0));
        assert_eq!(plan, ReasoningEchoPlan::Strip);
        assert!(!plan.keeps(&text_block()));
        assert!(!plan.keeps(&ReasoningBlock::Redacted {
            data: "opaque".into()
        }));
    }

    #[test]
    fn responses_protocol_mode_echoes_reasoning_items() {
        let cfg = config(Protocol::OpenAiResponses);
        let plan = plan(&cfg, &request(ReasoningEcho::Protocol, 0));
        assert_eq!(plan, ReasoningEchoPlan::ItemsOnly);
        assert!(plan.keeps(&ReasoningBlock::Item(Box::new(
            serde_json::json!({"type": "reasoning", "id": "rs_1"})
        ))));
        assert!(!plan.keeps(&text_block()));
        assert!(!plan.keeps(&ReasoningBlock::Redacted {
            data: "opaque".into()
        }));
    }

    #[test]
    fn never_mode_drops_reasoning_blocks_for_all_protocols() {
        for protocol in [
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
            Protocol::AnthropicMessages,
        ] {
            let cfg = config(protocol);
            let plan = plan(&cfg, &request(ReasoningEcho::Never, 2));
            assert_eq!(plan, ReasoningEchoPlan::Strip, "{protocol:?}");
            for block in [
                text_block(),
                ReasoningBlock::Redacted {
                    data: "opaque".into(),
                },
                ReasoningBlock::Item(Box::new(serde_json::json!({"type": "reasoning"}))),
            ] {
                assert!(!plan.keeps(&block), "{protocol:?} 应剔除 {block:?}");
            }
        }
    }

    #[test]
    fn message_helpers_used_by_encoders_agree_with_plan() {
        let message = Message {
            role: crate::types::Role::Assistant,
            content: vec![
                crate::types::ContentPart::Reasoning(text_block()),
                crate::types::ContentPart::Text("正文".into()),
            ],
        };
        let plan = ReasoningEchoPlan::TextOnly;
        let kept: Vec<&ReasoningBlock> = message
            .content
            .iter()
            .filter_map(|part| match part {
                crate::types::ContentPart::Reasoning(block) if plan.keeps(block) => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(kept.len(), 1);
    }
}
