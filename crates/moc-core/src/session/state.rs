//! 可重建的会话内存状态；只有完整 JSONL entry 才改变状态。

use moc_llm::{Role, Usage};
use moc_protocol::methods::{
    PlanInfo, SessionInfo, SessionMode, SessionPermission, TodoPhase, TranscriptEntry,
};

use crate::error::CoreError;

use super::entries::Entry;
use super::entries::PtcStateChange;
use std::{collections::BTreeMap, sync::Arc};

pub(crate) fn ptc_value_bytes(key: &str, value: &serde_json::Value) -> usize {
    key.len()
        + serde_json::to_vec(value)
            .expect("JSON Value 可序列化")
            .len()
}

pub(crate) fn add_usage(total: &mut Usage, usage: Usage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = Some(total.input_tokens.saturating_add(total.output_tokens));
    total.cached_input_tokens = add_optional(total.cached_input_tokens, usage.cached_input_tokens);
    total.cache_creation_input_tokens = add_optional(
        total.cache_creation_input_tokens,
        usage.cache_creation_input_tokens,
    );
    total.reasoning_tokens = add_optional(total.reasoning_tokens, usage.reasoning_tokens);
}

fn add_optional(current: Option<u64>, added: Option<u64>) -> Option<u64> {
    match (current, added) {
        (Some(current), Some(added)) => Some(current.saturating_add(added)),
        (Some(current), None) => Some(current),
        (None, Some(added)) => Some(added),
        (None, None) => None,
    }
}

/// 当前会话中每个模型的调用累计；属于运行态，随会话生命周期保留。
#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub calls: u64,
    pub usage: Usage,
    pub active_ms: u64,
}

/// 一个会话的 transcript 与当前投影。
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session: SessionInfo,
    pub entries: Vec<Entry>,
    pub transcript: Vec<TranscriptEntry>,
    /// 与 transcript 一一对应的 token 估算，避免上下文面板重复扫描正文。
    pub transcript_token_counts: Vec<u64>,
    pub mode: SessionMode,
    pub permission: SessionPermission,
    pub model: String,
    pub todos: Vec<TodoPhase>,
    pub plans: Vec<PlanInfo>,
    pub child_calls: Vec<moc_protocol::methods::session::TranscriptChildCall>,
    pub ptc_state: BTreeMap<String, Arc<serde_json::Value>>,
    pub ptc_state_bytes: usize,
    pub summary: Option<(String, String)>,
    pub native_input: Option<Vec<serde_json::Value>>,
    pub first_kept_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub prompt_sequence: u64,
    pub job_sequence: u64,
    pub notices: Vec<String>,
    pub usage: Usage,
    pub model_usage: BTreeMap<String, ModelUsage>,
    pub usage_at: usize,
    pub server_compaction_disabled: bool,
    /// CLM 实验模式的当前工作上下文；原始 transcript 始终保留。
    pub clm_context: Option<String>,
    pub last_entry_id: String,
}

impl SessionState {
    /// 从刚写入的 header 建立会话状态。
    pub fn new(header: Entry) -> Result<Self, CoreError> {
        Self::from_entries(vec![header])
    }

    /// 重放并校验 JSONL 线性 parent 链。
    pub fn from_entries(entries: Vec<Entry>) -> Result<Self, CoreError> {
        let Some(first) = entries.first() else {
            return Err(CoreError::Session("transcript 缺少 header".into()));
        };
        let Entry::Header {
            version,
            parent_session_id,
            mode,
            permission,
            model,
            ..
        } = first
        else {
            return Err(CoreError::Session("transcript 第一条不是 header".into()));
        };
        if *version != 1 {
            return Err(CoreError::Session(format!(
                "不支持 transcript 版本 {version}"
            )));
        }
        let session = first
            .session_info(None)
            .ok_or_else(|| CoreError::Session("header 无法生成 session info".into()))?;
        let mut state = Self {
            session,
            entries: Vec::with_capacity(entries.len()),
            transcript: Vec::new(),
            transcript_token_counts: Vec::new(),
            mode: *mode,
            permission: *permission,
            model: model.clone(),
            todos: Vec::new(),
            plans: Vec::new(),
            child_calls: Vec::new(),
            ptc_state: BTreeMap::new(),
            ptc_state_bytes: 0,
            parent_session_id: parent_session_id.clone(),
            prompt_sequence: 0,
            job_sequence: 0,
            usage: Usage::default(),
            model_usage: BTreeMap::new(),
            usage_at: 0,
            server_compaction_disabled: false,
            summary: None,
            first_kept_id: None,
            native_input: None,
            clm_context: None,
            notices: Vec::new(),
            last_entry_id: first.id().to_owned(),
        };
        state.entries.push(first.clone());
        for entry in entries.into_iter().skip(1) {
            if entry.parent_id() != Some(state.last_entry_id.as_str()) {
                return Err(CoreError::Session(format!(
                    "entry {} 的 parent_id 与 transcript 链不匹配",
                    entry.id()
                )));
            }
            state.apply_entry(entry);
        }
        Ok(state)
    }

