use super::SessionTable;
use super::archive;
use super::cache_budget::serialized_bytes;
use super::entries::Entry;
use super::lock;
use super::projection::page_plans;
use super::projection::project_message;
use super::projection::truncate_text;
use super::stored::StoredEntry;
use crate::error::CoreError;
use moc_protocol::methods::SessionLoadPageResult;
use moc_protocol::methods::SessionLoadResult;
use moc_protocol::methods::TranscriptChildCall;
use moc_protocol::methods::TranscriptEntry;
use moc_i18n::t;

impl SessionTable {
    /// 加载可展示的消息；不进入 transcript 的 entry 只更新会话状态，不出现在列表里。
    pub async fn load(
        &self,
        session_id: &str,
        max_entries: Option<u32>,
    ) -> Result<SessionLoadResult, CoreError> {
        self.load_projected(session_id, max_entries, false).await
    }

    pub(crate) async fn load_projected(
        &self,
        session_id: &str,
        max_entries: Option<u32>,
        omit_auxiliary: bool,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let mut state = record.state.lock().await;
        let count = state.transcript.len();
        let start = max_entries
            .and_then(|limit| usize::try_from(limit).ok())
            .map_or(0, |limit| count.saturating_sub(limit));
        let entries = state.transcript[start..]
            .iter()
            .map(|entry| (**entry).clone())
            .collect();
        let cool_end = archive::cold_end(&state);
        for index in start..cool_end {
            state.cool_content(index);
        }
        let mut session = state.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(SessionLoadResult {
            session,
            entries,
            truncated: start > 0,
            todos: if omit_auxiliary {
                Vec::new()
            } else {
                state.todos.clone()
            },
            plans: if omit_auxiliary {
                Vec::new()
            } else {
                state.plans.as_ref().clone()
            },
            child_calls: if omit_auxiliary || max_entries == Some(0) {
                Vec::new()
            } else {
                state.child_calls.as_ref().clone()
            },
            goal: if omit_auxiliary {
                None
            } else {
                crate::goal::view(state.goal.as_ref(), state.goal_rounds_started, false)
            },
        })
    }

