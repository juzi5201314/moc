//! 可以重建的会话内存状态；只有完整的 JSONL entry 才会改变它。

use moc_llm::{Role, Usage};
use moc_protocol::methods::{
    PlanInfo, PlanStatus, SessionEngine, SessionInfo, SessionPermission, TodoPhase, TranscriptEntry,
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

/// 当前会话里每个模型的调用累计；属于运行状态，随会话生命周期保留。
#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub calls: u64,
    pub usage: Usage,
    pub active_ms: u64,
    /// 首 token 等待总时长；只统计上报了首 token 的请求。
    pub ttft_ms: u64,
    /// 上报了首 token 的请求数。
    pub ttft_calls: u64,
    /// 首 token 之后的生成总时长。
    pub gen_ms: u64,
}

impl ModelUsage {
    /// 把另一份累计用量逐项加进来。
    pub fn merge(&mut self, other: &ModelUsage) {
        self.calls = self.calls.saturating_add(other.calls);
        add_usage(&mut self.usage, other.usage);
        self.active_ms = self.active_ms.saturating_add(other.active_ms);
        self.ttft_ms = self.ttft_ms.saturating_add(other.ttft_ms);
        self.ttft_calls = self.ttft_calls.saturating_add(other.ttft_calls);
        self.gen_ms = self.gen_ms.saturating_add(other.gen_ms);
    }
}

/// 一个会话的 transcript 和当前状态。
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session: SessionInfo,
    pub entries: Vec<Entry>,
    pub transcript: Vec<TranscriptEntry>,
    /// 与 transcript 一一对应的 token 估算，免得上下文面板反复扫描正文。
    pub transcript_token_counts: Vec<u64>,
    pub engine: SessionEngine,
    /// plan 模式：与引擎正交的持久执行模式，回放自 `PlanMode` / `EngineChange` entry。
    pub plan_active: bool,
    pub permission: SessionPermission,
    pub model: String,
    /// plan 模式临时覆盖前的会话模型，退出 plan 时恢复。它随 transcript 重放，
    /// 会话恢复后依然有效；只有处于 plan 时才有意义。
    plan_base_model: Option<String>,
    pub todos: Vec<TodoPhase>,
    /// 自上次 todo 变更以来的工具调用数，供停滞提醒计数。
    pub tool_calls_since_todo: u32,
    /// 本次提示已发出的 todo 提醒数。
    pub todo_reminders_sent: u32,
    pub plans: Vec<PlanInfo>,
    pub child_calls: Vec<moc_protocol::methods::session::TranscriptChildCall>,
    pub ptc_state: BTreeMap<String, Arc<serde_json::Value>>,
    pub ptc_state_bytes: usize,
    pub summary: Option<(String, String)>,
    /// 当前 checkpoint 的生成模型；原生数据只对这个模型有效。
    pub summary_model: Option<String>,
    /// Grok Build 整体替换历史时附带的运行时状态提醒；其它 checkpoint 为 `None`。
    pub summary_reminder: Option<String>,
    pub native_input: Option<Vec<serde_json::Value>>,
    pub first_kept_id: Option<String>,
    pub prompt_sequence: u64,
    pub job_sequence: u64,
    pub notices: Vec<String>,
    /// 已经写进历史的运行时提醒：条目 key → 正文摘要。`None` 表示该条目从未注入。
    ///
    /// 由 transcript 重放得到，所以会话恢复后不会重复注入。压缩会重写可见历史，
    /// 那时整表清空，让下一次请求补一份全量的。
    pub reminder_digests: BTreeMap<String, String>,
    pub usage: Usage,
    pub model_usage: BTreeMap<String, ModelUsage>,
    /// 工具执行耗时之和（毫秒）；手动 bash 与嵌套子调用不计入。
    pub tool_ms: u64,
    pub usage_at: usize,
    pub server_compaction_disabled: bool,
    /// CLM 实验模式当前的工作上下文；原始 transcript 始终保留。
    pub clm_context: Option<String>,
    /// spawn 时快照的 agent preset 契约；普通会话与普通子代理为 `None`。
    pub agent: Option<super::entries::AgentSessionMeta>,
    pub last_entry_id: String,
}

impl SessionState {
    /// 从刚写入的 header 建立会话状态。
    pub fn new(header: Entry) -> Result<Self, CoreError> {
        Self::from_entries(vec![header])
    }

