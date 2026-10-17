//! Grok Build 的摘要输入阶梯：原样、预算裁剪、有损裁剪三档。

use moc_llm::{ContentPart, Message, Role};

use crate::compact::{
    MESSAGE_TOKEN_OVERHEAD, estimate_tokens, message_tokens, messages_tokens, prefix_within_tokens,
};

/// 为摘要输出、系统提示与工具定义预留的窗口份额。
pub(crate) const SUMMARY_BUDGET_RESERVE_TOKENS: u64 = 32_768;

/// 有损阶段的窗口占比；超限请求只保留这部分对话。
pub(crate) const LOSSY_WINDOW_PERCENT: u64 = 70;

/// 摘要请求的输入档位，按代价从低到高排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputStage {
    /// 原始对话原样发送，只修掉不完整的尾部工具调用。
    Verbatim,
    /// 丢弃最老的完整轮次，直到放进预算。
    Fitted,
    /// 先删除图片与思维链，再按更紧的比例裁剪。
    Lossy,
}

impl InputStage {
    /// 阶梯中的下一档；`Lossy` 之后没有档位，调用方应报告上下文超限。
    pub(crate) fn next(self) -> Option<Self> {
        match self {
            Self::Verbatim => Some(Self::Fitted),
            Self::Fitted => Some(Self::Lossy),
            Self::Lossy => None,
        }
    }
}

/// 保留摘要输出余量后的输入预算。
pub(crate) fn fitted_budget(context_window: u64, tools_tokens: u64) -> u64 {
    context_window
        .saturating_sub(SUMMARY_BUDGET_RESERVE_TOKENS)
        .saturating_sub(tools_tokens)
}

/// 有损阶段的输入预算。
pub(crate) fn lossy_budget(context_window: u64, tools_tokens: u64) -> u64 {
    context_window
        .saturating_mul(LOSSY_WINDOW_PERCENT)
        .saturating_div(100)
        .saturating_sub(tools_tokens)
}

/// 首次尝试使用的档位：系统提示与对话合计放得下就原样发送。
pub(crate) fn initial_stage(
    system: &[Message],
    messages: &[Message],
    tools_tokens: u64,
    context_window: u64,
) -> InputStage {
    let total = messages_tokens(system).saturating_add(messages_tokens(messages));
    if total > fitted_budget(context_window, tools_tokens) {
        InputStage::Fitted
    } else {
        InputStage::Verbatim
    }
}

/// 按档位准备本次摘要请求的对话历史。
pub(crate) fn prepare_stage(
    stage: InputStage,
    messages: Vec<Message>,
    context_window: u64,
    tools_tokens: u64,
) -> Vec<Message> {
    let messages = truncate_incomplete_tail(messages);
    match stage {
        InputStage::Verbatim => messages,
        InputStage::Fitted => {
            fit_conversation_to_budget(messages, fitted_budget(context_window, tools_tokens))
        }
        InputStage::Lossy => fit_conversation_to_budget(
            drop_images_and_reasoning(messages),
            lossy_budget(context_window, tools_tokens),
        ),
    }
}

/// 删除尾部悬空的助手工具调用：被裁掉结果的调用不能单独发往上游。
pub(crate) fn truncate_incomplete_tail(mut messages: Vec<Message>) -> Vec<Message> {
    while messages.last().is_some_and(|message| {
        message.role == Role::Assistant && message.tool_calls().next().is_some()
    }) {
        messages.pop();
    }
    messages
}

/// 有损阶段的内容降级：图片与思维链不参与摘要。
fn drop_images_and_reasoning(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|message| Message {
            role: message.role,
            content: message
                .content
                .into_iter()
                .filter(|part| {
                    !matches!(part, ContentPart::Image { .. } | ContentPart::Reasoning(_))
                })
                .collect(),
        })
        .collect()
}

/// 按完整轮次裁剪历史，绝不拆开工具调用与结果的配对。
///
/// 轮次从最新一轮向前累加；最新一轮自身超预算时退化为有界摘录，因为摘要请求
/// 必须可发送——否则该次压缩必然超窗，之后没有任何方法能成功。
pub(crate) fn fit_conversation_to_budget(messages: Vec<Message>, budget: u64) -> Vec<Message> {
    if messages.is_empty() || messages_tokens(&messages) <= budget {
        return messages;
    }
    let rounds = rounds(&messages);
    let mut total = 0u64;
    let mut start = messages.len();
    for round in rounds.iter().rev() {
        let cost = messages_tokens(&messages[round.clone()]);
        if total.saturating_add(cost) > budget {
            break;
        }
        total = total.saturating_add(cost);
        start = round.start;
    }
    if start < messages.len() {
        return messages[start..].to_vec();
    }
    match rounds.last() {
        Some(newest) => shrink_round(&messages[newest.clone()], budget),
        None => shrink_round(&messages, budget),
    }
}

