use rust_i18n::t;
use super::SessionTable;
use super::TranscriptEntryId;
use super::entries;
use super::persist_orphans;
use super::stored::StoredEntry;
use crate::error::CoreError;
use moc_llm::Role;
use moc_llm::ToolCall;
use moc_protocol::methods::SessionLoadResult;
use moc_protocol::methods::TranscriptEntry;
use std::collections::BTreeSet;

impl SessionTable {
    /// 原地回退：把会话历史截回目标消息，不产生新会话。
    ///
    /// 用户消息作为待重新编辑的内容不保留（回到输入框）；助手的工具调用连同紧随其后的
    /// 工具结果一起保留，不留下不完整的工具历史。截掉的尾部压入
    /// [`SessionRecord::rewind_stash`]，供 [`Self::redo`] 前滚。
    pub async fn rewind(
        &self,
        session_id: &str,
        entry_id: &TranscriptEntryId,
    ) -> Result<SessionLoadResult, CoreError> {
        self.rewind_with_limit(session_id, entry_id, None, false)
            .await
    }

    pub(crate) async fn rewind_with_limit(
        &self,
        session_id: &str,
        entry_id: &TranscriptEntryId,
        max_entries: Option<u32>,
        omit_auxiliary: bool,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let (kept, tail) = {
            let state = record.state.lock().await;
            let Some(target) = state
                .entries
                .iter()
                .position(|entry| entry.id() == entry_id.as_str())
            else {
                return Err(CoreError::Session(format!(
                    "INVALID_PARAMS: transcript 消息不存在：{}",
                    entry_id.as_str()
                )));
            };
            let Some(TranscriptEntry {
                message,
                bash,
                agent,
                ..
            }) = state.entries[target].message_entry()
            else {
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 回退目标不是 transcript 消息".into(),
                ));
            };
            let mut end = if message.role == Role::User && bash.is_none() && agent.is_none() {
                target
            } else {
                target + 1
            };
            if message.role == Role::Assistant {
                let mut call_ids: Vec<&str> =
                    message.tool_calls().map(|call| call.id.as_str()).collect();
                while !call_ids.is_empty() && end < state.entries.len() {
                    if let Some(TranscriptEntry { message: next, .. }) =
                        state.entries[end].message_entry()
                    {
                        if next.role != Role::Tool
                            || !next
                                .tool_results()
                                .all(|result| call_ids.contains(&result.tool_call_id.as_str()))
                        {
                            break;
                        }
                        for result in next.tool_results() {
                            call_ids.retain(|id| *id != result.tool_call_id);
                        }
                    }
                    end += 1;
                }
            }
            (
                state.entries[..end]
                    .iter()
                    .map(StoredEntry::to_entry)
                    .collect::<Vec<_>>(),
                state.entries[end..]
                    .iter()
                    .map(StoredEntry::to_entry)
                    .collect::<Vec<_>>(),
            )
        };
        // 尾部引用过、前缀不引用的子代理被孤立：标记后不再进名册，运行中的由
        // service 层停杀；redo 接回引用时摘除标记。RPC spawn 的子代理从不进
        // transcript，两侧都没有引用，天然不受标记影响。标记在截断落盘之后
        // 写入，保证「先截断、后标记」的事实顺序。
        let new_orphans: BTreeSet<String> = if tail.is_empty() {
            BTreeSet::new()
        } else {
            let kept_refs = entries::referenced_agent_ids(&kept);
            entries::referenced_agent_ids(&tail)
                .difference(&kept_refs)
                .cloned()
                .collect()
        };
        self.rewrite(session_id, kept).await?;
        if !tail.is_empty() {
            record.rewind_stash.lock().push(tail.into());
        }
        if !new_orphans.is_empty() {
            let ids = {
                let mut marks = record.orphaned_agents.lock();
                marks.extend(new_orphans);
                marks.clone()
            };
            persist_orphans(&record.paths, &ids).await?;
        }
        self.load_projected(session_id, max_entries, omit_auxiliary)
            .await
    }

    /// 向前滚：把最近一次回退截掉的尾部接回，到 `entry_id` 为止（含）。
    ///
    /// 只校验栈顶能否接上当前 tip：接不上说明回退后写入了新历史，整条废弃时间线作废。
    /// 目标之后的剩余尾部重新压栈，可以分次前滚。
    pub async fn redo(
        &self,
        session_id: &str,
        entry_id: &TranscriptEntryId,
    ) -> Result<SessionLoadResult, CoreError> {
        self.redo_with_limit(session_id, entry_id, None, false)
            .await
    }

    pub(crate) async fn redo_with_limit(
        &self,
        session_id: &str,
        entry_id: &TranscriptEntryId,
        max_entries: Option<u32>,
        omit_auxiliary: bool,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let entries = {
            let state = record.state.lock().await;
            let tip = state.entries.last().map(StoredEntry::id);
            let mut stash = record.rewind_stash.lock();
            let chained = stash
                .last()
                .and_then(|tail| tail.first())
                .is_some_and(|first| first.parent_id() == tip);
            if !chained {
                stash.clear();
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 没有可向前滚的回退历史".into(),
                ));
            }
            let tail = stash.last().expect("栈顶已校验非空");
            let Some(pos) = tail.position(entry_id.as_str()) else {
                return Err(CoreError::Session(format!(
                    "INVALID_PARAMS: 前滚目标不在已回退尾部：{}",
                    entry_id.as_str()
                )));
            };
            let mut restored = state.log_entries();
            restored.extend_from_slice(&tail[..=pos]);
            let remainder = tail[pos + 1..].to_vec();
            if remainder.is_empty() {
                stash.pop();
            } else {
                *stash.last_mut().expect("栈顶已校验非空") = remainder.into();
            }
            restored
        };
        self.rewrite(session_id, entries.clone()).await?;
        // 接回来的条目重新引用了子代理，摘掉对应孤儿标记；剩余仍被截断的继续标记。
        let ids = {
            let refs = entries::referenced_agent_ids(&entries);
            let mut marks = record.orphaned_agents.lock();
            if marks.iter().any(|id| refs.contains(id)) {
                marks.retain(|id| !refs.contains(id));
                Some(marks.clone())
            } else {
                None
            }
        };
        if let Some(ids) = ids {
            persist_orphans(&record.paths, &ids).await?;
        }
        self.load_projected(session_id, max_entries, omit_auxiliary)
            .await
    }

    /// 重新作答前置：把 `tool_call_id` 标识的 `ask` 调用截回未回答状态。
    ///
    /// 含该调用的 assistant 消息原样保留；该调用的结果条目及其后全部丢弃——
    /// 不压回退栈：新结果紧接着落库，旧尾部本来就没有可接的位置。同一
    /// assistant 消息里排在该调用之后、结果随尾部被砍的兄弟调用回到「未
    /// 回答」状态，由下一次提示的 pending 修补兜底标记。截断引用的子代理
    /// 按与 [`Self::rewind`] 相同的规则标孤儿。返回待重放的调用。
    pub async fn prepare_reask(
        &self,
        session_id: &str,
        tool_call_id: &str,
    ) -> Result<ToolCall, CoreError> {
        let record = self.get(session_id).await?;
        let (call, kept, tail) = {
            let state = record.state.lock().await;
            let pos_call = state
                .entries
                .iter()
                .position(|entry| {
                    matches!(
                        entry.message_entry(),
                        Some(TranscriptEntry { message, .. })
                            if message.role == Role::Assistant
                                && message.tool_calls().any(|call| call.id == tool_call_id)
                    )
                })
                .ok_or_else(|| {
                    CoreError::Session(
                        t!("core.err.rewind_tool_call", id = tool_call_id).into_owned(),
                    )
                })?;
            let Some(TranscriptEntry { message, .. }) = state.entries[pos_call].message_entry()
            else {
                unreachable!("position 已锁定 assistant 消息");
            };
            let call = message
                .tool_calls()
                .find(|call| call.id == tool_call_id)
                .expect("position 已锁定调用")
                .clone();
            if call.name != "ask" {
                return Err(CoreError::Session(format!(
                    "INVALID_PARAMS: 只能重新作答 ask 调用，{} 是 {}",
                    call.id, call.name
                )));
            }
            // 结果条目：调用消息之后、装着该调用结果的第一条 Tool 消息。
            // 找不到说明这次提问还没收场——卡片仍挂着，没有可「重新」的回答。
            let Some(offset) = state.entries[pos_call + 1..].iter().position(|entry| {
                matches!(
                    entry.message_entry(),
                    Some(TranscriptEntry { message, .. })
                        if message.role == Role::Tool
                            && message
                                .tool_results()
                                .any(|result| result.tool_call_id == tool_call_id)
                )
            }) else {
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 该提问还没有结果，无需重新作答".into(),
                ));
            };
            let end = pos_call + 1 + offset;
            (
                call,
                state.entries[..end]
                    .iter()
                    .map(StoredEntry::to_entry)
                    .collect::<Vec<_>>(),
                state.entries[end..]
                    .iter()
                    .map(StoredEntry::to_entry)
                    .collect::<Vec<_>>(),
            )
        };
        // 孤儿标记规则与 rewind 相同（见 rewind 注释）。
        let new_orphans: BTreeSet<String> = if tail.is_empty() {
            BTreeSet::new()
        } else {
            let kept_refs = entries::referenced_agent_ids(&kept);
            entries::referenced_agent_ids(&tail)
                .difference(&kept_refs)
                .cloned()
                .collect()
        };
        self.rewrite(session_id, kept).await?;
        if !new_orphans.is_empty() {
            let ids = {
                let mut marks = record.orphaned_agents.lock();
                marks.extend(new_orphans);
                marks.clone()
            };
            persist_orphans(&record.paths, &ids).await?;
        }
        Ok(call)
    }
}