    /// 重放 JSONL，并校验 parent 链是否连续。
    pub fn from_entries(entries: Vec<Entry>) -> Result<Self, CoreError> {
        let Some(first) = entries.first() else {
            return Err(CoreError::Session("transcript 缺少 header".into()));
        };
        let Entry::Header {
            version,
            engine,
            permission,
            model,
            agent,
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
            engine: engine.engine(),
            plan_active: engine.legacy_enters_plan(),
            permission: *permission,
            model: model.clone(),
            // 以 plan 创建（旧持久模式或会话恢复）时，header 记录的就是会话自身模型。
            plan_base_model: engine.legacy_enters_plan().then(|| model.clone()),
            todos: Vec::new(),
            tool_calls_since_todo: 0,
            todo_reminders_sent: 0,
            plans: Vec::new(),
            child_calls: Vec::new(),
            ptc_state: BTreeMap::new(),
            ptc_state_bytes: 0,
            prompt_sequence: 0,
            job_sequence: 0,
            usage: Usage::default(),
            model_usage: BTreeMap::new(),
            tool_ms: 0,
            usage_at: 0,
            server_compaction_disabled: false,
            summary: None,
            summary_model: None,
            summary_reminder: None,
            first_kept_id: None,
            native_input: None,
            clm_context: None,
            agent: agent.clone(),
            notices: Vec::new(),
            reminder_digests: BTreeMap::new(),
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

    /// 把已写入的 entry 应用到会话状态。
    pub fn apply_entry(&mut self, entry: Entry) {
        self.session.updated_at_ms = self.session.updated_at_ms.max(entry.at_ms());
        match &entry {
            Entry::Header { .. } => {}
            Entry::Message {
                message,
                tool,
                bash,
                agent,
                reminder,
                ..
            } => {
                // 提醒是模型内部状态，不是用户提示：算进 `prompt_sequence` 会让 prompt ID 错位。
                if message.role == Role::User
                    && bash.is_none()
                    && agent.is_none()
                    && reminder.is_none()
                {
                    self.prompt_sequence = self.prompt_sequence.saturating_add(1);
                    // 有了第一条用户提示，引擎随即锁定。
                    self.session.engine_locked = true;
                }
                if let Some(stamp) = reminder {
                    for item in &stamp.items {
                        self.reminder_digests
                            .insert(item.key.clone(), item.digest.clone());
                    }
                }
                // 工具耗时随 transcript 持久化；手动 bash 与嵌套子调用不计入。
                if bash.is_none()
                    && let Some(duration_ms) = tool.as_ref().and_then(|tool| tool.duration_ms)
                {
                    self.tool_ms = self.tool_ms.saturating_add(duration_ms);
                }
                if let Some(projected) = entry.transcript_entry() {
                    let tokens = projected
                        .context_message()
                        .map_or(0, |message| crate::compact::message_tokens(&message));
                    self.transcript_token_counts.push(tokens);
                    let is_tool = projected.message.role == Role::Tool;
                    self.transcript.push(projected);
                    if is_tool {
                        self.order_tool_group();
                    }
                }
            }
            Entry::Compaction {
                method,
                summary,
                native,
                first_kept_id,
                model,
                reminder,
                ..
            } => {
                self.usage = Usage::default();
                self.usage_at = 0;
                // 压缩重写了可见历史，旧提醒可能已经被摘要取代；清表让下一次请求补全量。
                self.reminder_digests.clear();
                self.first_kept_id.clone_from(first_kept_id);
                self.summary_model.clone_from(model);
                self.summary_reminder.clone_from(reminder);
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
                    // 无法解码的数据不能作为截断历史的依据。
                    self.summary = None;
                    self.first_kept_id = None;
                    self.summary_model = None;
                    self.summary_reminder = None;
                } else {
                    self.summary = Some((method.clone(), summary.clone()));
                }
            }
            Entry::ResetBoundary { .. } => {
                self.ptc_state.clear();
                self.ptc_state_bytes = 0;
                self.summary = None;
                self.summary_model = None;
                self.summary_reminder = None;
                self.native_input = None;
                self.first_kept_id = None;
                self.clm_context = None;
                self.usage = Usage::default();
                self.usage_at = 0;
                // 历史被重置，旧提醒不再生效。
                self.reminder_digests.clear();
            }
            Entry::ClmContext { content, .. } => {
                self.clm_context = Some(content.clone());
            }
            Entry::EngineChange {
                engine,
                plan_active,
                ..
            } => {
                // 新条目带权威 plan 投影；旧条目按持久模式值映射：`plan` 模式 =
                // Standard 引擎 + plan 模式，其余值 = 对应引擎且退出 plan。
                let next = engine.engine();
                let plan = plan_active.unwrap_or_else(|| engine.legacy_enters_plan());
                let engine_changed = self.engine != next;
                if plan && !self.plan_active {
                    // plan 模式只临时覆盖模型：记住进入前的会话模型，退出时恢复。
                    self.plan_base_model = Some(self.model.clone());
                }
                self.engine = next;
                self.session.engine = next;
                self.plan_active = plan;
                self.session.plan_active = plan;
                if engine_changed || next != SessionEngine::ClmExperimental {
                    self.clm_context = None;
                }
            }
            Entry::AutonomyChange { autonomy, .. } => {
                self.session.autonomy = *autonomy;
            }
            Entry::PlanMode { active, .. } => {
                if *active && !self.plan_active {
                    self.plan_base_model = Some(self.model.clone());
                }
                self.plan_active = *active;
                self.session.plan_active = *active;
            }
            Entry::PermissionChange { permission, .. } => {
                self.permission = *permission;
                self.session.permission = *permission;
            }
            Entry::ModelChange { model, .. } => {
                self.model.clone_from(model);
                self.session.model.clone_from(model);
                // 原生数据只对生成它的模型有效；如果 checkpoint 就是这个模型生成的（比如
                // 计划审批先把上下文压到执行模型再切换），必须保留，否则这次压缩会被悄悄作废。
                let foreign_checkpoint = self.summary_model.as_deref() != Some(model.as_str());
                if foreign_checkpoint {
                    self.native_input = None;
                    if self
                        .summary
                        .as_ref()
                        .is_some_and(|(method, _)| method == "remote")
                    {
                        // 加密的 checkpoint 不能跨模型使用；这时恢复日志视图，而不是只留一条占位摘要。
                        self.summary = None;
                        self.first_kept_id = None;
                        self.summary_model = None;
                        self.summary_reminder = None;
                    }
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
                version,
                previous_plan_id,
                ..
            } => {
                // 状态变更条目只带自己关心的字段：批注与「上一版」沿用已有投影，
                // 只有明确给出上一版时才覆盖，避免批准/拒绝之类的状态重写把它们冲掉。
                let existing = self.plans.iter().find(|plan| plan.plan_id == *plan_id);
                let annotations = existing
                    .map(|plan| plan.annotations.clone())
                    .unwrap_or_default();
                let previous_plan_id = previous_plan_id
                    .clone()
                    .or_else(|| existing.and_then(|plan| plan.previous_plan_id.clone()));
                let info = PlanInfo {
                    plan_id: plan_id.clone(),
                    title: title.clone(),
                    path: path.clone(),
                    markdown: markdown.clone(),
                    status: *status,
                    version: *version,
                    previous_plan_id,
                    annotations,
                };
                if let Some(existing) = self.plans.iter_mut().find(|plan| plan.plan_id == *plan_id)
                {
                    *existing = info;
                } else {
                    self.plans.push(info);
                }
            }
            Entry::PlanFeedback {
                plan_id,
                annotations,
                ..
            } => {
                if let Some(plan) = self.plans.iter_mut().find(|plan| plan.plan_id == *plan_id) {
                    plan.annotations.clone_from(annotations);
                    plan.status = PlanStatus::Revised;
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
                ttft_ms,
                ..
            } => {
                let model_usage = self.model_usage.entry(model.clone()).or_default();
                model_usage.calls = model_usage.calls.saturating_add(1);
                add_usage(&mut model_usage.usage, *usage);
                model_usage.active_ms = model_usage.active_ms.saturating_add(*active_ms);
                // 只有看到过首 token 的请求才能拆分网络等待与生成耗时；失败请求与
                // 非流式请求（压缩摘要）没有首 token，只计入总用时。
                if let Some(ttft_ms) = *ttft_ms {
                    let ttft_ms = ttft_ms.min(*active_ms);
                    model_usage.ttft_ms = model_usage.ttft_ms.saturating_add(ttft_ms);
                    model_usage.ttft_calls = model_usage.ttft_calls.saturating_add(1);
                    model_usage.gen_ms = model_usage
                        .gen_ms
                        .saturating_add(active_ms.saturating_sub(ttft_ms));
                }
            }
        }
        self.last_entry_id = entry.id().to_owned();
        self.entries.push(entry);
    }

    /// 把最近这组连续的工具结果按所属 assistant 的调用顺序排列。
    ///
    /// 工具结果在实际完成时就落盘（并行批次里先完成的先提交），物理顺序是完成顺序；
    /// 模型协议要求结果顺序与调用顺序一致，因此只在内存投影里整理，物理日志不动。
    fn order_tool_group(&mut self) {
        let end = self.transcript.len();
        let mut start = end - 1;
        while start > 0 && self.transcript[start - 1].message.role == Role::Tool {
            start -= 1;
        }
        let Some(owner) = start.checked_sub(1) else {
            return;
        };
        let order: Vec<&str> = self.transcript[owner]
            .message
            .tool_calls()
            .map(|call| call.id.as_str())
            .collect();
        if order.is_empty() {
            return;
        }
        let mut ranks = Vec::with_capacity(end - start);
        for entry in &self.transcript[start..end] {
            let Some(result) = entry.message.tool_results().next() else {
                return;
            };
            let Some(rank) = order.iter().position(|id| *id == result.tool_call_id) else {
                return;
            };
            ranks.push(rank);
        }
        if ranks.windows(2).all(|pair| pair[0] <= pair[1]) {
            return;
        }
        // 组内元素很少（一次并行批次），选择排序比建置换更直白。
        for offset in 0..ranks.len() {
            let mut best = offset;
            for candidate in offset + 1..ranks.len() {
                if ranks[candidate] < ranks[best] {
                    best = candidate;
                }
            }
            if best != offset {
                ranks.swap(offset, best);
                self.transcript.swap(start + offset, start + best);
                self.transcript_token_counts
                    .swap(start + offset, start + best);
            }
        }
    }

    /// 当前会话的最新元信息。
    pub fn session_info(&self) -> SessionInfo {
        self.session.clone()
    }

    /// 退出 plan 模式时要恢复的会话模型。
    ///
    /// plan 期间该值来自进入时的记录；不在 plan 时返回当前模型，供统一的恢复路径使用。
    pub fn exit_plan_model(&self) -> &str {
        self.plan_base_model
            .as_deref()
            .unwrap_or(self.model.as_str())
    }
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{PlanStatus, SessionAutonomy, SessionEngine};

    use super::*;
    use crate::session::entries::{EntryPayload, SessionOrigin};

    /// 工具结果按完成顺序落盘，模型投影必须回到调用顺序。
    #[test]
    fn out_of_order_tool_results_project_in_call_order() {
        use moc_llm::{ContentPart, Role, ToolCall};

        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        );
        let assistant = Entry::from_payload(
            EntryPayload::Message {
                message: Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::ToolCall(ToolCall {
                            id: "call_a".into(),
                            name: "read".into(),
                            arguments: "{}".into(),
                        }),
                        ContentPart::ToolCall(ToolCall {
                            id: "call_b".into(),
                            name: "read".into(),
                            arguments: "{}".into(),
                        }),
                    ],
                },
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().into(),
            11,
        );
        // 先完成的是第二个调用。
        let second = Entry::from_payload(
            EntryPayload::Message {
                message: Message::tool_result("call_b", "乙", false),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            assistant.id().into(),
            12,
        );
        let first = Entry::from_payload(
            EntryPayload::Message {
                message: Message::tool_result("call_a", "甲", false),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            second.id().into(),
            13,
        );

        let state =
            SessionState::from_entries(vec![header, assistant, second, first]).expect("记录链完整");

        let ids: Vec<&str> = state.transcript[1..]
            .iter()
            .flat_map(|entry| entry.message.tool_results())
            .map(|result| result.tool_call_id.as_str())
            .collect();
        assert_eq!(ids, ["call_a", "call_b"]);
        assert_eq!(state.transcript.len(), state.transcript_token_counts.len());
    }

    #[test]
    fn replay_restores_metadata_todos_plans_and_transcript() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model:high".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        );
        let header_id = header.id().to_owned();
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("你好"),
                tool: None,
                finish_reason: None,
                agent: None,
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
                version: 1,
                previous_plan_id: None,
            },
            todo_id,
            13,
        );
        let engine = Entry::from_payload(
            EntryPayload::EngineChange {
                engine: SessionEngine::Standard,
                plan_active: true,
            },
            plan.id().to_owned(),
            14,
        );
        let autonomy = Entry::from_payload(
            EntryPayload::AutonomyChange {
                autonomy: SessionAutonomy::Unattended,
            },
            engine.id().to_owned(),
            15,
        );

        let state = SessionState::from_entries(vec![header, message, todo, plan, engine, autonomy])
            .expect("记录链完整");

        assert_eq!(state.session.title, "标题");
        assert_eq!(state.session.engine, SessionEngine::Standard);
        assert_eq!(state.session.autonomy, SessionAutonomy::Unattended);
        assert!(state.plan_active);
        assert_eq!(state.transcript.len(), 1);
        assert!(matches!(
            &state.transcript[0].message.content[0],
            moc_llm::ContentPart::Text(text) if text == "你好"
        ));
        assert_eq!(state.plans[0].status, PlanStatus::Approved);
        assert_eq!(state.session.updated_at_ms, 15);
        assert_eq!(state.prompt_sequence, 1);
    }

