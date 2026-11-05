//! 选择压缩后保留的连续历史；不拆开一次工具调用与其返回结果。

use moc_llm::{Message, Role};

pub(super) fn retained_start(history: &[Message], budget: u64) -> usize {
    // 冷路径上的顺序分组：组数随消息数增长，Vec 保存有序边界；pending 由工具调用 ID 点查。
    let mut groups: Vec<(usize, u64)> = Vec::new();
    let mut pending = std::collections::HashSet::new();
    for (index, message) in history.iter().enumerate() {
        if groups.is_empty() || (pending.is_empty() && message.role != Role::Tool) {
            groups.push((index, 0));
        }
        for call in message.tool_calls() {
            pending.insert(call.id.as_str());
        }
        for result in message.tool_results() {
            pending.remove(result.tool_call_id.as_str());
        }
        groups.last_mut().expect("每条消息都有所属组").1 += super::message_tokens(message);
    }
    // 纯摘要仍不能吞掉尚待执行的工具调用，否则之后的结果会失去请求所有者。
    if budget == 0 && pending.is_empty() {
        return history.len();
    }
    let mut start = history.len();
    let mut retained = 0u64;
    for (index, cost) in groups.into_iter().rev() {
        if start < history.len() && retained.saturating_add(cost) > budget {
            break;
        }
        // 最新原子组不可拆；它单独超预算是唯一例外，不扩展到整个用户回合。
        start = index;
        retained = retained.saturating_add(cost);
    }
    if start == 0 && retained <= budget && pending.is_empty() {
        // 整段都能保留时手动压缩仍应生成摘要，而不是把全部正文和重复摘要叠加。
        history.len()
    } else {
        start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::{ContentPart, ToolCall};

    fn call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
                id,
                "read",
                r#"{"path":"a.rs"}"#,
            ))],
        }
    }

    #[test]
    fn long_user_turn_keeps_recent_complete_tool_groups() {
        let history = vec![
            Message::user_text("物品不要写死，应兼容 XML mod"),
            call("old"),
            Message::tool_result("old", "旧代码".repeat(50_000), false),
            call("new"),
            Message::tool_result("new", "当前迁移尚未验证", false),
        ];
        let budget = super::super::messages_tokens(&history[3..]);
        assert_eq!(retained_start(&history, budget), 3);
    }

    #[test]
    fn newest_atomic_group_is_the_only_budget_exception() {
        let history = vec![
            Message::user_text("要求"),
            call("a"),
            Message::tool_result("a", "x".repeat(1000), false),
        ];
        assert_eq!(retained_start(&history, 10), 1);
        assert_eq!(retained_start(&history, 0), history.len(), "零预算是纯摘要");
    }

    #[test]
    fn only_atomic_group_is_retained_when_it_exceeds_the_budget() {
        let history = vec![
            call("a"),
            Message::tool_result("a", "x".repeat(1000), false),
        ];
        assert_eq!(retained_start(&history, 10), 0);
        assert_eq!(retained_start(&history, 1000), history.len());
    }

    #[test]
    fn pending_tool_call_survives_even_zero_budget() {
        let history = vec![Message::user_text("执行读取"), call("pending")];
        assert_eq!(retained_start(&history, 0), 1);
    }

    #[test]
    fn parallel_results_and_injected_messages_keep_their_owner() {
        let mut owner = call("a");
        owner
            .content
            .push(ContentPart::ToolCall(ToolCall::new("b", "read", "{}")));
        let history = vec![
            Message::user_text("旧任务"),
            owner,
            Message::tool_result("b", "b", false),
            Message::custom("运行时提醒"),
            Message::tool_result("a", "a", false),
            Message::assistant_text("当前结论"),
        ];
        let budget = super::super::messages_tokens(&history[2..]);
        assert_eq!(
            retained_start(&history, budget),
            5,
            "整组放不下时不能保留孤立结果"
        );
        assert_eq!(
            retained_start(&history, super::super::messages_tokens(&history[1..])),
            1
        );
    }
}
