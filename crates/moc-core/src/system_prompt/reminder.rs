//! 运行时提醒条目：决定哪些事实要进历史、什么时候进。
//!
//! 提醒是**条目化**的：每个事实有自己的稳定 key，注入前按内容摘要与历史里已有的
//! 那份比较，只有变化了才写一条新的进去。写进去的提醒成为历史的一部分，因此可以
//! 被缓存，也不会在后续每个请求里重复付费。
//!
//! 后台任务完成和工作区变更通过会话 notice 告知模型（见 `tools/job.rs`、
//! `service/prompt/workspace.rs`），这里不重复。

use std::collections::BTreeMap;
use std::process::Command;

use moc_llm::Message;
use moc_protocol::ReminderItem;

/// 条目的稳定键。跨会话重启、跨版本都不能改，否则历史里的旧条目会失去对应。
pub mod key {
    pub const SANDBOX: &str = "sandbox";
    pub const MODE: &str = "mode";
    pub const MCP_INDEX: &str = "mcp-index";
    pub const ROSTER: &str = "roster";
    pub const TODO_STALL: &str = "todo-stall";
    pub const LOCAL_TIME: &str = "local-time";
    pub const CYCLE: &str = "cycle";
    pub const THINKING: &str = "thinking";
    pub const NOTICE: &str = "notice";
}

/// 一个待注入的运行时提醒条目。
#[derive(Debug, Clone)]
pub struct Reminder {
    /// 稳定键；同一个键的新条目覆盖旧条目。
    pub key: &'static str,
    /// 条目正文，不含 `<system-reminder>` 外壳。
    pub text: String,
    /// 一次性条目不计入去重表：它每次出现都是新事实，相同的文字也可能需要重发。
    pub ephemeral: bool,
}

impl Reminder {
    /// 一个持久条目：内容不变就不再注入。
    pub fn durable(key: &'static str, text: String) -> Self {
        Self {
            key,
            text,
            ephemeral: false,
        }
    }

    /// 一个一次性条目：每次都参与注入，且不写入去重表。
    pub fn once(key: &'static str, text: String) -> Self {
        Self {
            key,
            text,
            ephemeral: true,
        }
    }
}

/// 条目正文的摘要，用于判断「这条是否已经在历史里」。
pub fn digest(text: &str) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
}

/// 从候选条目里挑出需要注入的那些。
///
/// 一次性条目总是入选；持久条目只在从未注入或正文变化时入选。
pub fn pending<'a>(
    candidates: &'a [Reminder],
    known: &BTreeMap<String, String>,
) -> Vec<&'a Reminder> {
    candidates
        .iter()
        .filter(|reminder| {
            reminder.ephemeral || known.get(reminder.key) != Some(&digest(&reminder.text))
        })
        .collect()
}

/// 把选中的条目渲染成一条 [`Role::Custom`] 消息，并给出要记进历史的条目摘要。
///
/// 每个条目各自包一层 `<system-reminder>`：模型据此区分提醒与用户输入，人也认得。
/// 角色本身已经是 `Custom`，所以重放与压缩不必再嗅探这个前缀。条目之间空一行。
pub fn render(selected: &[&Reminder]) -> (Message, Vec<ReminderItem>) {
    let text = selected
        .iter()
        .map(|reminder| format!("<system-reminder>{}</system-reminder>", reminder.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let items = selected
        .iter()
        .filter(|reminder| !reminder.ephemeral)
        .map(|reminder| ReminderItem {
            key: reminder.key.to_owned(),
            digest: digest(&reminder.text),
        })
        .collect();
    (Message::custom(text), items)
}

#[derive(Debug, Clone, Default)]
pub struct ReminderState {
    last_day: Option<String>,
}

impl ReminderState {
    /// 本地日期变化时返回时间提醒正文，否则返回 `None`。
    pub fn render(&mut self, now_local: &str) -> Option<String> {
        let day = now_local.get(..10).unwrap_or(now_local);
        if self.last_day.as_deref() == Some(day) {
            return None;
        }
        self.last_day = Some(day.to_owned());
        Some(format!("Current local time: {now_local}."))
    }
}

/// 本机本地时区的当前时间；`ReminderState` 只在日期变化时注入。
pub fn local_time() -> String {
    #[cfg(unix)]
    let output = Command::new("date")
        .args(["+%Y-%m-%d %H:%M (%:z)"])
        .output();
    #[cfg(windows)]
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Date -Format 'yyyy-MM-dd HH:mm (zzz)'",
        ])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| "time unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_time_once_per_local_day() {
        let mut state = ReminderState::default();
        assert!(
            state
                .render("2026-09-24 18:33 (+08:00)")
                .unwrap()
                .contains("Current local time")
        );
        assert!(state.render("2026-09-24 18:45 (+08:00)").is_none());
        assert!(
            state
                .render("2026-09-25 00:01 (+08:00)")
                .unwrap()
                .contains("Current local time")
        );
    }

    fn known_of(items: Vec<ReminderItem>) -> BTreeMap<String, String> {
        items
            .into_iter()
            .map(|item| (item.key, item.digest))
            .collect()
    }

    #[test]
    fn unchanged_entries_are_not_pending_again() {
        let candidates = vec![
            Reminder::durable(key::SANDBOX, "Sandbox mode: workspace.".into()),
            Reminder::durable(key::MODE, "Ask mode is active.".into()),
        ];

        let first = pending(&candidates, &BTreeMap::new());
        assert_eq!(first.len(), 2);
        let (message, items) = render(&first);
        assert_eq!(items.len(), 2);
        assert!(message.text().contains("<system-reminder>Sandbox mode"));
        assert!(message.text().contains("<system-reminder>Ask mode"));

        assert!(
            pending(&candidates, &known_of(items)).is_empty(),
            "内容没变就不该再注入"
        );
    }

    #[test]
    fn only_the_changed_entry_is_pending() {
        let mode = Reminder::durable(key::MODE, "Ask mode is active.".into());
        let known = known_of(
            render(&pending(
                &[
                    Reminder::durable(key::SANDBOX, "Sandbox mode: workspace.".into()),
                    mode.clone(),
                ],
                &BTreeMap::new(),
            ))
            .1,
        );

        let changed_entries = [
            Reminder::durable(key::SANDBOX, "Sandbox mode: full.".into()),
            mode,
        ];
        let changed = pending(&changed_entries, &known);
        assert_eq!(changed.len(), 1, "只有沙盒变了");
        assert_eq!(changed[0].key, key::SANDBOX);
        let (message, items) = render(&changed);
        assert!(message.text().contains("Sandbox mode: full"));
        assert!(!message.text().contains("Ask mode"), "未变的模式不该重发");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn ephemeral_entries_always_inject_and_are_not_recorded() {
        let notice = Reminder::once(key::NOTICE, "后台任务完成。".into());
        let known = BTreeMap::from([(key::NOTICE.to_owned(), digest("后台任务完成。"))]);

        let selected = pending(std::slice::from_ref(&notice), &known);
        assert_eq!(selected.len(), 1, "一次性条目相同文本也要重发");
        let (_, items) = render(&selected);
        assert!(items.is_empty(), "一次性条目不该被记下去重表");
    }
}