    /// 把已落盘 entry 投影到可读的会话状态。
    pub fn apply_entry(&mut self, entry: Entry) {
        self.session.updated_at_ms = self.session.updated_at_ms.max(entry.at_ms());
        match &entry {
            Entry::Header { .. } => {}
            Entry::Message { message, bash, .. } => {
                if message.role == Role::User && bash.is_none() {
                    self.prompt_sequence = self.prompt_sequence.saturating_add(1);
                }
                if let Some(projected) = entry.transcript_entry() {
                    let tokens = if !projected.in_context() {
                        0
                    } else if let Some(bash) = &projected.bash {
                        crate::compact::message_tokens(&bash.context_message(&projected.message))
                    } else {
                        crate::compact::message_tokens(&projected.message)
                    };
                    self.transcript_token_counts.push(tokens);
                    self.transcript.push(projected);
                }
            }
            Entry::Compaction {
                method,
                summary,
                native,
                first_kept_id,
                ..
            } => {
                self.usage = Usage::default();
                self.usage_at = 0;
                self.first_kept_id.clone_from(first_kept_id);
                self.native_input = native
                    .as_ref()
                    .and_then(|native| native.as_array())
                    .filter(|items| {
                        !items.is_empty()
                            && items.iter().all(|item| {
                                item.get("type")
                                    .and_then(serde_json::Value::as_str)
                                    .is_some()
                            })
                    })
                    .cloned();
                if method == "remote" && self.native_input.is_none() {
                    // 不可解码的载荷不能成为截断历史的依据。
                    self.summary = None;
                    self.first_kept_id = None;
                } else {
                    self.summary = Some((method.clone(), summary.clone()));
                }
            }
            Entry::ResetBoundary { .. } => {
                self.ptc_state.clear();
                self.ptc_state_bytes = 0;
                self.summary = None;
                self.native_input = None;
                self.first_kept_id = None;
                self.clm_context = None;
                self.usage = Usage::default();
                self.usage_at = 0;
            }
            Entry::ClmContext { content, .. } => {
                self.clm_context = Some(content.clone());
            }
            Entry::ModeChange { mode, .. } => {
                let mode_changed = self.mode != *mode;
                self.mode = *mode;
                self.session.mode = *mode;
                if mode_changed || *mode != SessionMode::ClmExperimental {
                    self.clm_context = None;
                }
            }
            Entry::PermissionChange { permission, .. } => {
                self.permission = *permission;
                self.session.permission = *permission;
            }
            Entry::ModelChange { model, .. } => {
                self.model.clone_from(model);
                self.session.model.clone_from(model);
                self.native_input = None;
                if self
                    .summary
                    .as_ref()
                    .is_some_and(|(method, _)| method == "remote")
                {
                    // 加密 checkpoint 不可跨模型使用；恢复日志而非仅留下占位摘要。
                    self.summary = None;
                    self.first_kept_id = None;
                }
                self.usage = Usage::default();
                self.usage_at = 0;
                self.server_compaction_disabled = false;
            }
            Entry::TitleChange { title, .. } => {
                self.session.title.clone_from(title);
            }
            Entry::Todos { phases, .. } => {
                self.todos.clone_from(phases);
            }
            Entry::Plan {
                plan_id,
                title,
                path,
                markdown,
                status,
                ..
            } => {
                let info = PlanInfo {
                    plan_id: plan_id.clone(),
                    title: title.clone(),
                    path: path.clone(),
                    markdown: markdown.clone(),
                    status: *status,
                };
                if let Some(existing) = self.plans.iter_mut().find(|plan| plan.plan_id == *plan_id)
                {
                    *existing = info;
                } else {
                    self.plans.push(info);
                }
            }
            Entry::ChildTool { call, .. } => self.child_calls.push(call.clone()),
            Entry::PtcState { change, .. } => match change {
                PtcStateChange::Put { key, value } => {
                    if let Some(old) = self.ptc_state.insert(key.clone(), Arc::new(value.clone())) {
                        self.ptc_state_bytes -= ptc_value_bytes(key, &old);
                    }
                    self.ptc_state_bytes += ptc_value_bytes(key, value);
                }
                PtcStateChange::Delete { key } => {
                    if let Some(old) = self.ptc_state.remove(key) {
                        self.ptc_state_bytes -= ptc_value_bytes(key, &old);
                    }
                }
            },
            Entry::Usage {
                model,
                usage,
                active_ms,
                ..
            } => {
                let model_usage = self.model_usage.entry(model.clone()).or_default();
                model_usage.calls = model_usage.calls.saturating_add(1);
                add_usage(&mut model_usage.usage, *usage);
                model_usage.active_ms = model_usage.active_ms.saturating_add(*active_ms);
            }
        }
        self.last_entry_id = entry.id().to_owned();
        self.entries.push(entry);
    }

