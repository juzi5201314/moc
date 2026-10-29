//! TUI 子代理视图：侧栏与名册用的状态快照。
//!
//! 子代理的转写不在 TUI 里另存一份：`Ctrl+N` 名册里 `Enter` 把主 TUI 聚焦到该
//! 子代理的会话，转写直接走主滚动区（见 `docs/tui.md` 的子代理一节）。

use moc_protocol::methods::{AgentInfo, AgentStatus};

/// 一个子代理的状态快照。
#[derive(Debug, Clone)]
pub struct SubagentView {
    pub info: AgentInfo,
    /// 是否仍在运行（侧栏的 ● / ○，以及名册里的排序依据）。
    pub running: bool,
}

fn live(status: AgentStatus) -> bool {
    matches!(status, AgentStatus::Queued | AgentStatus::Running)
}

impl SubagentView {
    pub fn new(info: AgentInfo) -> Self {
        let running = live(info.status);
        Self { info, running }
    }

    /// 状态更新：`last_message` 只在有值时覆盖（终态带回结果预览）。
    pub fn set_status(&mut self, status: AgentStatus, last_message: Option<String>) {
        self.info.status = status;
        self.info.updated_at_ms = crate::transcript::now_ms();
        self.running = live(status);
        if last_message.is_some() {
            self.info.last_message = last_message;
        }
    }

    pub fn status_label(&self) -> &'static str {
        crate::render::scrollback::agent_status_label(self.info.status)
    }
}

/// 非模态异步操作的用户可见提示。
#[derive(Debug, Clone)]
pub struct AgentOperationNotice {
    /// 投递目标（代理 id、`parent` 或 `all`）。
    pub target: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> AgentInfo {
        AgentInfo {
            id: "ses_child".into(),
            parent_id: "ses_parent".into(),
            title: "child".into(),
            model: "test/model".into(),
            cwd: "/tmp".into(),
            preset: None,
            status: AgentStatus::Running,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_message: None,
        }
    }

    #[test]
    fn queued_agent_counts_as_running() {
        let mut info = info();
        info.status = AgentStatus::Queued;
        assert!(SubagentView::new(info).running);
    }

    #[test]
    fn status_update_changes_running_card() {
        let mut view = SubagentView::new(info());
        view.set_status(AgentStatus::Completed, Some("done".into()));
        assert!(!view.running);
        assert_eq!(view.status_label(), "已完成");
        assert_eq!(view.info.last_message.as_deref(), Some("done"));
    }

    #[test]
    fn status_update_keeps_the_previous_preview_when_absent() {
        let mut view = SubagentView::new(info());
        view.set_status(AgentStatus::Completed, Some("done".into()));
        view.set_status(AgentStatus::Running, None);
        assert!(view.running);
        assert_eq!(view.info.last_message.as_deref(), Some("done"));
    }
}