/// 按用户消息切分轮次；首轮可以从助手/工具消息开始。
fn rounds(messages: &[Message]) -> Vec<std::ops::Range<usize>> {
    let mut starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, _)| index)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts.push(messages.len());
    starts
        .windows(2)
        .filter(|pair| pair[0] < pair[1])
        .map(|pair| pair[0]..pair[1])
        .collect()
}

/// 单轮超预算时的有界摘录：保留尾部工具结果整组，并尽量保住该轮的用户请求。
fn shrink_round(round: &[Message], budget: u64) -> Vec<Message> {
    let mut results_start = round.len();
    while results_start > 0 && round[results_start - 1].role == Role::Tool {
        results_start -= 1;
    }
    if results_start == round.len() {
        // 这一轮没有尾部工具结果：优先保住用户请求，其次才退到最后一条消息。
        let message = round
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .or_else(|| round.last());
        return match message {
            Some(message) => vec![truncate_message(message.clone(), budget)],
            None => Vec::new(),
        };
    }
    let results = &round[results_start..];
    let owner = results_start
        .checked_sub(1)
        .map(|index| &round[index])
        .filter(|message| message.role == Role::Assistant && message.tool_calls().next().is_some());
    if let Some(owner) = owner {
        let prompt = round
            .iter()
            .rev()
            .find(|message| message.role == Role::User);
        let count = u64::try_from(results.len()).unwrap_or(u64::MAX).max(1);
        let overhead = count.saturating_mul(MESSAGE_TOKEN_OVERHEAD);
        // 每个结果自身还有固定开销；先扣掉它，裁剪后的候选才真正放得进预算。
        let shrink = |reserved: u64| {
            budget
                .saturating_sub(reserved)
                .saturating_sub(overhead)
                .saturating_div(count)
                .max(1)
        };
        // 摘要模型必须看得到这一轮的用户请求，否则摘要会缺掉最新的意图。
        if let Some(prompt) = prompt {
            let reserved = message_tokens(owner).saturating_add(message_tokens(prompt));
            let mut candidate = Vec::with_capacity(results.len() + 2);
            candidate.push(prompt.clone());
            candidate.push(owner.clone());
            let per = shrink(reserved);
            candidate.extend(
                results
                    .iter()
                    .cloned()
                    .map(|result| truncate_message(result, per)),
            );
            if messages_tokens(&candidate) <= budget {
                return candidate;
            }
        }
        let reserved = message_tokens(owner);
        let mut candidate = Vec::with_capacity(results.len() + 1);
        candidate.push(owner.clone());
        let per = shrink(reserved);
        candidate.extend(
            results
                .iter()
                .cloned()
                .map(|result| truncate_message(result, per)),
        );
        if messages_tokens(&candidate) <= budget {
            return candidate;
        }
    }
    // 配对放不下时只保留用户消息：孤立的工具结果会被上游拒绝。
    let user = round
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .or_else(|| round.last());
    match user {
        Some(message) => vec![truncate_message(message.clone(), budget)],
        None => Vec::new(),
    }
}

/// 把单条消息的文本与工具结果裁剪到 `budget` 估算 token。
fn truncate_message(message: Message, budget: u64) -> Message {
    let mut remaining = budget;
    let mut content = Vec::with_capacity(message.content.len());
    for part in message.content {
        match part {
            ContentPart::Text(text) => {
                let kept = prefix_within_tokens(&text, remaining).to_owned();
                remaining = remaining.saturating_sub(estimate_tokens(&kept));
                content.push(ContentPart::Text(kept));
            }
            ContentPart::ToolResult(mut result) => {
                let kept = prefix_within_tokens(&result.content, remaining).to_owned();
                remaining = remaining.saturating_sub(estimate_tokens(&kept));
                result.content = kept;
                content.push(ContentPart::ToolResult(result));
            }
            other => content.push(other),
        }
    }
    Message {
        role: message.role,
        content,
    }
}

#[cfg(test)]
mod tests {
    use moc_llm::{ImageSource, ToolCall};