    /// 最新持久化视图的 session 元信息。
    pub fn session_info(&self) -> SessionInfo {
        self.session.clone()
    }
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{PlanStatus, SessionMode};

    use super::*;
    use crate::session::entries::EntryPayload;

    #[test]
    fn replay_restores_metadata_todos_plans_and_transcript() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model:high".into(),
            SessionMode::Normal,
            None,
            "标题".into(),
            10,
        );
        let header_id = header.id().to_owned();
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("你好"),
                tool: None,
                finish_reason: None,
            },
            header_id,
            11,
        );
        let message_id = message.id().to_owned();
        let todo = Entry::from_payload(EntryPayload::Todos { phases: Vec::new() }, message_id, 12);
        let todo_id = todo.id().to_owned();
        let plan = Entry::from_payload(
            EntryPayload::Plan {
                plan_id: "plan_12345678".into(),
                title: "计划".into(),
                path: "/tmp/plan.md".into(),
                markdown: "# 计划".into(),
                status: PlanStatus::Approved,
            },
            todo_id,
            13,
        );
        let mode = Entry::from_payload(
            EntryPayload::ModeChange {
                mode: SessionMode::Plan,
            },
            plan.id().to_owned(),
            14,
        );

        let state = SessionState::from_entries(vec![header, message, todo, plan, mode])
            .expect("记录链完整");

        assert_eq!(state.session.title, "标题");
        assert_eq!(state.session.mode, SessionMode::Plan);
        assert_eq!(state.transcript.len(), 1);
        assert!(matches!(
            &state.transcript[0].message.content[0],
            moc_llm::ContentPart::Text(text) if text == "你好"
        ));
        assert_eq!(state.plans[0].status, PlanStatus::Approved);
        assert_eq!(state.session.updated_at_ms, 14);
        assert_eq!(state.prompt_sequence, 1);
    }

    #[test]
    fn replay_restores_ptc_state_and_reset_clears_projection() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionMode::Normal,
            None,
            "标题".into(),
            10,
        );
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("run"),
                tool: None,
                finish_reason: None,
            },
            header.id().to_owned(),
            11,
        );
        let put = Entry::from_payload(
            EntryPayload::PtcState {
                parent_tool_call_id: "call".into(),
                change: PtcStateChange::Put {
                    key: "x".into(),
                    value: serde_json::json!({"n":1}),
                },
            },
            message.id().to_owned(),
            12,
        );
        let delete = Entry::from_payload(
            EntryPayload::PtcState {
                parent_tool_call_id: "call".into(),
                change: PtcStateChange::Delete {
                    key: "missing".into(),
                },
            },
            put.id().to_owned(),
            13,
        );
        let reset = Entry::from_payload(EntryPayload::ResetBoundary, delete.id().to_owned(), 14);
        let state = SessionState::from_entries(vec![header, message, put, delete, reset])
            .expect("状态日志可回放");
        assert!(state.ptc_state.is_empty());
        assert_eq!(state.ptc_state_bytes, 0);
    }

    #[test]
    fn replay_accumulates_model_usage() {
        let header = Entry::header(
            "ses_usage123456".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionMode::Normal,
            None,
            "标题".into(),
            10,
        );
        let first = Entry::from_payload(
            EntryPayload::Usage {
                model: "p/model".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_input_tokens: Some(40),
                    reasoning_tokens: Some(8),
                    ..Usage::default()
                },
                active_ms: 500,
            },
            header.id().to_owned(),
            11,
        );
        let second = Entry::from_payload(
            EntryPayload::Usage {
                model: "p/model".into(),
                usage: Usage {
                    input_tokens: 60,
                    output_tokens: 10,
                    cached_input_tokens: Some(20),
                    reasoning_tokens: Some(4),
                    ..Usage::default()
                },
                active_ms: 250,
            },
            first.id().to_owned(),
            12,
        );
        let state = SessionState::from_entries(vec![header, first, second]).expect("用量日志有效");
        let summary = state.model_usage.get("p/model").expect("模型统计存在");
        assert_eq!(summary.calls, 2);
        assert_eq!(summary.usage.input_tokens, 160);
        assert_eq!(summary.usage.output_tokens, 30);
        assert_eq!(summary.usage.cached_input_tokens, Some(60));
        assert_eq!(summary.usage.reasoning_tokens, Some(12));
        assert_eq!(summary.active_ms, 750);
    }

    #[test]
    fn replay_rejects_broken_parent_chains() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionMode::Normal,
            None,
            "标题".into(),
            10,
        );
        let entry = Entry::from_payload(EntryPayload::ResetBoundary, "wrong-parent".into(), 11);
        assert!(SessionState::from_entries(vec![header, entry]).is_err());
    }
}
