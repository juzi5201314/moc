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

/// 提示生命周期使用的控制状态；不持有正文、计划正文或工具结果。
#[derive(Debug, Clone)]
pub(crate) struct SessionMetadata {
    pub session: SessionInfo,
    pub engine: SessionEngine,
    pub plan_active: bool,
    pub workflow_activated: bool,
    pub permission: SessionPermission,
    pub model: String,
    pub prompt_sequence: u64,
    pub has_history: bool,
    pub tool_policy: Option<crate::agents::ToolPolicy>,
}

impl SessionMetadata {
    pub fn workflows_active(&self, config: &crate::config::WorkflowConfig) -> bool {
        crate::workflow::enabled_for(self.workflow_activated, self.session.is_subagent(), config)
    }
}

/// 一个会话的 transcript 和当前状态。
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session: SessionInfo,
    pub entries: Arc<Vec<super::stored::StoredEntry>>,
    pub transcript: Arc<Vec<Arc<TranscriptEntry>>>,
    transcript_log_indices: Arc<Vec<usize>>,
    /// 稀疏消息 ID 到当前投影下标；工具重排和重放同步维护。
    transcript_positions: Arc<std::collections::HashMap<String, usize>>,
    /// 旧日志若重复使用 ID，保留原来的末次匹配语义，不依赖唯一键索引。
    transcript_ids_unique: bool,
    /// 与 transcript 一一对应的 token 估算，免得上下文面板反复扫描正文。
    pub transcript_token_counts: Arc<Vec<u64>>,
    pub(super) unfrozen_bytes: usize,
    pub engine: SessionEngine,
    /// plan 模式：与引擎正交的持久执行模式，回放自 `PlanMode` / `EngineChange` entry。
    pub plan_active: bool,
    /// workflow 工具面是否激活（`session/workflow_activate` 后持久化恢复）。
    pub workflow_activated: bool,
    pub permission: SessionPermission,
    pub model: String,
    pub todos: Vec<TodoPhase>,
    /// 自上次 todo 变更以来的工具调用数，供停滞提醒计数。
    pub tool_calls_since_todo: u32,
    /// 本次提示已发出的 todo 提醒数。
    pub todo_reminders_sent: u32,
    pub plans: Arc<Vec<PlanInfo>>,
    pub child_calls: Arc<Vec<moc_protocol::methods::session::TranscriptChildCall>>,
    child_calls_by_parent: Arc<std::collections::HashMap<String, Vec<usize>>>,
    pub ptc_state: BTreeMap<String, Arc<serde_json::Value>>,
    pub ptc_state_bytes: usize,
    pub summary: Option<(String, String)>,
    /// 当前 checkpoint 的生成模型。
    pub summary_model: Option<String>,
    /// 全替换算法附带的运行时状态提醒；其它 checkpoint 为 `None`。
    pub summary_reminder: Option<String>,
    pub first_kept_id: Option<String>,
    /// 与 transcript 索引同口径的上下文边界；回放/重写时重建。
    pub(crate) context_start: usize,
    pub(crate) reset_start: usize,
    pub(crate) reasoning_start: usize,
    pub(crate) compact_end: usize,
    pub(crate) has_reset: bool,
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
    /// CLM 实验模式当前的工作上下文；原始 transcript 始终保留。
    pub clm_context: Option<String>,
    /// spawn 时快照的 agent preset 契约；普通会话与普通子代理为 `None`。
    pub agent: Option<super::entries::AgentSessionMeta>,
    /// 当前持久目标快照；无目标或已清除为 `None`（回放自 `GoalChange` 条目）。
    pub goal: Option<super::entries::GoalSnapshot>,
    /// 当前目标已落库的续跑轮数；只统计 `goal_id` 匹配当前目标的 Message.goal 条目。
    pub goal_rounds_started: u32,
    pub last_entry_id: String,
}

impl SessionState {
    /// 重写、派生或导出日志时才还原持久化记录；正文仍共享不可变存储。
    pub fn log_entries(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .map(super::stored::StoredEntry::to_entry)
            .collect()
    }