    /// 读取大小受限的展示页：超出单条预算的正文、思维链和工具结果不会被完整复制。
    pub async fn load_page(
        &self,
        session_id: &str,
        before_entry_id: Option<&str>,
        max_entries: u32,
        max_message_bytes: Option<u32>,
    ) -> Result<SessionLoadPageResult, CoreError> {
        self.load_pages(
            session_id,
            before_entry_id,
            max_entries,
            max_message_bytes,
            None,
            None,
            false,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "对应同一分页请求的正交投影参数，调用方保持旧借用接口"
    )]
    pub(crate) async fn load_pages(
        &self,
        session_id: &str,
        before_entry_id: Option<&str>,
        max_entries: u32,
        max_message_bytes: Option<u32>,
        redo_page: Option<&moc_protocol::methods::session::RedoPageParams>,
        plan_page: Option<&moc_protocol::methods::session::PlanPageParams>,
        full_messages: bool,
    ) -> Result<SessionLoadPageResult, CoreError> {
        const MAX_PAGE_ENTRIES: usize = 128;
        const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
        const DEFAULT_MESSAGE_BYTES: usize = 64 * 1024;
        const DEFAULT_MESSAGE_BYTES_U32: u32 = 64 * 1024;
        const MAX_MESSAGE_BYTES: usize = 256 * 1024;

        let record = self.get(session_id).await?;
        let before_entry_id = before_entry_id.map(str::to_owned);
        let redo_page = redo_page.cloned();
        let plan_page = plan_page.cloned();
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let redo_page = redo_page.as_ref();
            let plan_page = plan_page.as_ref();
            let state = record.state.blocking_lock();
            let end = match before_entry_id.as_deref() {
                Some(id) => state.transcript_position(id).ok_or_else(|| {
                    CoreError::Session(t!("core.err.transcript_msg_missing", id = id).into_owned())
                })?,
                None => state.transcript.len(),
            };
            let limit = usize::try_from(max_entries)
                .unwrap_or(MAX_PAGE_ENTRIES)
                .clamp(usize::from(plan_page.is_none()), MAX_PAGE_ENTRIES);
            let start = end.saturating_sub(limit);
            let budget = usize::try_from(max_message_bytes.unwrap_or(DEFAULT_MESSAGE_BYTES_U32))
                .unwrap_or(DEFAULT_MESSAGE_BYTES)
                .clamp(1024, MAX_MESSAGE_BYTES);
            let mut entries = Vec::with_capacity(end - start);
            let mut content_truncated = false;
            // 预算计入 JSON 转义和条目元数据；单个超大旧条目保持完整附加信息。
            let mut full_children = Vec::new();
            let mut page_bytes = if full_messages { 4usize } else { 2usize };
            let history_budget = if redo_page.is_some_and(|page| page.max_entries > 0) {
                MAX_PAGE_BYTES / 2
            } else {
                MAX_PAGE_BYTES
            };
            for entry in state.transcript[start..end].iter().rev() {
                let remaining = history_budget.saturating_sub(page_bytes);
                if remaining == 0 {
                    break;
                }
                let entry_budget = budget.min(remaining);
                let (message, truncated) = if full_messages {
                    (entry.message.clone(), false)
                } else {
                    project_message(&entry.message, entry_budget)
                };
                let projected = TranscriptEntry {
                    id: entry.id.clone(),
                    at_ms: entry.at_ms,
                    message,
                    tool: entry.tool.clone(),
                    bash: entry.bash.clone(),
                    agent: entry.agent.clone(),
                    agent_result: entry.agent_result.clone(),
                    goal_round: entry.goal_round.clone(),
                    reminder: entry.reminder.clone(),
                    job_notice: entry.job_notice.clone(),
                    response: entry.response.clone(),
                };
                let mut size = serialized_bytes(&projected).saturating_add(1);
                let children = if full_messages {
                    let ids = entry.message.content.tool_call_ids().collect();
                    state.child_call_indices(&ids, usize::MAX)
                } else {
                    Vec::new()
                };
                for &index in &children {
                    size = size
                        .saturating_add(serialized_bytes(&state.child_calls[index]))
                        .saturating_add(1);
                }
                if !entries.is_empty() && size > remaining {
                    break;
                }
                content_truncated |= truncated;
                page_bytes = page_bytes.saturating_add(size);
                full_children.extend(children);
                entries.push(projected);
            }
            entries.reverse();
            let child_calls = if full_messages {
                // 一个旧日志可重复引用父调用 ID；输出仍按子调用原顺序去重。
                full_children.sort_unstable();
                full_children.dedup();
                full_children
                    .into_iter()
                    .map(|index| state.child_calls[index].clone())
                    .collect()
            } else {
                let ids: std::collections::HashSet<&str> = entries
                    .iter()
                    .flat_map(|entry| entry.message.content.tool_call_ids())
                    .collect();
                let child_budget = budget.min(8 * 1024);
                state
                    .child_call_indices(&ids, 64)
                    .into_iter()
                    .map(|index| &state.child_calls[index])
                    .map(|call| TranscriptChildCall {
                        id: truncate_text(&call.id, child_budget).0,
                        parent_tool_call_id: truncate_text(&call.parent_tool_call_id, child_budget)
                            .0,
                        name: truncate_text(&call.name, child_budget).0,
                        arguments: truncate_text(&call.arguments, child_budget).0,
                        result: truncate_text(&call.result, child_budget).0,
                        status: call.status,
                        duration_ms: call.duration_ms,
                        diff: call.diff.clone(),
                    })
                    .collect()
            };
            let has_more = start > 0 || entries.len() < end.saturating_sub(start);
            // 前滚列表只暴露 transcript 条目；非消息 entry 会随目标一并接回，但不在选择器里占位。
            let tip = state.entries.last().map(StoredEntry::id);
            let mut redo_has_more = false;
            let mut redo_tip_entry_id = None;
            let redo_entries = {
                let stash = record.rewind_stash.lock();
                match stash
                    .last()
                    .filter(|tail| tail.first().is_some_and(|first| first.parent_id() == tip))
                {
                    None => Vec::new(),
                    Some(tail) => match redo_page {
                        None => tail.iter().filter_map(Entry::transcript_entry).collect(),
                        Some(page) => {
                            redo_tip_entry_id = tail.message_tip.clone();
                            let end = match &page.before_entry_id {
                                Some(id) => tail.position(id).ok_or_else(|| {
                                    CoreError::Session(
                                        t!("core.err.rollforward_msg_missing", id = id)
                                            .into_owned(),
                                    )
                                })?,
                                None => tail.len(),
                            };
                            let mut candidates = tail[..end]
                                .iter()
                                .rev()
                                .filter(|entry| matches!(entry, Entry::Message { .. }))
                                .peekable();
                            let limit = (page.max_entries as usize).min(MAX_PAGE_ENTRIES);
                            let mut entries = Vec::with_capacity(limit);
                            let mut redo_bytes = 2usize;
                            let redo_budget = MAX_PAGE_BYTES
                                .saturating_sub(page_bytes)
                                .max(MAX_PAGE_BYTES / 2);
                            while entries.len() < limit && redo_bytes < redo_budget {
                                let Some(entry) = candidates.peek() else {
                                    break;
                                };
                                let bytes = serialized_bytes(*entry);
                                if !entries.is_empty()
                                    && bytes > redo_budget.saturating_sub(redo_bytes)
                                {
                                    break;
                                }
                                // 单条超预算独占候选页，保持既有的完整详情；协议原帧上限仍适用。
                                let entry = candidates.next().expect("已确认候选存在");
                                redo_bytes = redo_bytes.saturating_add(bytes).saturating_add(1);
                                entries.push(entry.transcript_entry().expect("已筛选消息条目"));
                            }
                            redo_has_more = candidates.peek().is_some();
                            entries.reverse();
                            entries
                        }
                    },
                }
            };
            let (plans, plans_before) = match plan_page {
                None => (state.plans.as_ref().clone(), None),
                Some(page) => page_plans(&state.plans, page)?,
            };
            let mut session = state.session_info();
            session.lock = lock::read(record.paths.lock_file(&session_id));
            Ok(SessionLoadPageResult {
                session,
                entries,
                has_more,
                todos: state.todos.clone(),
                plans,
                plans_before,
                child_calls,
                content_truncated,
                redo_entries,
                redo_has_more,
                redo_tip_entry_id,
                goal: crate::goal::view(state.goal.as_ref(), state.goal_rounds_started, false),
            })
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }
}
