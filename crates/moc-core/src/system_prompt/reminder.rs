//! 运行时提醒条目：决定哪些事实要进历史、什么时候进。
//!
//! 提醒是**条目化**的：每个事实有自己的稳定 key，注入前按内容摘要与历史里已有的
//! 那份比较，只有变化了才写一条新的进去。写进去的提醒成为历史的一部分，因此可以
//! 被缓存，也不会在后续每个请求里重复付费。
//!
//! 后台任务完成通过会话 notice 告知模型（见 `tools/job.rs`），这里不重复。

use std::collections::BTreeMap;
use std::process::Command;

use moc_llm::Message;
use moc_protocol::ReminderItem;

/// 条目的稳定键。跨会话重启、跨版本都不能改，否则历史里的旧条目会失去对应。
pub mod key {
    pub const SANDBOX: &str = "sandbox";
    pub const ENGINE: &str = "engine";
    pub const MODE: &str = "mode";
    pub const MCP_INDEX: &str = "mcp-index";
    /// MCP server 状态与工具集合快照（正常基线静默记录，异常或变更时注入）。
    pub const MCP_STATE: &str = "mcp-state";
    pub const ROSTER: &str = "roster";
    pub const TODO_STALL: &str = "todo-stall";
    pub const LOCAL_TIME: &str = "local-time";
    pub const CYCLE: &str = "cycle";
    pub const THINKING: &str = "thinking";
    pub const NOTICE: &str = "notice";
    /// 持久目标上下文（目标存在期间每轮注入）。
    pub const GOAL: &str = "goal";
}

/// 条目的注入策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderKind {
    /// 持久条目：内容不变就不再注入。
    Durable,
    /// 一次性条目：每次出现都注入，且不写入去重表——相同的文字也可能是新事实。
    Once,
    /// 撤回条目：只在历史里已注入过同 key 条目时才注入，用来声明旧条目失效；
    /// 它本身记入去重表，所以同一份撤回只写一次。条目不再产生时旧提醒仍留在
    /// 历史里，撤回项是把「没有新条目」这个事实讲给模型听的方式。
    Retract,
    /// 基线跟踪条目：历史里尚无此 key 时仅静默记录摘要、不向模型发送正文；
    /// 已有历史摘要且发生变化时，注入正文并更新摘要。
    Baseline,
}

/// 一个待注入的运行时提醒条目。
#[derive(Debug, Clone)]
pub struct Reminder {
    /// 稳定键；同一个键的新条目覆盖旧条目。
    pub key: &'static str,
    /// 条目正文，不含 `<system-reminder>` 外壳。
    pub text: String,
    /// 注入策略。
    pub kind: ReminderKind,
    /// 可选的去重指纹覆盖；`None` 时直接对 `text` 计算摘要。
    pub fingerprint: Option<String>,
}

impl Reminder {
    /// 一个持久条目：内容不变就不再注入。
    pub fn durable(key: &'static str, text: String) -> Self {
        Self {
            key,
            text,
            kind: ReminderKind::Durable,
            fingerprint: None,
        }
    }

    /// 带独立去重指纹的持久条目：指纹不变则不重发，正文保持紧凑展示。
    pub fn durable_with_fingerprint(key: &'static str, text: String, fingerprint: String) -> Self {
        Self {
            key,
            text,
            kind: ReminderKind::Durable,
            fingerprint: Some(fingerprint),
        }
    }

    /// 带独立去重指纹的基线跟踪条目：首次静默记基线，后续指纹变化时注入。
    pub fn baseline_with_fingerprint(key: &'static str, text: String, fingerprint: String) -> Self {
        Self {
            key,
            text,
            kind: ReminderKind::Baseline,
            fingerprint: Some(fingerprint),
        }
    }

    /// 一个一次性条目：每次都参与注入，且不写入去重表。
    pub fn once(key: &'static str, text: String) -> Self {
        Self {
            key,
            text,
            kind: ReminderKind::Once,
            fingerprint: None,
        }
    }

    /// 一个撤回条目：该 key 以前注入过内容时才注入，声明旧条目失效。
    pub fn retract(key: &'static str, text: impl Into<String>) -> Self {
        Self {
            key,
            text: text.into(),
            kind: ReminderKind::Retract,
            fingerprint: None,
        }
    }

    /// 用于去重表比对的条目摘要。
    pub fn item_digest(&self) -> String {
        digest(self.fingerprint.as_deref().unwrap_or(&self.text))
    }
}

/// 条目正文的摘要，用于判断「这条是否已经在历史里」。
pub fn digest(text: &str) -> String {
    format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes()))
}

