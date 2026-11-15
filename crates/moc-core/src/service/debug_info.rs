//! `/debuginfo` 仅统计保留历史中的工具终态和压缩次数。

use rust_i18n::t;
use super::{CoreInner, to_rpc};
use crate::session::{entries::Entry, stored::StoredEntry};
use moc_protocol::{RpcError, ToolStatus, TranscriptChildCall, TranscriptEntry};
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

pub(super) async fn report(inner: &CoreInner, session_id: &str) -> Result<String, RpcError> {
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let (transcript, children, compactions) = {
        let state = record.state.lock().await;
        let compactions = state.entries.iter().filter(|entry| {
            matches!(entry, StoredEntry::Other(entry) if matches!(entry.as_ref(), Entry::Compaction { .. }))
        }).count();
        (
            Arc::clone(&state.transcript),
            Arc::clone(&state.child_calls),
            compactions,
        )
    };
    // 冻结的消息可能需要解码；不占用异步执行线程或持有会话状态锁。
    tokio::task::spawn_blocking(move || {
        let stats = collect(&transcript, &children);
        render(stats, compactions)
    })
    .await
    .map_err(|error| to_rpc(crate::CoreError::Session(error.to_string())))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    completed: u64,
    error: u64,
    denied: u64,
    cancelled: u64,
    pending: u64,
}

impl Counts {
    fn add(&mut self, status: Option<ToolStatus>) {
        match status {
            Some(ToolStatus::Completed) => self.completed += 1,
            Some(ToolStatus::Error) => self.error += 1,
            Some(ToolStatus::Denied) => self.denied += 1,
            Some(ToolStatus::Cancelled) => self.cancelled += 1,
            None => self.pending += 1,
        }
    }

    fn calls(&self) -> u64 {
        self.completed + self.error + self.denied + self.cancelled + self.pending
    }

    fn merge(&mut self, other: &Self) {
        self.completed += other.completed;
        self.error += other.error;
        self.denied += other.denied;
        self.cancelled += other.cancelled;
        self.pending += other.pending;
    }

    fn rate(&self) -> String {
        let finished = self.completed + self.error;
        if finished == 0 {
            "—".into()
        } else {
            let tenths = (u128::from(self.completed) * 1000 + u128::from(finished) / 2)
                / u128::from(finished);
            format!("{}.{:01}%", tenths / 10, tenths % 10)
        }
    }
}

fn collect<'a>(
    transcript: &'a [Arc<TranscriptEntry>],
    children: &'a [TranscriptChildCall],
) -> HashMap<&'a str, Counts> {
    // 调用 ID 稀疏且无固定上界；只借用元数据，不复制正文或工具参数。
    let mut outcomes = HashMap::new();
    for entry in transcript {
        if entry.bash.is_some() {
            continue;
        }
        if let Some(tool) = &entry.tool {
            outcomes.insert(tool.tool_call_id.as_str(), tool.status);
        } else if entry.message.role == moc_llm::Role::Tool {
            // 旧日志可能只有工具结果，没有 ToolExecution 元数据。
            for result in entry.message.tool_results() {
                outcomes.insert(
                    result.tool_call_id.as_str(),
                    if result.is_error {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    },
                );
            }
        }
    }
    let mut groups: HashMap<&str, Counts> = HashMap::new();
    for entry in transcript {
        if entry.bash.is_some() || entry.message.role != moc_llm::Role::Assistant {
            continue;
        }
        for call in entry.message.tool_calls() {
            groups
                .entry(&call.name)
                .or_default()
                .add(outcomes.get(call.id.as_str()).copied());
        }
    }
    // PTC 内部调用不在 transcript 中，每条只从 child_calls 统计一次。
    for call in children {
        groups.entry(&call.name).or_default().add(Some(call.status));
    }
    groups
}

fn render(groups: HashMap<&str, Counts>, compactions: usize) -> String {
    let mut rows: Vec<_> = groups.into_iter().collect();
    rows.sort_unstable_by(|(left_name, left), (right_name, right)| {
        right
            .calls()
            .cmp(&left.calls())
            .then_with(|| left_name.cmp(right_name))
    });
    let mut text = t!("core.debug.title").into_owned();
    let mut total = Counts::default();
    for (name, counts) in rows {
        append_counts(&mut text, name, &counts);
        total.merge(&counts);
    }
    append_counts(&mut text, &t!("core.debug.total"), &total);
    let _ = write!(
        text,
        "\n{}",
        t!("core.debug.compactions", n = compactions)
    );
    text
}

fn append_counts(text: &mut String, name: &str, counts: &Counts) {
    let _ = write!(
        text,
        "{}",
        t!(
            "core.debug.row",
            name = name,
            calls = counts.calls(),
            ok = counts.completed,
            err = counts.error,
            rate = counts.rate()
        )
    );
    for (key, count) in [
        ("core.debug.denied", counts.denied),
        ("core.debug.cancelled", counts.cancelled),
        ("core.debug.pending", counts.pending),
    ] {
        if count != 0 {
            let _ = write!(text, " · {} {count}", t!(key));
        }
    }
    text.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::{ContentPart, Message, Role, ToolCall};
    use moc_protocol::ToolExecution;

    fn entry(message: Message, tool: Option<ToolExecution>) -> Arc<TranscriptEntry> {
        Arc::new(TranscriptEntry {
            id: String::new(),
            at_ms: 0,
            message,
            tool,
            bash: None,
            agent: None,
            agent_result: None,
            goal_round: None,
            reminder: None,
        })
    }

    #[test]
    fn tool_counts_keep_outcomes_and_nested_calls_distinct() {
        let ids = ["ok", "error", "denied", "cancelled", "pending", "legacy"];
        let calls = ids
            .iter()
            .map(|id| ContentPart::ToolCall(ToolCall::new(*id, "read", "{}")))
            .collect::<Vec<_>>();
        let mut transcript = vec![entry(
            Message {
                role: Role::Assistant,
                content: calls.into(),
            },
            None,
        )];
        for (id, status) in ids.iter().zip([
            ToolStatus::Completed,
            ToolStatus::Error,
            ToolStatus::Denied,
            ToolStatus::Cancelled,
        ]) {
            transcript.push(entry(
                Message::tool_result(*id, "", true),
                Some(ToolExecution {
                    tool_call_id: (*id).into(),
                    status,
                    duration_ms: None,
                    diff: None,
                }),
            ));
        }
        transcript.push(entry(Message::tool_result("legacy", "", false), None));
        let mut manual = entry(Message::tool_result("ok", "", true), None);
        Arc::get_mut(&mut manual).unwrap().bash = Some(moc_protocol::BashCommand {
            command: "false".into(),
            exclude_from_context: true,
        });
        transcript.push(manual);
        let children = [TranscriptChildCall {
            id: "nested".into(),
            parent_tool_call_id: "execute".into(),
            name: "read".into(),
            arguments: "{}".into(),
            result: String::new(),
            status: ToolStatus::Completed,
            duration_ms: None,
            diff: None,
        }];
        let stats = collect(&transcript, &children);
        assert_eq!(
            stats["read"],
            Counts {
                completed: 3,
                error: 1,
                denied: 1,
                cancelled: 1,
                pending: 1
            }
        );
        assert_eq!(stats["read"].calls(), 7);
        assert_eq!(stats["read"].rate(), "75.0%");
        assert_eq!(
            Counts {
                denied: 2,
                cancelled: 1,
                ..Counts::default()
            }
            .rate(),
            "—"
        );
    }
}