    use super::*;

    fn text_tokens(chars: usize) -> String {
        "a".repeat(chars * 4)
    }

    #[test]
    fn budgets_reserve_output_and_scale_with_window() {
        assert_eq!(fitted_budget(200_000, 10_000), 200_000 - 32_768 - 10_000);
        assert_eq!(lossy_budget(200_000, 10_000), 140_000 - 10_000);
        assert_eq!(fitted_budget(10_000, 0), 0, "小窗口不为负");
    }

    #[test]
    fn initial_stage_only_cuts_when_system_plus_history_exceeds_budget() {
        let system = vec![Message::user_text("system")];
        let small = vec![Message::user_text("hi")];
        assert_eq!(
            initial_stage(&system, &small, 0, 200_000),
            InputStage::Verbatim
        );
        let large = vec![Message::user_text(text_tokens(8_000))];
        assert_eq!(
            initial_stage(&system, &large, 0, 40_000),
            InputStage::Fitted
        );
    }

    #[test]
    fn whole_rounds_are_dropped_and_tool_pairs_stay_together() {
        let round_one = vec![
            Message::user_text(text_tokens(100)),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new(
                    "c1",
                    "read",
                    r#"{"path":"a"}"#,
                ))],
            },
            Message::tool_result("c1", text_tokens(100), false),
        ];
        let round_two = vec![Message::user_text("第二轮")];
        let mut messages = round_one;
        messages.extend(round_two);

        let fitted = fit_conversation_to_budget(messages.clone(), 50);
        assert_eq!(
            fitted,
            vec![Message::user_text("第二轮")],
            "只保留最新完整轮次"
        );

        let fitted = fit_conversation_to_budget(messages, 1_000);
        assert_eq!(fitted.len(), 4, "预算足够时整轮保留");
        assert!(fitted.iter().any(|message| message.role == Role::Tool));
    }

    #[test]
    fn oversized_newest_round_keeps_request_owner_and_truncated_results() {
        let prompt = Message::user_text(text_tokens(50));
        let owner = Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall(ToolCall::new(
                "c1",
                "read",
                r#"{"path":"a"}"#,
            ))],
        };
        let messages = vec![
            prompt.clone(),
            owner.clone(),
            Message::tool_result("c1", text_tokens(4_000), false),
        ];
        let fitted = fit_conversation_to_budget(messages, 400);
        assert_eq!(fitted.len(), 3, "{fitted:?}");
        assert_eq!(fitted[0], prompt, "这一轮的用户请求必须保留");
        assert_eq!(fitted[1], owner);
        assert_eq!(fitted[2].role, Role::Tool, "工具结果必须与调用配对");
        assert!(
            messages_tokens(&fitted) <= 400,
            "裁剪后应有界：{}",
            messages_tokens(&fitted)
        );

        let fitted = fit_conversation_to_budget(vec![Message::user_text(text_tokens(4_000))], 100);
        assert_eq!(fitted.len(), 1);
        assert!(
            messages_tokens(&fitted) <= 140,
            "{}",
            messages_tokens(&fitted)
        );
    }

    #[test]
    fn dangling_tool_call_tail_is_dropped() {
        let messages = vec![
            Message::user_text("做点事"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new("c1", "read", "{}"))],
            },
        ];
        assert_eq!(
            truncate_incomplete_tail(messages),
            vec![Message::user_text("做点事")]
        );
    }

    #[test]
    fn lossy_stage_drops_images_and_reasoning_before_fitting() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentPart::Text("看图".into()),
                ContentPart::Image {
                    source: ImageSource::Url("https://example.invalid/a.png".into()),
                    detail: None,
                },
            ],
        }];
        let prepared = prepare_stage(InputStage::Lossy, messages, 200_000, 0);
        assert_eq!(prepared, vec![Message::user_text("看图")]);

        let reasoning = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning(moc_llm::ReasoningBlock::text("想")),
                ContentPart::Text("答".into()),
            ],
        }];
        let prepared = prepare_stage(InputStage::Lossy, reasoning, 200_000, 0);
        assert_eq!(
            prepared,
            vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Text("答".into())],
            }]
        );
    }

    #[test]
    fn verbatim_stage_keeps_everything_but_dangling_calls() {
        let messages = vec![Message::user_text("hi"), Message::user_text("there")];
        assert_eq!(
            prepare_stage(InputStage::Verbatim, messages.clone(), 1, 1),
            messages
        );
    }
}