    pub(crate) fn metadata(&self) -> SessionMetadata {
        SessionMetadata {
            session: self.session.clone(),
            engine: self.engine,
            plan_active: self.plan_active,
            workflow_activated: self.workflow_activated,
            permission: self.permission,
            model: self.model.clone(),
            prompt_sequence: self.prompt_sequence,
            has_history: !self.transcript.is_empty(),
            tool_policy: self
                .agent
                .as_ref()
                .and_then(|meta| meta.tool_policy.clone()),
        }
    }

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
            entries: Arc::new(Vec::with_capacity(entries.len())),
            transcript: Arc::new(Vec::new()),
            transcript_log_indices: Arc::new(Vec::new()),
            transcript_positions: Arc::new(std::collections::HashMap::new()),
            transcript_ids_unique: true,
            transcript_token_counts: Arc::new(Vec::new()),
            unfrozen_bytes: 0,
            engine: engine.engine(),
            plan_active: engine.legacy_enters_plan(),
            workflow_activated: false,
            permission: *permission,
            model: model.clone(),
            // 以 plan 创建（旧持久模式或会话恢复）时，header 记录的就是会话自身模型。
            todos: Vec::new(),
            tool_calls_since_todo: 0,
            todo_reminders_sent: 0,
            plans: Arc::new(Vec::new()),
            child_calls: Arc::new(Vec::new()),
            child_calls_by_parent: Arc::new(std::collections::HashMap::new()),
            ptc_state: BTreeMap::new(),
            ptc_state_bytes: 0,
            prompt_sequence: 0,
            job_sequence: 0,
            usage: Usage::default(),
            model_usage: BTreeMap::new(),
            tool_ms: 0,
            usage_at: 0,
            summary: None,
            summary_model: None,
            summary_reminder: None,
            first_kept_id: None,
            context_start: 0,
            reset_start: 0,
            reasoning_start: 0,
            compact_end: 0,
            has_reset: false,
            clm_context: None,
            agent: agent.clone(),
            goal: None,
            goal_rounds_started: 0,
            notices: Vec::new(),
            reminder_digests: BTreeMap::new(),
            last_entry_id: first.id().to_owned(),
        };
        Arc::make_mut(&mut state.entries)
            .push(super::stored::StoredEntry::new(first.clone(), None));
        for entry in entries.into_iter().skip(1) {
            state.replay_entry(entry)?;
        }
        Ok(state)
    }

    pub(super) fn replay_entry(&mut self, entry: Entry) -> Result<(), CoreError> {
        if entry.parent_id() != Some(self.last_entry_id.as_str()) {
            return Err(CoreError::Session(format!(
                "entry {} 的 parent_id 与 transcript 链不匹配",
                entry.id()
            )));
        }
        self.apply_entry(entry);
        Ok(())
    }

    /// 把已写入的 entry 应用到会话状态。
    pub fn apply_entry(&mut self, entry: Entry) {
        self.session.updated_at_ms = self.session.updated_at_ms.max(entry.at_ms());
        let projected = entry.transcript_entry().map(Arc::new);
        match &entry {
            Entry::Header { .. } => {}
            Entry::Message {
                message,
                tool,
                bash,
                agent,
                goal,
                reminder,
                ..
            } => {
                // 提醒是模型内部状态，不是用户提示：算进 `prompt_sequence` 会让 prompt ID 错位。
                // goal 续跑轮是一个完整提示（需要独立的 prompt_id），照常计数。
                if message.role == Role::User
                    && bash.is_none()
                    && agent.is_none()
                    && reminder.is_none()
                {
                    self.prompt_sequence = self.prompt_sequence.saturating_add(1);
                    // 有了第一条用户提示，引擎随即锁定。
                    self.session.engine_locked = true;
                }
                // 只统计已落库且属于当前目标的轮次；其它目标或编辑前的旧轮不计。
                if let Some(meta) = goal
                    && self
                        .goal
                        .as_ref()
                        .is_some_and(|current| meta.goal_id == current.id)
                {
                    self.goal_rounds_started = self.goal_rounds_started.max(meta.round);
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
                if let Some(projected) = projected.as_ref() {
                    self.unfrozen_bytes = self
                        .unfrozen_bytes
                        .saturating_add(crate::session::message_storage_bytes(&projected.message));
                    let tokens = projected
                        .context_message()
                        .map_or(0, |message| crate::compact::message_tokens(&message));
                    Arc::make_mut(&mut self.transcript_token_counts).push(tokens);
                    let is_tool = projected.message.role == Role::Tool;
                    if Arc::make_mut(&mut self.transcript_positions)
                        .insert(projected.id.clone(), self.transcript.len())
                        .is_some()
                    {
                        self.transcript_ids_unique = false;
                    }
                    Arc::make_mut(&mut self.transcript_log_indices).push(self.entries.len());
                    Arc::make_mut(&mut self.transcript).push(Arc::clone(projected));
                    if is_tool {
                        self.order_tool_group();
                    }
                }
            }
            Entry::Compaction {
                method,
                summary,
                first_kept_id,
                model,
                reminder,
                ..
            } => {
                self.compact_end = self.transcript.len();
                self.context_start = if method == "remote" {
                    self.reset_start
                } else {
                    first_kept_id
                        .as_ref()
                        .and_then(|id| self.transcript.iter().position(|entry| &entry.id == id))
                        .unwrap_or(self.transcript.len())
                };
                self.usage = Usage::default();
                self.usage_at = 0;
                // 压缩重写了可见历史，旧提醒可能已经被摘要取代；清表让下一次请求补全量。
                self.reminder_digests.clear();
                if method == "remote" {
                    // 旧 remote 文本只记录文件操作，真正的摘要在已移除的加密载荷里。
                    // 作废它的截断边界，从最近 reset 重放原始历史，交给现有压缩管线处理。
                    self.first_kept_id = None;
                    self.summary_model = None;
                    self.summary_reminder = None;
                    self.summary = None;
                } else {
                    self.first_kept_id.clone_from(first_kept_id);
                    self.summary_model.clone_from(model);
                    self.summary_reminder.clone_from(reminder);
                    self.summary = Some((method.clone(), summary.clone()));
                }
            }
            Entry::ResetBoundary { .. } => {
                self.has_reset = true;
                self.reset_start = self.transcript.len();
                self.context_start = self.reset_start;
                self.reasoning_start = self.reset_start;
                self.ptc_state.clear();
                self.ptc_state_bytes = 0;
                self.summary = None;
                self.summary_model = None;
                self.summary_reminder = None;
                self.first_kept_id = None;
                self.clm_context = None;
                self.usage = Usage::default();
                self.usage_at = 0;
                // 历史被重置，旧提醒不再生效。
                self.reminder_digests.clear();
            }
            Entry::ReasoningBoundary { .. } => {
                self.reasoning_start = self.transcript.len();
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
            Entry::FastModeChange { enabled, .. } => {
                self.session.fast_mode = *enabled;
            }
            Entry::PlanMode { active, .. } => {
                self.plan_active = *active;
                self.session.plan_active = *active;
            }
            Entry::WorkflowActivated { active, .. } => {
                self.workflow_activated = *active;
            }
            Entry::GoalChange { goal, .. } => {
                // 同一目标的连续变更（edit/phase 迁移）不清零轮数；
                // 新目标（id 不同）与清除墓碑才归零。
                let same_goal = self
                    .goal
                    .as_ref()
                    .is_some_and(|current| goal.as_ref().is_some_and(|next| next.id == current.id));
                self.goal = goal.clone();
                if !same_goal {
                    self.goal_rounds_started = 0;
                }
            }
            Entry::PermissionChange { permission, .. } => {
                self.permission = *permission;
                self.session.permission = *permission;
            }
            Entry::ModelChange { model, .. } => {
                // 真实发请求的模型（Fusion 的 lead 侧）没变时，用量基线仍然有效，
                // 只换 sidekick 不应重置它。
                let lead_changed = crate::config::model::request_selector(&self.model)
                    != crate::config::model::request_selector(model);
                self.model.clone_from(model);
                self.session.model.clone_from(model);
                if lead_changed {
                    self.usage = Usage::default();
                    self.usage_at = 0;
                }
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
                if let Some(existing) = Arc::make_mut(&mut self.plans)
                    .iter_mut()
                    .find(|plan| plan.plan_id == *plan_id)
                {
                    *existing = info;
                } else {
                    Arc::make_mut(&mut self.plans).push(info);
                }
            }
            Entry::PlanFeedback {
                plan_id,
                annotations,
                ..
            } => {
                if let Some(plan) = Arc::make_mut(&mut self.plans)
                    .iter_mut()
                    .find(|plan| plan.plan_id == *plan_id)
                {
                    plan.annotations.clone_from(annotations);
                    plan.status = PlanStatus::Revised;
                }
            }
            Entry::ChildTool { call, .. } => {
                Arc::make_mut(&mut self.child_calls_by_parent)
                    .entry(call.parent_tool_call_id.clone())
                    .or_default()
                    .push(self.child_calls.len());
                Arc::make_mut(&mut self.child_calls).push(call.clone());
            }
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
        Arc::make_mut(&mut self.entries).push(super::stored::StoredEntry::new(entry, projected));
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
                if self.summary.is_some() && self.first_kept_id.is_some() {
                    if self.context_start == start + offset {
                        self.context_start = start + best;
                    } else if self.context_start == start + best {
                        self.context_start = start + offset;
                    }
                }
                Arc::make_mut(&mut self.transcript).swap(start + offset, start + best);
                Arc::make_mut(&mut self.transcript_log_indices).swap(start + offset, start + best);
                if self.transcript_ids_unique {
                    let positions = Arc::make_mut(&mut self.transcript_positions);
                    positions.insert(self.transcript[start + offset].id.clone(), start + offset);
                    positions.insert(self.transcript[start + best].id.clone(), start + best);
                }
                Arc::make_mut(&mut self.transcript_token_counts).swap(start + offset, start + best);
            }
        }
    }

    pub(super) fn replace_content(&mut self, index: usize, content: moc_llm::MessageContent) {
        let log_index = self.transcript_log_indices[index];
        debug_assert_eq!(self.transcript[index].id, self.entries[log_index].id());
        let super::stored::StoredEntry::Message { entry, .. } =
            &mut Arc::make_mut(&mut self.entries)[log_index]
        else {
            unreachable!("消息投影只能指向日志中的消息条目")
        };
        let projected = &mut Arc::make_mut(&mut self.transcript)[index];
        let mut replacement = (**projected).clone();
        replacement.message.content = content;
        *projected = Arc::new(replacement);
        *entry = Arc::clone(projected);
    }

    /// 查看归档消息后不让其完整解码副本永久驻留；近期未归档正文保持热数据。
    pub(super) fn cool_content(&mut self, index: usize) {
        if self.transcript[index]
            .message
            .content
            .archive_cache_loaded()
        {
            let content = self.transcript[index].message.content.cold_clone();
            self.replace_content(index, content);
        }
    }

    pub(crate) fn child_call_indices(
        &self,
        parents: &std::collections::HashSet<&str>,
        limit: usize,
    ) -> Vec<usize> {
        // 每个父调用最多贡献 limit 个候选，再按原位置排序，不扫描其完整结果列表。
        let mut indices: Vec<_> = parents
            .iter()
            .filter_map(|parent| self.child_calls_by_parent.get(*parent))
            .flat_map(|indices| indices.iter().take(limit).copied())
            .collect();
        indices.sort_unstable();
        indices.truncate(limit);
        debug_assert!(indices.iter().all(|index| *index < self.child_calls.len()));
        indices
    }

    pub(crate) fn transcript_position(&self, id: &str) -> Option<usize> {
        if !self.transcript_ids_unique {
            return self.transcript.iter().rposition(|entry| entry.id == id);
        }
        let position = self.transcript_positions.get(id).copied()?;
        debug_assert!(position < self.transcript.len());
        debug_assert_eq!(self.transcript[position].id, id);
        Some(position)
    }

    /// 当前会话的最新元信息。
    pub fn session_info(&self) -> SessionInfo {
        self.session.clone()
    }
}

#[cfg(test)]
mod tests;