    /// 计划状态重写不得冲掉批注与版本；`PlanFeedback` 只改批注并把状态推到修订中。
    #[test]
    fn replay_keeps_plan_version_and_annotations_across_status_rewrites() {
        fn entries(extra: Vec<EntryPayload>) -> Vec<Entry> {
            let header = Entry::header(
                "ses_123456789abc".into(),
                "/tmp/project".into(),
                "p/model".into(),
                SessionEngine::Standard,
                SessionPermission::Full,
                SessionOrigin::root(),
                "标题".into(),
                10,
            );
            let mut parent = header.id().to_owned();
            let mut at_ms = 11;
            let mut entries = vec![header];
            let mut push = |entries: &mut Vec<Entry>, payload: EntryPayload| {
                let entry = Entry::from_payload(payload, parent.clone(), at_ms);
                parent = entry.id().to_owned();
                at_ms += 1;
                entries.push(entry);
            };
            let plan_payload = |status, version| EntryPayload::Plan {
                plan_id: "plan_1".into(),
                title: "计划".into(),
                path: "local://plan_1.md".into(),
                markdown: "# 计划\n\n1. 先看代码\n".into(),
                status,
                version,
                previous_plan_id: None,
            };
            push(&mut entries, plan_payload(PlanStatus::Draft, 0));
            push(&mut entries, plan_payload(PlanStatus::Submitted, 1));
            push(
                &mut entries,
                EntryPayload::PlanFeedback {
                    plan_id: "plan_1".into(),
                    annotations: vec![moc_protocol::methods::PlanAnnotation {
                        id: "a1".into(),
                        start_line: 3,
                        end_line: 3,
                        quote: "先看代码".into(),
                        body: "具体到文件".into(),
                    }],
                    note: Some("整体保持简洁".into()),
                },
            );
            for payload in extra {
                push(&mut entries, payload);
            }
            entries
        }

        let state = SessionState::from_entries(entries(vec![EntryPayload::Plan {
            plan_id: "plan_1".into(),
            title: "计划".into(),
            path: "local://plan_1.md".into(),
            markdown: "# 计划\n\n1. 先看代码\n".into(),
            status: PlanStatus::Approved,
            version: 1,
            previous_plan_id: None,
        }]))
        .expect("记录链完整");
        assert_eq!(state.plans.len(), 1);
        assert_eq!(state.plans[0].status, PlanStatus::Approved);
        assert_eq!(state.plans[0].version, 1);
        assert_eq!(state.plans[0].annotations.len(), 1);
        assert_eq!(state.plans[0].annotations[0].body, "具体到文件");

        let revised = SessionState::from_entries(entries(Vec::new())).expect("记录链完整");
        assert_eq!(revised.plans[0].status, PlanStatus::Revised);
        assert_eq!(revised.plans[0].annotations.len(), 1);
    }

    #[test]
    fn replay_restores_ptc_state_and_reset_clears_projection() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        );
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("run"),
                tool: None,
                finish_reason: None,
                agent: None,
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
    fn replay_rejects_broken_parent_chains() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        );
        let entry = Entry::from_payload(EntryPayload::ResetBoundary, "wrong-parent".into(), 11);
        assert!(SessionState::from_entries(vec![header, entry]).is_err());
    }
}