/// 提醒筛选结果：区分为待注入正文与仅静默记入去重表的基线。
#[derive(Debug, Default)]
pub struct Selection<'a> {
    pub emit: Vec<&'a Reminder>,
    pub record_only: Vec<&'a Reminder>,
}

/// 从候选条目里挑出需要注入或静默记基线的条目。
///
/// 一次性条目总是入选；持久条目在从未注入或摘要变化时入选；撤回条目只在
/// 历史里已有同 key 条目且还没撤回过时入选；基线条目在首次出现时静默记录
/// 摘要，后续摘要变化时才注入正文。
pub fn select<'a>(
    candidates: &'a [Reminder],
    known: &BTreeMap<String, String>,
) -> Selection<'a> {
    let mut selection = Selection::default();
    for reminder in candidates {
        match reminder.kind {
            ReminderKind::Once => selection.emit.push(reminder),
            ReminderKind::Durable => {
                if known.get(reminder.key) != Some(&reminder.item_digest()) {
                    selection.emit.push(reminder);
                }
            }
            ReminderKind::Retract => {
                if known
                    .get(reminder.key)
                    .is_some_and(|prev| *prev != reminder.item_digest())
                {
                    selection.emit.push(reminder);
                }
            }
            ReminderKind::Baseline => match known.get(reminder.key) {
                None => selection.record_only.push(reminder),
                Some(prev) if *prev != reminder.item_digest() => selection.emit.push(reminder),
                Some(_) => {}
            },
        }
    }
    selection
}

/// 把选中的条目渲染成一条 [`Role::Custom`] 消息，并给出要记进历史的条目摘要。
///
/// 每个条目各自包一层 `<system-reminder>`：模型据此区分提醒与用户输入，人也认得。
/// 角色本身已经是 `Custom`，所以重放与压缩不必再嗅探这个前缀。条目之间空一行。
/// 自带系统级标签的条目（如 `<system-interrupt>`）不再套壳，保留原标签语义。
/// 静默基线条目不进入消息正文，但其摘要一并写入 `items` 落盘。
pub fn render(selection: &Selection<'_>) -> (Message, Vec<ReminderItem>) {
    let text = selection
        .emit
        .iter()
        .map(|reminder| wrap(&reminder.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    let items = selection
        .emit
        .iter()
        .filter(|reminder| reminder.kind != ReminderKind::Once)
        .chain(selection.record_only.iter())
        .map(|reminder| ReminderItem {
            key: reminder.key.to_owned(),
            digest: reminder.item_digest(),
        })
        .collect();
    (Message::custom(text), items)
}

/// 提醒正文的外壳：自带 `<system-*` 标签的保留原标签，其余套 `<system-reminder>`。
///
/// `Role::Custom` 在上游不支持 `developer` 角色时降级为 `user` 下发，这层标签是
/// 模型区分注入内容与真实用户输入的唯一依据；不经 `render` 直发提醒的地方也必须
/// 过这层壳。
pub fn wrap(text: &str) -> String {
    if text.starts_with("<system-") {
        text.to_owned()
    } else {
        format!("<system-reminder>{text}</system-reminder>")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReminderState {
    last_day: Option<String>,
    /// 上次注入的提醒正文：同一提示里压缩清空去重表后，要靠它把条目补回历史。
    last_text: Option<String>,
}

impl ReminderState {
    /// 本地日期变化时返回时间提醒正文，否则返回 `None`。
    pub fn render(&mut self, now_local: &str) -> Option<String> {
        let day = now_local.get(..10).unwrap_or(now_local);
        if self.last_day.as_deref() == Some(day) {
            return None;
        }
        self.last_day = Some(day.to_owned());
        let text = format!("Current local time: {now_local}.");
        self.last_text = Some(text.clone());
        Some(text)
    }

    /// 上次注入过的提醒正文；还没注入过为 `None`。
    pub fn last_text(&self) -> Option<&str> {
        self.last_text.as_deref()
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

        let first = select(&candidates, &BTreeMap::new());
        assert_eq!(first.emit.len(), 2);
        let (message, items) = render(&first);
        assert_eq!(items.len(), 2);
        assert!(message.text().contains("<system-reminder>Sandbox mode"));
        assert!(message.text().contains("<system-reminder>Ask mode"));

        let next = select(&candidates, &known_of(items));
        assert!(next.emit.is_empty(), "内容没变就不该再注入");
        assert!(next.record_only.is_empty());
    }

    #[test]
    fn only_the_changed_entry_is_pending() {
        let mode = Reminder::durable(key::MODE, "Ask mode is active.".into());
        let known = known_of(
            render(&select(
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
        let changed = select(&changed_entries, &known);
        assert_eq!(changed.emit.len(), 1, "只有沙盒变了");
        assert_eq!(changed.emit[0].key, key::SANDBOX);
        let (message, items) = render(&changed);
        assert!(message.text().contains("Sandbox mode: full"));
        assert!(!message.text().contains("Ask mode"), "未变的模式不该重发");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn ephemeral_entries_always_inject_and_are_not_recorded() {
        let notice = Reminder::once(key::NOTICE, "后台任务完成。".into());
        let known = BTreeMap::from([(key::NOTICE.to_owned(), digest("后台任务完成。"))]);

        let selected = select(std::slice::from_ref(&notice), &known);
        assert_eq!(selected.emit.len(), 1, "一次性条目相同文本也要重发");
        let (_, items) = render(&selected);
        assert!(items.is_empty(), "一次性条目不该被记下去重表");
    }

    #[test]
    fn retraction_fires_once_and_yields_to_a_returning_entry() {
        let durable = || Reminder::durable(key::ROSTER, "Peers: alpha.".into());
        let retract = || Reminder::retract(key::ROSTER, "The earlier roster no longer applies.");

        let candidates = [retract()];
        assert!(
            select(&candidates, &BTreeMap::new()).emit.is_empty(),
            "历史里没有同 key 条目时撤回无事可撤"
        );

        let candidates = [durable()];
        let known = known_of(render(&select(&candidates, &BTreeMap::new())).1);
        let candidates = [retract()];
        let selected = select(&candidates, &known);
        assert_eq!(selected.emit.len(), 1, "条目消失后撤回一次");
        let (_, items) = render(&selected);
        assert_eq!(items.len(), 1, "撤回本身记入去重表");
        let known = known_of(items);

        assert!(
            select(&candidates, &known).emit.is_empty(),
            "撤回已记录就不再重复注入"
        );
        let candidates = [durable()];
        assert_eq!(
            select(&candidates, &known).emit.len(),
            1,
            "条目恢复时重新注入"
        );
    }

    #[test]
    fn baseline_records_silently_then_emits_only_on_fingerprint_change() {
        let base_other = Reminder::durable(key::SANDBOX, "Sandbox mode: full.".into());
        let ok_v1 = Reminder::baseline_with_fingerprint(
            key::MCP_STATE,
            "MCP servers updated: tavily (connected, 5 tools).".into(),
            "tavily\tconnected\ta,b,c,d,e".into(),
        );
        let first_candidates = [base_other.clone(), ok_v1.clone()];
        let first = select(&first_candidates, &BTreeMap::new());
        assert_eq!(first.emit.len(), 1, "首轮正常基线不发送正文");
        assert_eq!(first.emit[0].key, key::SANDBOX);
        assert_eq!(first.record_only.len(), 1);
        assert_eq!(first.record_only[0].key, key::MCP_STATE);
        let (msg, items) = render(&first);
        assert!(!msg.text().contains("tavily"), "静默基线不能出现在消息正文");
        assert_eq!(items.len(), 2, "静默基线摘要应随本轮提醒一起落盘");
        let known = known_of(items);

        // 指纹不变（如进程重启后同状态）：不发送也不再记录。
        let same_candidates = [base_other.clone(), ok_v1];
        let same = select(&same_candidates, &known);
        assert!(same.emit.is_empty());
        assert!(same.record_only.is_empty());

        // 工具名变化（即使数量仍是 5、展示正文相同）：指纹变化，必须注入。
        let ok_v2 = Reminder::baseline_with_fingerprint(
            key::MCP_STATE,
            "MCP servers updated: tavily (connected, 5 tools).".into(),
            "tavily\tconnected\ta,b,c,d,f".into(),
        );
        let changed_candidates = [base_other, ok_v2.clone()];
        let changed = select(&changed_candidates, &known);
        assert_eq!(changed.emit.len(), 1);
        assert_eq!(changed.emit[0].key, key::MCP_STATE);
        let (msg2, items2) = render(&changed);
        assert!(msg2.text().contains("tavily (connected, 5 tools)"));
        let mut known2 = known;
        known2.extend(known_of(items2));
        let settled_candidates = [ok_v2];
        assert!(select(&settled_candidates, &known2).emit.is_empty());
    }

    #[test]
    fn system_tagged_text_is_not_wrapped_again() {
        let interrupt = Reminder::once(
            key::CYCLE,
            "<system-interrupt reason=\"tool_loop\">stop.</system-interrupt>".into(),
        );
        let selected = select(std::slice::from_ref(&interrupt), &BTreeMap::new());
        let (message, _) = render(&selected);
        let text = message.text();
        assert!(text.starts_with("<system-interrupt"));
        assert!(!text.contains("<system-reminder>"));
    }
}
