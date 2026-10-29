//! 会话表、追加式 transcript，以及每个会话的提示互斥。

pub mod entries;
pub mod lock;
pub(crate) mod log;
pub mod maintenance;
pub mod state;
pub mod store;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moc_llm::{ContentPart, FinishReason, Message, ReasoningBlock, Role, ToolCall, ToolResult};
use moc_protocol::methods::{
    SessionAutonomy, SessionEngine, SessionInfo, SessionLoadPageResult, SessionLoadResult,
    SessionLockInfo, SessionPermission, SessionRenameParams, ToolExecution, TranscriptChildCall,
    TranscriptEntry,
};
use parking_lot::RwLock;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::CoreError;
use crate::ids;

use self::entries::{Entry, EntryPayload, SessionOrigin};
use self::state::SessionState;
use self::store::{SessionFile, SessionPaths, SessionStore, now_ms};

/// transcript 条目 ID，避免内部调用时和会话 ID 混淆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntryId(String);

impl TranscriptEntryId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一份加载中的会话。
pub struct SessionRecord {
    pub state: Arc<Mutex<SessionState>>,
    pub prompt_lock: Arc<Mutex<()>>,
    append_lock: Mutex<()>,
    active: AtomicBool,
    pub paths: SessionPaths,
    /// 磁盘上还没有 transcript 文件时为 `None`；第一次 append 时才写入磁盘。
    file: Mutex<Option<SessionFile>>,
    header: Entry,
    /// 跨进程写租约；`None` 表示本进程只读打开或尚未打开写模式。
    ///
    /// 用同步锁包装：只在取/放租约的短临界区里访问，不跨 `await`。
    lease: parking_lot::Mutex<Option<lock::SessionLockGuard>>,
    /// 串行化「检查 → 取租约 → 对齐磁盘」的整个流程，避免两个并发请求同时去抢内核锁。
    lease_gate: Mutex<()>,
    /// 回退截掉的尾部，栈式存放供 `redo` 前滚；每次回退压一层，栈顶是当前可接回的
    /// 那段。栈顶首条的 `parent_id` 接不上当前 tip 时整条废弃时间线作废（清空）。
    ///
    /// 用同步锁包装：只在取/放栈的短临界区里访问，不跨 `await`。
    rewind_stash: parking_lot::Mutex<Vec<Vec<Entry>>>,
    /// 被回退孤立的子代理 id：截掉的尾部引用过、但保留前缀不引用的那些。
    /// redo 把引用接回时摘除对应标记；新历史落盘作废 stash 后标记仍保留——
    /// 那条分支已永久死去，名册继续对它们不可见。
    ///
    /// 与 transcript 截断同为永久事实，所以写进父会话 sidecar 的
    /// `orphaned_agents` 文件（每行一个 id）持久化：进程重启后孤儿标记仍有效，
    /// 这点和仅限进程内的 `rewind_stash` 不同。用同步锁包装：只在取/放集合的
    /// 短临界区里访问，不跨 `await`。
    pub(crate) orphaned_agents: parking_lot::Mutex<BTreeSet<String>>,
}

impl SessionRecord {
    pub fn try_prompt_lock(&self) -> Result<OwnedMutexGuard<()>, CoreError> {
        let guard = Arc::clone(&self.prompt_lock)
            .try_lock_owned()
            .map_err(|_| CoreError::Session("SESSION_BUSY: 会话已有进行中的提示".into()))?;
        if !self.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        Ok(guard)
    }
}

/// 会话生命周期管理器。
pub struct SessionTable {
    store: SessionStore,
    locking: bool,
    records: RwLock<HashMap<String, Arc<SessionRecord>>>,
}

impl SessionTable {
    pub fn new(root: impl Into<PathBuf>, locking: bool) -> Self {
        Self {
            store: SessionStore::new(root),
            locking,
            records: RwLock::new(HashMap::new()),
        }
    }

    /// 只登记内存状态；transcript 在第一次 append 时才写盘，空会话不会留下文件。
    /// `compress` 由调用者传入本工作区的有效配置，格式在创建时固定。
    pub async fn create(
        &self,
        cwd: impl AsRef<Path>,
        model: &str,
        engine: SessionEngine,
        permission: SessionPermission,
        title: Option<&str>,
        origin: SessionOrigin,
        compress: bool,
    ) -> Result<SessionInfo, CoreError> {
        let cwd = cwd.as_ref();
        let session_id = ids::session_id();
        let created_at_ms = now_ms();
        let title = title
            .map(sanitize_title)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "新会话".into());
        let origin_parent = origin.parent_session_id.clone();
        let header = Entry::header(
            session_id.clone(),
            cwd.to_string_lossy().into_owned(),
            model.to_owned(),
            engine,
            permission,
            origin,
            title,
            created_at_ms,
        );
        let state = SessionState::new(header.clone())?;
        // 子代理落在父会话目录里，代理树和目录树同构：删除父会话会一并删除整棵子树。
        let paths = match origin_parent.as_deref() {
            Some(parent) => {
                self.store
                    .paths_for_child(cwd, parent, &session_id, created_at_ms, compress)
                    .await?
            }
            None if compress => self
                .store
                .paths_for_compressed(cwd, &session_id, created_at_ms),
            None => self.store.paths_for(cwd, &session_id, created_at_ms),
        };
        let info = state.session_info();
        self.records.write().insert(
            session_id,
            Arc::new(SessionRecord {
                state: Arc::new(Mutex::new(state)),
                prompt_lock: Arc::new(Mutex::new(())),
                append_lock: Mutex::new(()),
                active: AtomicBool::new(true),
                file: Mutex::new(None),
                header,
                paths,
                lease: parking_lot::Mutex::new(None),
                lease_gate: Mutex::new(()),
                rewind_stash: parking_lot::Mutex::new(Vec::new()),
                orphaned_agents: parking_lot::Mutex::new(BTreeSet::new()),
            }),
        );
        Ok(info)
    }

    /// 返回已加载会话；磁盘会话按需读入。
    pub async fn get(&self, session_id: &str) -> Result<Arc<SessionRecord>, CoreError> {
        if let Some(record) = self.records.read().get(session_id).cloned() {
            return Ok(record);
        }
        let paths = self
            .store
            .find(session_id)
            .await?
            .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {session_id}")))?;
        // 只读加载不碰写租约：写者持锁期间读者仍可查看已落盘的完整行。
        let entries = self.store.read_entries(&paths).await?;
        let state = SessionState::from_entries(entries)?;
        if state.session.id != session_id {
            return Err(CoreError::Session(format!(
                "transcript session_id {} 与文件名 {session_id} 不匹配",
                state.session.id
            )));
        }
        let file = self.store.open(&paths).await?;
        let header = state
            .entries
            .first()
            .cloned()
            .ok_or_else(|| CoreError::Session("transcript 缺少会话头部".into()))?;
        let orphaned_agents = load_orphans(&paths).await;
        let record = Arc::new(SessionRecord {
            state: Arc::new(Mutex::new(state)),
            prompt_lock: Arc::new(Mutex::new(())),
            append_lock: Mutex::new(()),
            active: AtomicBool::new(true),
            file: Mutex::new(Some(file)),
            header,
            paths,
            lease: parking_lot::Mutex::new(None),
            lease_gate: Mutex::new(()),
            rewind_stash: parking_lot::Mutex::new(Vec::new()),
            orphaned_agents: parking_lot::Mutex::new(orphaned_agents),
        });
        let mut records = self.records.write();
        Ok(Arc::clone(
            records.entry(session_id.to_owned()).or_insert(record),
        ))
    }

    /// 该会话当前标记为孤儿的子代理 id。
    ///
    /// 已加载会话读内存集合；未加载时只读 sidecar 里的标记文件，不加载整个
    /// transcript——名册构建是热路径，不能为这点数据付全量解析的代价。
    pub(crate) async fn orphan_marks(&self, session_id: &str) -> BTreeSet<String> {
        if let Some(record) = self.records.read().get(session_id) {
            return record.orphaned_agents.lock().clone();
        }
        match self.store.find(session_id).await {
            Ok(Some(paths)) => load_orphans(&paths).await,
            _ => BTreeSet::new(),
        }
    }

    /// 磁盘上的日志是跨进程的唯一依据，不能用缓存状态覆盖外部更新。
    pub async fn list(&self) -> Result<Vec<SessionInfo>, CoreError> {
        self.store.list().await
    }

    /// 读取插件在本会话中的一个状态值。插件状态不进入 transcript，且按插件文件隔离。
    pub async fn plugin_state_get(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        let state = self.read_plugin_state(&record, plugin).await?;
        Ok(state.get(key).cloned())
    }

    /// 一次读取完整命名空间，供插件本次回调缓存，避免枚举时逐键重读文件。
    pub async fn plugin_state_snapshot(
        &self,
        session_id: &str,
        plugin: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, CoreError> {
        validate_plugin_state_component(plugin, "plugin")?;
        let record = self.get(session_id).await?;
        self.read_plugin_state(&record, plugin).await
    }

    /// 列出插件在本会话中已保存的状态键。
    pub async fn plugin_state_keys(
        &self,
        session_id: &str,
        plugin: &str,
    ) -> Result<Vec<String>, CoreError> {
        validate_plugin_state_component(plugin, "plugin")?;
        let record = self.get(session_id).await?;
        let state = self.read_plugin_state(&record, plugin).await?;
        Ok(state.keys().cloned().collect())
    }

    /// 原子写入插件状态；磁盘失败时不更新内存（状态仅以文件为准）。
    pub async fn plugin_state_set(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        self.ensure_write_lease(session_id, &record, "moc-plugin")
            .await?;
        let _append = record.append_lock.lock().await;
        if !record.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        self.ensure_transcript_header(&record).await?;
        let mut state = self.read_plugin_state(&record, plugin).await?;
        state.insert(key.to_owned(), value);
        self.store
            .write_plugin_state(&record.paths, plugin, &state)
            .await
    }

    /// 原子删除插件状态；返回是否存在该键。
    pub async fn plugin_state_remove(
        &self,
        session_id: &str,
        plugin: &str,
        key: &str,
    ) -> Result<bool, CoreError> {
        validate_plugin_state_key(plugin, key)?;
        let record = self.get(session_id).await?;
        self.ensure_write_lease(session_id, &record, "moc-plugin")
            .await?;
        let _append = record.append_lock.lock().await;
        if !record.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        let mut state = self.read_plugin_state(&record, plugin).await?;
        let removed = state.remove(key).is_some();
        if removed {
            self.store
                .write_plugin_state(&record.paths, plugin, &state)
                .await?;
        }
        Ok(removed)
    }

    async fn read_plugin_state(
        &self,
        record: &Arc<SessionRecord>,
        plugin: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, CoreError> {
        let ensure_active = || {
            if record.active.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(CoreError::Session(
                    "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
                ))
            }
        };
        ensure_active()?;
        let state = self.store.read_plugin_state(&record.paths, plugin).await;
        ensure_active()?;
        state
    }

    async fn ensure_transcript_header(&self, record: &Arc<SessionRecord>) -> Result<(), CoreError> {
        let mut file = record.file.lock().await;
        if file.is_none() {
            *file = Some(
                SessionFile::create_with_header(&record.paths.transcript, &record.header).await?,
            );
        }
        Ok(())
    }

    /// 已知会话：合并磁盘上的会话和内存里已加载的会话，内存记录优先。
    ///
    /// `list` 只看磁盘，而 transcript 要到首次写入 entry 或插件状态时才落盘：
    /// `session/new` 和刚 spawn 的子代理可能只存在于内存里，解析代理关系时仍须可见。
    pub async fn known(&self) -> Result<Vec<SessionInfo>, CoreError> {
        let mut sessions: HashMap<String, SessionInfo> = self
            .store
            .list()
            .await?
            .into_iter()
            .map(|info| (info.id.clone(), info))
            .collect();
        let records: Vec<Arc<SessionRecord>> = self.records.read().values().cloned().collect();
        for record in records {
            let info = record.state.lock().await.session_info();
            sessions.insert(info.id.clone(), info);
        }
        let mut sessions: Vec<SessionInfo> = sessions.into_values().collect();
        // 固定顺序：按创建时间排列，子代理的解析顺序因此稳定，等待快照不会随哈希顺序抖动。
        sessions.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }

    /// 加载可展示的消息；不进入 transcript 的 entry 只更新会话状态，不出现在列表里。
    pub async fn load(
        &self,
        session_id: &str,
        max_entries: Option<u32>,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let state = record.state.lock().await;
        let count = state.transcript.len();
        let start = max_entries
            .and_then(|limit| usize::try_from(limit).ok())
            .map_or(0, |limit| count.saturating_sub(limit));
        let entries = state.transcript[start..].to_vec();
        let mut session = state.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(SessionLoadResult {
            session,
            entries,
            truncated: start > 0,
            todos: state.todos.clone(),
            plans: state.plans.clone(),
            child_calls: state.child_calls.clone(),
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
        const MAX_PAGE_ENTRIES: usize = 128;
        const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
        const DEFAULT_MESSAGE_BYTES: usize = 64 * 1024;
        const DEFAULT_MESSAGE_BYTES_U32: u32 = 64 * 1024;
        const MAX_MESSAGE_BYTES: usize = 256 * 1024;

        let record = self.get(session_id).await?;
        let state = record.state.lock().await;
        let end = match before_entry_id {
            Some(id) => state
                .transcript
                .iter()
                .rposition(|entry| entry.id == id)
                .ok_or_else(|| CoreError::Session(format!("transcript 消息不存在：{id}")))?,
            None => state.transcript.len(),
        };
        let limit = usize::try_from(max_entries)
            .unwrap_or(MAX_PAGE_ENTRIES)
            .clamp(1, MAX_PAGE_ENTRIES);
        let start = end.saturating_sub(limit);
        let budget = usize::try_from(max_message_bytes.unwrap_or(DEFAULT_MESSAGE_BYTES_U32))
            .unwrap_or(DEFAULT_MESSAGE_BYTES)
            .clamp(1024, MAX_MESSAGE_BYTES);
        let mut entries = Vec::with_capacity(end - start);
        let mut content_truncated = false;
        let mut page_bytes = 0usize;
        for entry in &state.transcript[start..end] {
            let remaining = MAX_PAGE_BYTES.saturating_sub(page_bytes);
            if remaining == 0 {
                break;
            }
            let entry_budget = budget.min(remaining);
            let (message, truncated) = project_message(&entry.message, entry_budget);
            content_truncated |= truncated;
            page_bytes = page_bytes
                .saturating_add(message_bytes(&message))
                .saturating_add(entry.id.len())
                .saturating_add(64);
            entries.push(TranscriptEntry {
                id: entry.id.clone(),
                at_ms: entry.at_ms,
                message,
                tool: entry.tool.clone(),
                bash: entry.bash.clone(),
                agent: entry.agent.clone(),
                reminder: entry.reminder.clone(),
            });
        }
        let ids: std::collections::HashSet<&str> = entries
            .iter()
            .flat_map(|entry| entry.message.tool_calls().map(|call| call.id.as_str()))
            .collect();
        let child_budget = budget.min(8 * 1024);
        let child_calls = state
            .child_calls
            .iter()
            .filter(|call| ids.contains(call.parent_tool_call_id.as_str()))
            .take(64)
            .map(|call| TranscriptChildCall {
                id: truncate_text(&call.id, child_budget).0,
                parent_tool_call_id: truncate_text(&call.parent_tool_call_id, child_budget).0,
                name: truncate_text(&call.name, child_budget).0,
                arguments: truncate_text(&call.arguments, child_budget).0,
                result: truncate_text(&call.result, child_budget).0,
                status: call.status,
                duration_ms: call.duration_ms,
            })
            .collect();
        let has_more = start > 0 || entries.len() < end.saturating_sub(start);
        // 前滚列表只暴露 transcript 条目；非消息 entry 会随目标一并接回，但不在选择器里占位。
        let tip = state.entries.last().map(Entry::id);
        let redo_entries = {
            let stash = record.rewind_stash.lock();
            if stash
                .last()
                .and_then(|tail| tail.first())
                .is_some_and(|first| first.parent_id() == tip)
            {
                stash
                    .last()
                    .expect("栈顶已校验非空")
                    .iter()
                    .filter_map(|entry| entry.transcript_entry())
                    .collect()
            } else {
                Vec::new()
            }
        };
        let mut session = state.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(SessionLoadPageResult {
            session,
            entries,
            has_more,
            todos: state.todos.clone(),
            plans: state.plans.clone(),
            child_calls,
            content_truncated,
            redo_entries,
        })
    }
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
            let Entry::Message {
                message,
                bash,
                agent,
                ..
            } = &state.entries[target]
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
                    if let Entry::Message { message: next, .. } = &state.entries[end] {
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
            (state.entries[..end].to_vec(), state.entries[end..].to_vec())
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
            record.rewind_stash.lock().push(tail);
        }
        if !new_orphans.is_empty() {
            let ids = {
                let mut marks = record.orphaned_agents.lock();
                marks.extend(new_orphans);
                marks.clone()
            };
            persist_orphans(&record.paths, &ids).await?;
        }
        self.load(session_id, None).await
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
        let record = self.get(session_id).await?;
        let entries = {
            let state = record.state.lock().await;
            let tip = state.entries.last().map(Entry::id);
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
            let Some(pos) = tail
                .iter()
                .position(|entry| entry.id() == entry_id.as_str())
            else {
                return Err(CoreError::Session(format!(
                    "INVALID_PARAMS: 前滚目标不在已回退尾部：{}",
                    entry_id.as_str()
                )));
            };
            let mut restored = state.entries.clone();
            restored.extend_from_slice(&tail[..=pos]);
            let remainder = tail[pos + 1..].to_vec();
            if remainder.is_empty() {
                stash.pop();
            } else {
                *stash.last_mut().expect("栈顶已校验非空") = remainder;
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
        self.load(session_id, None).await
    }

    /// 追加新 entry；持有状态锁时不跨文件 I/O 的 await。
    ///
    /// transcript 文件还不存在时（新会话尚未写盘），先写 header 再写 entry，完成延迟
    /// 写入；因此从未产生 entry 的会话不会在磁盘上留下文件。
    pub async fn append(
        &self,
        session_id: &str,
        payload: EntryPayload,
    ) -> Result<Entry, CoreError> {
        let record = self.get(session_id).await?;
        // 写入事务放在独立任务里：即使请求的 future 被丢弃，磁盘和内存的提交也必须完成。
        tokio::spawn(async move {
            let _append = record.append_lock.lock().await;
            if !record.active.load(Ordering::Acquire) {
                return Err(CoreError::Session(
                    "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
                ));
            }
            let entry = {
                let state = record.state.lock().await;
                Entry::from_payload(payload, state.last_entry_id.clone(), now_ms())
            };
            let mut file_slot = record.file.lock().await;
            if file_slot.is_none() {
                *file_slot = Some(
                    SessionFile::create_with_header(&record.paths.transcript, &record.header)
                        .await?,
                );
            }
            file_slot
                .as_ref()
                .expect("此处一定已经打开或创建了文件句柄")
                .append(&entry, None)
                .await?;
            record.state.lock().await.apply_entry(entry.clone());
            // 回退后写入新历史会斩断可前滚的尾部：栈顶接不上新 tip 就整栈作废。
            {
                let mut stash = record.rewind_stash.lock();
                let chained = stash
                    .last()
                    .and_then(|tail| tail.first())
                    .is_some_and(|first| first.parent_id() == Some(entry.id()));
                if !chained {
                    stash.clear();
                }
            }
            Ok(entry)
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }

    /// 追加一条消息 entry。
    pub async fn append_message(
        &self,
        session_id: &str,
        message: Message,
        tool: Option<ToolExecution>,
        finish_reason: Option<FinishReason>,
    ) -> Result<Entry, CoreError> {
        self.append(
            session_id,
            EntryPayload::Message {
                message,
                tool,
                finish_reason,
                agent: None,
            },
        )
        .await
    }

    /// 追加一条代理间消息 entry（带来源信息）。
    pub async fn append_agent_message(
        &self,
        session_id: &str,
        message: Message,
        agent: moc_protocol::methods::AgentMessageMeta,
    ) -> Result<Entry, CoreError> {
        self.append(
            session_id,
            EntryPayload::Message {
                message,
                tool: None,
                finish_reason: None,
                agent: Some(agent),
            },
        )
        .await
    }

    /// 追加一条运行时提醒 entry。
    ///
    /// 提醒落库后成为历史的一部分，所以只在条目内容变化时写一次；`items` 是本次
    /// 真正写进去的条目摘要，重放时据此判断哪些条目已经在历史里。
    pub async fn append_reminder(
        &self,
        session_id: &str,
        message: Message,
        items: Vec<moc_protocol::ReminderItem>,
    ) -> Result<Entry, CoreError> {
        self.append(session_id, EntryPayload::Reminder { message, items })
            .await
    }

    /// 原子替换 transcript，同时重建内存里的状态。
    pub async fn rewrite(&self, session_id: &str, entries: Vec<Entry>) -> Result<(), CoreError> {
        let record = self.get(session_id).await?;
        let mut state = SessionState::from_entries(entries.clone())?;
        if state.session.id != session_id {
            return Err(CoreError::Session(
                "重写 transcript 的会话 ID 不匹配".into(),
            ));
        }
        tokio::spawn(async move {
            let _append = record.append_lock.lock().await;
            if !record.active.load(Ordering::Acquire) {
                return Err(CoreError::Session(
                    "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
                ));
            }
            let mut file_slot = record.file.lock().await;
            if file_slot.is_none() {
                *file_slot = Some(
                    SessionFile::create_with_header(&record.paths.transcript, &record.header)
                        .await?,
                );
            }
            file_slot
                .as_ref()
                .expect("此处一定已经打开或创建了文件句柄")
                .rewrite(&entries)
                .await?;
            let mut current = record.state.lock().await;
            state.notices = std::mem::take(&mut current.notices);
            state.job_sequence = current.job_sequence;
            state.server_compaction_disabled = current.server_compaction_disabled;
            *current = state;
            Ok(())
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }

    /// 改名，并追加一条可重放的标题事件。
    pub async fn rename(
        &self,
        session_id: &str,
        params: SessionRenameParams,
    ) -> Result<SessionInfo, CoreError> {
        if params.session_id != session_id {
            return Err(CoreError::Session(
                "INVALID_PARAMS: session_id 不匹配".into(),
            ));
        }
        let record = self.get(session_id).await?;
        let title = sanitize_title(&params.title);
        if title.is_empty() {
            return Err(CoreError::Session("INVALID_PARAMS: 标题不能为空".into()));
        }
        self.append(session_id, EntryPayload::TitleChange { title })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话引擎的变更。
    ///
    /// 引擎是会话级属性：transcript 里每一种引擎的工具调用形态不同，混排会让
    /// 模型模仿已失效的调用格式，所以一旦会话跑过用户提示（`prompt_sequence > 0`
    /// 即 [`SessionInfo::engine_locked`]）就拒绝一切引擎切换；回退到空白后自然解锁。
    pub async fn set_engine(
        &self,
        session_id: &str,
        engine: SessionEngine,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        let plan_active = {
            let state = record.state.lock().await;
            if state.prompt_sequence > 0 && engine != state.engine {
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 会话已有提示历史，引擎已锁定；请新建会话选择引擎".into(),
                ));
            }
            state.plan_active
        };
        self.append(
            session_id,
            EntryPayload::EngineChange {
                engine,
                plan_active,
            },
        )
        .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话自主程度的变更。
    ///
    /// 自主程度随时可改：它只影响之后确认由谁应答，不改动工具表与历史，
    /// 所以不受引擎锁定限制。
    pub async fn set_autonomy(
        &self,
        session_id: &str,
        autonomy: SessionAutonomy,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::AutonomyChange { autonomy })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录 plan 模式的进入或退出。
    pub async fn set_plan_active(
        &self,
        session_id: &str,
        active: bool,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::PlanMode { active })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 记录会话执行权限的变更。
    pub async fn set_permission(
        &self,
        session_id: &str,
        permission: SessionPermission,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::PermissionChange { permission })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }
    /// 记录会话模型的变更，并清除旧模型专用的回放数据。
    pub async fn set_model(
        &self,
        session_id: &str,
        model: String,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.append(session_id, EntryPayload::ModelChange { model })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 取会话写租约；已持有则直接返回。
    ///
    /// 只读打开过的会话升级为写模式时，先拿到租约再对齐磁盘：租约之前的写入可能来自
    /// 其它进程，内存状态必须重新从 transcript 重建。
    pub async fn ensure_write_lease(
        &self,
        session_id: &str,
        record: &Arc<SessionRecord>,
        frontend: &str,
    ) -> Result<(), CoreError> {
        if !self.locking {
            return Ok(());
        }
        if record.lease.lock().is_some() {
            return Ok(());
        }
        let _gate = record.lease_gate.lock().await;
        if record.lease.lock().is_some() {
            return Ok(());
        }
        // 锁文件和 transcript 在同一目录；延迟写盘时目录可能还不存在，先创建。
        if let Some(directory) = record.paths.transcript.parent() {
            tokio::fs::create_dir_all(directory).await?;
        }
        let lease = lock::SessionLockGuard::acquire(record.paths.lock_file(session_id), frontend)?;
        let _append = record.append_lock.lock().await;
        let file_slot = record.file.lock().await;
        if let Some(file) = file_slot.as_ref() {
            // 崩溃可能留下半条记录：拿到写租约后先截掉残尾，否则下一次追加会把它粘成坏行。
            // 只读打开绝不修改文件，所以修复只发生在这里。
            file.truncate_partial_tail().await?;
        }
        if let Some(file) = file_slot.as_ref()
            && file.changed_on_disk().await?
        {
            let entries = self.store.read_entries(&record.paths).await?;
            let mut fresh = SessionState::from_entries(entries)?;
            file.reopen().await?;
            let mut current = record.state.lock().await;
            fresh.notices = std::mem::take(&mut current.notices);
            fresh.job_sequence = current.job_sequence;
            fresh.server_compaction_disabled = current.server_compaction_disabled;
            *current = fresh;
        }
        *record.lease.lock() = Some(lease);
        Ok(())
    }

    /// 读回磁盘上的全部条目；`/retry` 用它裁剪上一轮没跑完的尾部。
    ///
    /// 返回原始 [`Entry`]（含 `finish_reason` 与条目 ID），这是内存里那份投影掉的
    /// 信息；重写 transcript 需要按原样写回其余条目。
    pub async fn read_entries(&self, session_id: &str) -> Result<Vec<Entry>, CoreError> {
        let record = self.get(session_id).await?;
        self.store.read_entries(&record.paths).await
    }

    /// 释放本进程持有的写租约；返回是否确实持有。
    ///
    /// 不允许跨进程强夺：内核锁没有安全抢占方式，删掉别人正在用的锁文件只会让两个
    /// 进程同时写。需要写入就请持有者卸载会话或退出进程。
    pub fn release_lease(&self, record: &SessionRecord) -> bool {
        record.lease.lock().take().is_some()
    }

    /// 会话当前写租约的持有者投影：本进程持有则直接给出，否则探测其它进程。
    pub fn lease_info(&self, session_id: &str, record: &SessionRecord) -> Option<SessionLockInfo> {
        let held = record
            .lease
            .lock()
            .as_ref()
            .map(lock::SessionLockGuard::info);
        held.or_else(|| lock::read(record.paths.lock_file(session_id)))
    }

    /// 从内存表里卸载会话，不删除 JSONL 和 sidecar。
    pub async fn unload(&self, session_id: &str) -> Result<(), CoreError> {
        let record = self.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        self.unload_locked(session_id, &record);
        Ok(())
    }

    /// 调用者必须持有该 record 的提示锁，防止已经拿到句柄的请求继续写入。
    pub(crate) fn unload_locked(&self, session_id: &str, record: &SessionRecord) {
        record.active.store(false, Ordering::Release);
        self.release_lease(record);
        self.records.write().remove(session_id);
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), CoreError> {
        let record = self.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        self.ensure_write_lease(session_id, &record, "moc-core")
            .await?;
        self.delete_locked(session_id, &record).await
    }

    pub(crate) async fn delete_locked(
        &self,
        session_id: &str,
        record: &SessionRecord,
    ) -> Result<(), CoreError> {
        let _append = record.append_lock.lock().await;
        // 删除期间保持租约：先让文件从磁盘消失，再放开内核锁，否则另一个进程
        // 可能在 release 与 delete 之间抢到租约并开始写一个正在被删除的会话。
        self.store.delete(session_id, &record.paths).await?;
        self.release_lease(record);
        self.unload_locked(session_id, record);
        Ok(())
    }

    pub fn locking_enabled(&self) -> bool {
        self.locking
    }
}

fn validate_plugin_state_component(value: &str, label: &str) -> Result<(), CoreError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || value.as_bytes()[0].is_ascii_digit()
        || value.as_bytes()[0] == b'-'
    {
        return Err(CoreError::Session(format!(
            "插件状态 {label} 不合法：需为 1-64 位小写字母、数字或连字符，且以字母开头"
        )));
    }
    Ok(())
}

fn validate_plugin_state_key(plugin: &str, key: &str) -> Result<(), CoreError> {
    validate_plugin_state_component(plugin, "名")?;
    if key.is_empty() || key.len() > 128 {
        return Err(CoreError::Session(
            "插件状态 key 必须是 1..128 UTF-8 字节".into(),
        ));
    }
    Ok(())
}

fn message_bytes(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => text.len(),
            ContentPart::Image { .. } => 32,
            ContentPart::Reasoning(ReasoningBlock::Text { text, .. }) => text.len(),
            ContentPart::Reasoning(ReasoningBlock::Redacted { data }) => data.len(),
            ContentPart::Reasoning(ReasoningBlock::Item(_)) => 64,
            ContentPart::ToolCall(call) => call.id.len() + call.name.len() + call.arguments.len(),
            ContentPart::ToolResult(result) => result.tool_call_id.len() + result.content.len(),
        })
        .sum::<usize>()
        .saturating_add(32)
}

fn project_message(message: &Message, budget: usize) -> (Message, bool) {
    let mut remaining = budget;
    let mut truncated = false;
    let mut content = Vec::with_capacity(message.content.len());
    for part in &message.content {
        let projected = match part {
            ContentPart::Text(text) => {
                let (text, was_truncated) = truncate_text(text, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(text.len());
                ContentPart::Text(text)
            }
            ContentPart::Image { .. } => {
                truncated = true;
                ContentPart::Text("[图片内容已省略]".into())
            }
            ContentPart::Reasoning(ReasoningBlock::Text { text, .. }) => {
                let (text, was_truncated) = truncate_text(text, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(text.len());
                ContentPart::Reasoning(ReasoningBlock::text(text))
            }
            ContentPart::Reasoning(ReasoningBlock::Redacted { data }) => {
                let (data, was_truncated) = truncate_text(data, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(data.len());
                ContentPart::Reasoning(ReasoningBlock::Redacted { data })
            }
            ContentPart::Reasoning(ReasoningBlock::Item(_)) => {
                truncated = true;
                ContentPart::Reasoning(ReasoningBlock::text("（思维内容已省略）"))
            }
            ContentPart::ToolCall(call) => {
                let (arguments, was_truncated) = truncate_text(&call.arguments, remaining);
                truncated |= was_truncated;
                remaining = remaining.saturating_sub(arguments.len());
                ContentPart::ToolCall(ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments,
                })
            }
            ContentPart::ToolResult(result) => {
                let (content, was_truncated) = truncate_text(&result.content, remaining);
                truncated |= was_truncated || !result.images.is_empty();
                remaining = remaining.saturating_sub(content.len());
                ContentPart::ToolResult(ToolResult {
                    tool_call_id: result.tool_call_id.clone(),
                    content,
                    is_error: result.is_error,
                    images: Vec::new(),
                })
            }
        };
        content.push(projected);
    }
    (
        Message {
            role: message.role,
            content,
        },
        truncated,
    )
}

fn truncate_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    const MARKER: &str = "…[内容已截断]…";
    if limit <= MARKER.len() {
        return (MARKER.chars().take(limit).collect(), true);
    }
    let available = limit - MARKER.len();
    let mut head = available / 2;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = available - head;
    while tail > 0 && !text.is_char_boundary(text.len() - tail) {
        tail -= 1;
    }
    let mut output = String::with_capacity(limit);
    output.push_str(&text[..head]);
    output.push_str(MARKER);
    output.push_str(&text[text.len() - tail..]);
    (output, true)
}

fn sanitize_title(title: &str) -> String {
    title
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(60)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// 孤儿标记文件路径：父会话 sidecar 下每行一个被孤立子代理的 id。
fn orphans_path(paths: &SessionPaths) -> PathBuf {
    paths.sidecar.join("orphaned_agents")
}

/// 读取持久化的孤儿标记；文件不存在或读不出都按空集合处理——最坏情况是
/// 孤儿重新出现在名册里，与未打标记的行为一致，不会误伤正常子代理。
async fn load_orphans(paths: &SessionPaths) -> BTreeSet<String> {
    match tokio::fs::read_to_string(orphans_path(paths)).await {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// 把孤儿标记写回磁盘：空集合删除文件；非空按临时文件 + rename 原子发布，
/// 和 transcript rewrite 同一套崩溃语义。
async fn persist_orphans(paths: &SessionPaths, ids: &BTreeSet<String>) -> Result<(), CoreError> {
    let file = orphans_path(paths);
    if ids.is_empty() {
        match tokio::fs::remove_file(&file).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(CoreError::Io(error)),
        }
    }
    tokio::fs::create_dir_all(&paths.sidecar).await?;
    let temp = file.with_extension(format!("tmp-{}", crate::ids::entry_id()));
    let mut text = ids.iter().cloned().collect::<Vec<_>>().join("\n");
    text.push('\n');
    tokio::fs::write(&temp, text).await?;
    tokio::fs::rename(&temp, &file).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use moc_llm::{ContentPart, Role, ToolCall};
    use moc_protocol::methods::{
        AgentMessageMeta, AgentMessageOutcome, SessionEngine, SessionRenameParams, ToolStatus,
    };

    use super::*;

    /// 引用收集：工具结果 JSON 任意深度的字符串 `agent_id` 与代理消息 `from_id`
    /// 都算引用；非法 JSON、非字符串值被忽略。
    #[test]
    fn referenced_agent_ids_collects_nested_results_and_agent_meta() {
        let tool_entry = |content: &str| {
            Entry::from_payload(
                EntryPayload::Message {
                    message: Message {
                        role: Role::Tool,
                        content: vec![ContentPart::ToolResult(ToolResult {
                            tool_call_id: "call_1".into(),
                            content: content.into(),
                            is_error: false,
                            images: Vec::new(),
                        })],
                    },
                    tool: None,
                    finish_reason: None,
                    agent: None,
                },
                "e0".into(),
                0,
            )
        };
        let agent_entry = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("ping"),
                tool: None,
                finish_reason: None,
                agent: Some(AgentMessageMeta {
                    from_id: "ses_sender".into(),
                    from_title: "sender".into(),
                    delivery: AgentMessageOutcome::Injected,
                }),
            },
            "e1".into(),
            0,
        );
        let entries = vec![
            tool_entry(r#"{"agents":[{"agent_id":"ses_a"}],"errors":[]}"#),
            tool_entry(r#"{"nested":{"items":[{"agent_id":"ses_b"}]}}"#),
            tool_entry("not json"),
            tool_entry(r#"{"agent_id":123}"#),
            agent_entry,
        ];
        let ids = entries::referenced_agent_ids(&entries);
        assert_eq!(
            ids.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["ses_a", "ses_b", "ses_sender"],
            "只收集字符串 agent_id 与 agent.from_id：{ids:?}"
        );
    }

    async fn create_plugin_state_session(table: &SessionTable) -> SessionInfo {
        table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话")
    }

    #[tokio::test]
    async fn plugin_state_isolated_and_persistent_without_transcript_entries() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let session = create_plugin_state_session(&table).await;
        table
            .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
            .await
            .expect("写插件状态");
        assert_eq!(
            table
                .plugin_state_get(&session.id, "demo", "count")
                .await
                .expect("读插件状态"),
            Some(serde_json::json!(1))
        );
        assert_eq!(
            table
                .plugin_state_keys(&session.id, "other")
                .await
                .expect("读另一个插件状态"),
            Vec::<String>::new()
        );
        let record = table.get(&session.id).await.expect("会话记录");
        assert_eq!(
            record.state.lock().await.entries.len(),
            1,
            "状态不进入 transcript"
        );
        let paths = record.paths.clone();
        table.unload(&session.id).await.expect("卸载会话");
        let restored = table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .expect("恢复插件状态");
        assert_eq!(restored, Some(serde_json::json!(1)));
        assert!(paths.plugin_state("demo").is_file());
    }

    #[tokio::test]
    async fn plugin_state_corruption_does_not_affect_other_plugins_or_session_load() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let session = create_plugin_state_session(&table).await;
        for plugin in ["demo", "other"] {
            table
                .plugin_state_set(&session.id, plugin, "count", serde_json::json!(1))
                .await
                .expect("保存插件状态");
        }
        let record = table.get(&session.id).await.expect("会话记录");
        tokio::fs::write(record.paths.plugin_state("demo"), b"{broken")
            .await
            .expect("损坏单个插件状态文件");
        assert!(
            table
                .plugin_state_get(&session.id, "demo", "count")
                .await
                .is_err()
        );
        assert!(table.plugin_state_keys(&session.id, "demo").await.is_err());
        table.unload(&session.id).await.expect("卸载会话");
        table
            .load(&session.id, None)
            .await
            .expect("损坏状态不影响恢复会话");
        table
            .plugin_state_set(&session.id, "other", "count", serde_json::json!(2))
            .await
            .expect("另一个插件仍可写入");
        assert_eq!(
            table
                .plugin_state_get(&session.id, "other", "count")
                .await
                .expect("读取其它插件"),
            Some(serde_json::json!(2))
        );
        assert!(
            table.read_plugin_state(&record, "other").await.is_err(),
            "已卸载的记录不能读取状态"
        );
    }

    #[tokio::test]
    async fn plugin_state_limit_preserves_previous_file_and_bounds_reads() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let session = create_plugin_state_session(&table).await;
        table
            .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
            .await
            .expect("保存原状态");
        let record = table.get(&session.id).await.expect("会话记录");
        let path = record.paths.plugin_state("demo");
        let before = tokio::fs::read(&path).await.expect("原文件字节");
        assert!(
            table
                .plugin_state_set(
                    &session.id,
                    "demo",
                    "count",
                    serde_json::json!("x".repeat(1024 * 1024))
                )
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(&path).await.expect("写入失败后的文件"),
            before
        );
        assert_eq!(
            table
                .plugin_state_get(&session.id, "demo", "count")
                .await
                .expect("原状态仍可读取"),
            Some(serde_json::json!(1))
        );
        tokio::fs::write(
            record.paths.plugin_state("other"),
            vec![b'x'; 1024 * 1024 + 1],
        )
        .await
        .expect("外部写入超限文件");
        assert!(
            table
                .plugin_state_keys(&session.id, "other")
                .await
                .expect_err("读取也有容量上限")
                .to_string()
                .contains("1 MiB")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&path)
                    .await
                    .expect("文件权限")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn plugin_state_survives_in_place_rewind_and_reset() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let session = create_plugin_state_session(&table).await;
        table
            .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
            .await
            .expect("保存原会话状态");
        let message = table
            .append_message(&session.id, Message::user_text("回退目标"), None, None)
            .await
            .expect("追加用户消息");
        let rewound = table
            .rewind(&session.id, &TranscriptEntryId::new(message.id()))
            .await
            .expect("原地回退");
        assert_eq!(rewound.session.id, session.id, "回退不产生新会话");
        assert_eq!(
            table
                .plugin_state_get(&session.id, "demo", "count")
                .await
                .expect("状态随会话保留"),
            Some(serde_json::json!(1))
        );
        table
            .append(&session.id, EntryPayload::ResetBoundary)
            .await
            .expect("清空会话上下文");
        assert_eq!(
            table
                .plugin_state_get(&session.id, "demo", "count")
                .await
                .expect("ResetBoundary 不动插件状态"),
            Some(serde_json::json!(1))
        );
        let record = table.get(&session.id).await.expect("会话记录");
        let path = record.paths.plugin_state("demo");
        table.delete(&session.id).await.expect("删除会话");
        assert!(!path.exists(), "删除会话会清理插件状态");
    }

    /// 子代理会话整个放在父会话目录里：transcript 和父会话的 sidecar 同目录，自己的
    /// sidecar 是它的子目录。恢复后父链接和来源都在，删除父会话会一并删除子树。
    #[tokio::test]
    async fn spawned_child_session_lives_inside_the_parent_directory() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let parent = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建父会话");
        table
            .append_message(&parent.id, Message::user_text("父"), None, None)
            .await
            .expect("追加父消息");
        let child = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                Some("子任务"),
                SessionOrigin::spawned(parent.id.clone(), 1, None),
                true,
            )
            .await
            .expect("创建子代理会话");
        table
            .append_message(&child.id, Message::user_text("子"), None, None)
            .await
            .expect("追加子消息");

        let parent_paths = table.get(&parent.id).await.expect("父记录").paths.clone();
        let child_paths = table.get(&child.id).await.expect("子记录").paths.clone();
        assert!(!parent_paths.compressed());
        assert!(child_paths.compressed(), "子代理按本次有效配置选择格式");
        assert_eq!(
            child_paths.transcript.parent(),
            Some(parent_paths.sidecar.as_path())
        );
        assert_eq!(child_paths.sidecar, parent_paths.sidecar.join(&child.id));
        assert!(child_paths.transcript.exists(), "子代理日志已经落盘");

        // 代理树由会话头恢复：列表同时收录两者，子代理仍带父链接。
        let listed = table.list().await.expect("列出会话");
        let child_info = listed
            .iter()
            .find(|info| info.id == child.id)
            .expect("子代理在列表里");
        assert_eq!(
            child_info.parent_session_id.as_deref(),
            Some(parent.id.as_str())
        );

        table.delete(&parent.id).await.expect("删除父会话");
        assert!(!parent_paths.sidecar.exists());
        assert!(!child_paths.transcript.exists());
    }

    #[tokio::test]
    async fn fresh_session_is_deferred_to_disk_until_first_entry() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        // 还没产生 entry 时，磁盘上既没有 transcript 也没有 sidecar，列表里也不会出现。
        let paths = table.store.paths_for(
            std::path::Path::new("/tmp/project"),
            &info.id,
            info.created_at_ms,
        );
        assert!(!paths.transcript.exists());
        assert!(table.list().await.expect("列出会话").is_empty());
        // append 之后：header 和 entry 一起写盘，列表里能看到。
        table
            .append_message(&info.id, Message::user_text("第一条消息"), None, None)
            .await
            .expect("追加消息");
        assert!(paths.transcript.exists());
        let entries = table
            .store
            .read_entries(&paths)
            .await
            .expect("读取 transcript");
        assert_eq!(entries.len(), 2, "header 加一条用户消息");
        assert_eq!(table.list().await.expect("列出会话").len(), 1);
    }

    #[tokio::test]
    async fn compressed_sessions_use_zstd_only_for_new_sessions() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let compressed = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                true,
            )
            .await
            .expect("创建压缩会话");
        table
            .append_message(&compressed.id, Message::user_text("压缩消息"), None, None)
            .await
            .expect("追加消息");
        let record = table.get(&compressed.id).await.expect("取会话");
        assert_eq!(
            record
                .paths
                .transcript
                .extension()
                .and_then(|value| value.to_str()),
            Some("zst")
        );
        let bytes = std::fs::read(&record.paths.transcript).expect("读取日志");
        assert_eq!(
            &bytes[..4],
            &[0x28, 0xb5, 0x2f, 0xfd],
            "文件应以 zstd magic 开头"
        );
        let loaded = table
            .load(&compressed.id, None)
            .await
            .expect("加载压缩会话");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].message.text(), "压缩消息");
        assert_eq!(table.list().await.expect("列出会话").len(), 1);
        assert!(
            table
                .store
                .find(&compressed.id)
                .await
                .expect("查找会话")
                .is_some(),
            "压缩日志也要能被 find 发现"
        );

        // 整表重写仍然是一个压缩文件，格式不能退回明文。
        let entries = table
            .store
            .read_entries(&record.paths)
            .await
            .expect("读取 entries");
        table
            .rewrite(&compressed.id, entries)
            .await
            .expect("压缩会话原子重写");
        let bytes = std::fs::read(&record.paths.transcript).expect("读取重写后的日志");
        assert_eq!(&bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
        assert_eq!(
            table
                .load(&compressed.id, None)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );

        // 关闭开关只影响以后创建的会话；已有会话保持压缩格式。
        let plain = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建明文会话");
        table
            .append_message(&plain.id, Message::user_text("明文消息"), None, None)
            .await
            .expect("追加明文消息");
        let plain_record = table.get(&plain.id).await.expect("取明文会话");
        assert_eq!(
            plain_record
                .paths
                .transcript
                .extension()
                .and_then(|value| value.to_str()),
            Some("jsonl")
        );
        assert_eq!(table.list().await.expect("列出会话").len(), 2);
        table.delete(&compressed.id).await.expect("删除压缩会话");
        assert!(!record.paths.transcript.exists());
    }

    #[tokio::test]
    async fn compressed_write_lease_truncates_a_torn_frame() {
        use tokio::io::AsyncWriteExt;

        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                true,
            )
            .await
            .expect("创建压缩会话");
        table
            .append_message(&info.id, Message::user_text("第一条"), None, None)
            .await
            .expect("追加第一条");
        let record = table.get(&info.id).await.expect("取会话");
        table
            .ensure_write_lease(&info.id, &record, "tui")
            .await
            .expect("取得写租约");
        // 模拟崩溃：只写出下一帧的前半段。
        let frame = crate::session::log::compress_frame(b"{\"partial\":\n").expect("压缩半帧");
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&record.paths.transcript)
            .await
            .expect("打开日志");
        file.write_all(&frame[..frame.len() / 2])
            .await
            .expect("写入半帧");
        file.sync_all().await.expect("刷写半帧");
        assert!(table.release_lease(&record));
        table
            .ensure_write_lease(&info.id, &record, "tui")
            .await
            .expect("重新取得租约时截掉残帧");
        table
            .append_message(&info.id, Message::user_text("第二条"), None, None)
            .await
            .expect("截断后继续追加");
        let loaded = table.load(&info.id, None).await.expect("加载");
        assert_eq!(loaded.entries.len(), 2);
        assert_eq!(loaded.entries[0].message.text(), "第一条");
        assert_eq!(loaded.entries[1].message.text(), "第二条");
    }

    #[tokio::test]
    async fn session_lifecycle_persists_transcript_and_true_truncation() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model:high",
                SessionEngine::Standard,
                SessionPermission::Full,
                Some("  标题  "),
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        assert_eq!(info.title, "标题");
        for text in ["第一条", "第二条", "第三条"] {
            table
                .append_message(&info.id, Message::user_text(text), None, None)
                .await
                .expect("追加消息");
        }
        let loaded = table.load(&info.id, Some(2)).await.expect("限制加载");
        assert!(loaded.truncated);
        assert_eq!(loaded.entries.len(), 2);
        assert!(matches!(
            &loaded.entries[0].message.content[0],
            moc_llm::ContentPart::Text(text) if text == "第二条"
        ));
        table.unload(&info.id).await.expect("卸载");
        let loaded = table.load(&info.id, None).await.expect("从磁盘重载");
        assert!(!loaded.truncated);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(loaded.session.engine, SessionEngine::Standard);
        let renamed = table
            .rename(
                &info.id,
                SessionRenameParams {
                    session_id: info.id.clone(),
                    title: "新标题\n第二行".into(),
                },
            )
            .await
            .expect("重命名");
        assert_eq!(renamed.title, "新标题");
        table.unload(&info.id).await.expect("再次卸载");
        assert_eq!(
            table
                .load(&info.id, None)
                .await
                .expect("重读标题")
                .session
                .title,
            "新标题"
        );
        table.delete(&info.id).await.expect("删除");
        assert!(table.load(&info.id, None).await.is_err());
    }

    #[tokio::test]
    async fn permission_changes_persist_and_reload() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        assert_eq!(info.permission, moc_protocol::SessionPermission::Full);
        table
            .append_message(&info.id, Message::user_text("落盘"), None, None)
            .await
            .expect("追加消息");
        let changed = table
            .set_permission(&info.id, moc_protocol::SessionPermission::Workspace)
            .await
            .expect("切换权限");
        assert_eq!(
            changed.permission,
            moc_protocol::SessionPermission::Workspace
        );
        assert_eq!(
            table.list().await.expect("列出会话")[0].permission,
            moc_protocol::SessionPermission::Workspace
        );
        table.unload(&info.id).await.expect("卸载");
        let loaded = table.load(&info.id, None).await.expect("重载");
        assert_eq!(
            loaded.session.permission,
            moc_protocol::SessionPermission::Workspace
        );
        table
            .set_permission(&info.id, moc_protocol::SessionPermission::Full)
            .await
            .expect("切回权限");
        table.unload(&info.id).await.expect("再次卸载");
        assert_eq!(
            table
                .load(&info.id, None)
                .await
                .expect("再次重载")
                .session
                .permission,
            moc_protocol::SessionPermission::Full
        );
    }

    #[tokio::test]
    async fn prompt_and_file_locks_reject_parallel_writers() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        table
            .append_message(&info.id, Message::user_text("落盘"), None, None)
            .await
            .expect("追加消息");
        let record = table.get(&info.id).await.expect("取会话");
        let _prompt = record.try_prompt_lock().expect("取得提示锁");
        assert!(record.try_prompt_lock().is_err());
        table
            .ensure_write_lease(&info.id, &record, "tui")
            .await
            .expect("取得写租约");
        // 同进程重复取租约是幂等的：跨进程互斥由内核文件锁负责，进程内写互斥由提示锁负责。
        table
            .ensure_write_lease(&info.id, &record, "webui")
            .await
            .expect("重复取租约");
        assert!(table.release_lease(&record));
        assert!(!table.release_lease(&record));
        table
            .ensure_write_lease(&info.id, &record, "webui")
            .await
            .expect("释放后可再取");
    }

    #[tokio::test]
    async fn a_second_table_cannot_take_the_write_lease_while_the_first_holds_it() {
        let directory = tempfile::tempdir().expect("临时目录");
        let first = SessionTable::new(directory.path(), true);
        let info = first
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        first
            .append_message(&info.id, Message::user_text("写者"), None, None)
            .await
            .expect("追加消息");
        let first_record = first.get(&info.id).await.expect("取会话");
        first
            .ensure_write_lease(&info.id, &first_record, "tui")
            .await
            .expect("取得写租约");

        // 只读加载不取租约，因此在别人持锁时仍然成功。
        let second = SessionTable::new(directory.path(), true);
        let loaded = second.load(&info.id, None).await.expect("只读加载");
        assert_eq!(loaded.entries[0].message.text(), "写者");
        assert_eq!(
            loaded
                .session
                .lock
                .as_ref()
                .map(|lock| lock.frontend.as_str()),
            Some("tui"),
            "只读加载能看到持有者投影"
        );

        let second_record = second.get(&info.id).await.expect("取第二份会话");
        let error = second
            .ensure_write_lease(&info.id, &second_record, "webui")
            .await
            .expect_err("第二份会话不能取得写租约");
        assert!(
            error.to_string().contains("SESSION_LOCKED"),
            "错误码要能让前端降到只读：{error}"
        );

        assert!(first.release_lease(&first_record));
        second
            .ensure_write_lease(&info.id, &second_record, "webui")
            .await
            .expect("释放后可以接管");
    }

    #[tokio::test]
    async fn lock_refreshes_external_appends_and_replaced_transcript_handles() {
        let directory = tempfile::tempdir().unwrap();
        let first = SessionTable::new(directory.path(), true);
        let info = first
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .unwrap();
        first
            .append_message(&info.id, Message::user_text("落盘"), None, None)
            .await
            .unwrap();
        let second = SessionTable::new(directory.path(), true);
        second.load(&info.id, None).await.unwrap();
        let first_record = first.get(&info.id).await.unwrap();
        first
            .ensure_write_lease(&info.id, &first_record, "first")
            .await
            .unwrap();
        first
            .append_message(&info.id, Message::user_text("外部追加"), None, None)
            .await
            .unwrap();
        first
            .rename(
                &info.id,
                SessionRenameParams {
                    session_id: info.id.clone(),
                    title: "跨进程新标题".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(second.list().await.unwrap()[0].title, "跨进程新标题");
        let entries = first_record.state.lock().await.entries.clone();
        first.rewrite(&info.id, entries).await.unwrap();
        assert!(first.release_lease(&first_record));
        let second_record = second.get(&info.id).await.unwrap();
        second
            .ensure_write_lease(&info.id, &second_record, "second")
            .await
            .unwrap();
        let loaded = second.load(&info.id, None).await.unwrap();
        assert_eq!(
            loaded.entries[1].message.text(),
            "外部追加",
            "取租约后应看到 other 进程追加的消息"
        );
        second
            .append_message(&info.id, Message::assistant_text("续写"), None, None)
            .await
            .unwrap();
        assert!(second.release_lease(&second_record));
        let restored = SessionTable::new(directory.path(), true)
            .load(&info.id, None)
            .await
            .unwrap();
        assert_eq!(restored.entries.len(), 3);
        assert_eq!(restored.entries[2].message.text(), "续写");
    }

    #[tokio::test]
    async fn engine_locks_after_first_prompt_not_before() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let create = |engine| {
            let table = &table;
            async move {
                table
                    .create(
                        "/tmp/project",
                        "p/model",
                        engine,
                        SessionPermission::Full,
                        None,
                        SessionOrigin::root(),
                        false,
                    )
                    .await
                    .expect("创建会话")
            }
        };
        // 尚未跑过提示的会话不锁定引擎（CLM 也一样）。
        let untouched = create(SessionEngine::ClmExperimental).await;
        table
            .set_engine(&untouched.id, SessionEngine::Standard)
            .await
            .expect("首次提示前允许切换引擎");
        // 无提示历史时混入的实时上下文不构成锁定理由。
        let forged = create(SessionEngine::Standard).await;
        table
            .append(
                &forged.id,
                EntryPayload::ClmContext {
                    content: "伪造的实时上下文".into(),
                },
            )
            .await
            .expect("写入实时上下文");
        table
            .set_engine(&forged.id, SessionEngine::Ptc)
            .await
            .expect("无提示历史的会话不因异常数据锁引擎");
        // 跑过用户提示的会话引擎锁定；同引擎重设不报错。
        let clm = create(SessionEngine::ClmExperimental).await;
        table
            .append(
                &clm.id,
                EntryPayload::ClmContext {
                    content: "真实实时上下文".into(),
                },
            )
            .await
            .expect("写入实时上下文");
        table
            .append_message(&clm.id, Message::user_text("提示"), None, None)
            .await
            .expect("写入用户提示");
        let error = table
            .set_engine(&clm.id, SessionEngine::Standard)
            .await
            .expect_err("首次提示后引擎必须锁定");
        assert!(error.to_string().contains("引擎"), "{error}");
        table
            .set_engine(&clm.id, SessionEngine::ClmExperimental)
            .await
            .expect("同引擎重设不报错");
        // 自主程度不受引擎锁定限制：有提示历史后仍随时可改，且随会话持久化。
        let autonomy = table
            .set_autonomy(&clm.id, SessionAutonomy::Unattended)
            .await
            .expect("提示历史后可改自主程度");
        assert_eq!(autonomy.autonomy, SessionAutonomy::Unattended);
        let reloaded = table.load(&clm.id, None).await.expect("重载会话");
        assert_eq!(
            reloaded.session.autonomy,
            SessionAutonomy::Unattended,
            "自主程度必须随 transcript 持久化"
        );
    }
    #[tokio::test]
    async fn rewind_truncates_user_message_and_keeps_tool_history_complete() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        let first_user = table
            .append_message(&info.id, Message::user_text("第一条"), None, None)
            .await
            .expect("追加用户消息");
        let assistant = table
            .append_message(
                &info.id,
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall(ToolCall::new("call_1", "read", "{}"))],
                },
                None,
                None,
            )
            .await
            .expect("追加工具调用");
        table
            .append_message(
                &info.id,
                Message::tool_result("call_1", "结果", false),
                Some(ToolExecution {
                    tool_call_id: "call_1".into(),
                    status: ToolStatus::Completed,
                    duration_ms: Some(1),
                }),
                None,
            )
            .await
            .expect("追加工具结果");
        let second_user = table
            .append_message(&info.id, Message::user_text("第二条"), None, None)
            .await
            .expect("追加第二条用户消息");
        let second_user_id = TranscriptEntryId::new(second_user.id());
        let assistant_id = TranscriptEntryId::new(assistant.id());

        let rewound = table
            .rewind(&info.id, &second_user_id)
            .await
            .expect("回退到用户消息");
        assert_eq!(rewound.entries.len(), 3);
        assert_eq!(
            rewound.entries.last().expect("工具结果").message.role,
            Role::Tool
        );
        // 原地回退不产生新会话：id、header、出身全部保持原样。
        assert_eq!(rewound.session.id, info.id);
        assert!(rewound.session.parent_session_id.is_none());
        assert!(rewound.session.forked_from.is_none());
        // 截断已落盘：换一张表从磁盘重读，尾部确实不在了。
        let reloaded = SessionTable::new(directory.path().join("sessions"), true)
            .load(&info.id, None)
            .await
            .expect("从磁盘重新读取截断后的会话");
        assert_eq!(reloaded.entries.len(), 3);
        let original = table.load(&info.id, None).await.expect("读取会话");
        assert_eq!(original.entries.len(), 3);

        let rewound_again = table
            .rewind(&info.id, &assistant_id)
            .await
            .expect("回退到助手工具调用");
        assert_eq!(rewound_again.entries.len(), 3);
        assert_eq!(rewound_again.entries[0].id, first_user.id());
    }

    #[tokio::test]
    async fn redo_reattaches_rewound_tail_until_new_history_lands() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        let u1 = table
            .append_message(&info.id, Message::user_text("一"), None, None)
            .await
            .expect("一");
        table
            .append_message(&info.id, Message::assistant_text("答一"), None, None)
            .await
            .expect("答一");
        let u2 = table
            .append_message(&info.id, Message::user_text("二"), None, None)
            .await
            .expect("二");
        let a2 = table
            .append_message(&info.id, Message::assistant_text("答二"), None, None)
            .await
            .expect("答二");
        let u3 = table
            .append_message(&info.id, Message::user_text("三"), None, None)
            .await
            .expect("三");
        let a3 = table
            .append_message(&info.id, Message::assistant_text("答三"), None, None)
            .await
            .expect("答三");

        // 回退到 u2：尾部 [u2, a2, u3, a3] 压栈。
        table
            .rewind(&info.id, &TranscriptEntryId::new(u2.id()))
            .await
            .expect("回退到 u2");
        // 分次前滚：先到 a2，剩余 [u3, a3] 留在栈上并可被分页读取。
        let partial = table
            .redo(&info.id, &TranscriptEntryId::new(a2.id()))
            .await
            .expect("前滚到 a2");
        assert_eq!(partial.session.id, info.id);
        assert_eq!(partial.entries.len(), 4);
        assert_eq!(partial.entries[0].id, u1.id());
        let page = table
            .load_page(&info.id, None, 32, None)
            .await
            .expect("分页");
        assert_eq!(page.redo_entries.len(), 2);
        // 剩余尾部整体接回，条目保持原 id 与顺序。
        let full = table
            .redo(&info.id, &TranscriptEntryId::new(a3.id()))
            .await
            .expect("前滚到 a3");
        assert_eq!(full.entries.len(), 6);
        assert_eq!(full.entries[4].id, u3.id());
        assert_eq!(full.entries[5].id, a3.id());
        assert!(
            table
                .load_page(&info.id, None, 32, None)
                .await
                .expect("分页")
                .redo_entries
                .is_empty()
        );
        // 接回的内容已落盘：换一张表重读仍在。
        let reloaded = SessionTable::new(directory.path().join("sessions"), true)
            .load(&info.id, None)
            .await
            .expect("磁盘重读");
        assert_eq!(reloaded.entries.len(), 6);
        // 栈空后再前滚报错。
        assert!(
            table
                .redo(&info.id, &TranscriptEntryId::new(a3.id()))
                .await
                .is_err(),
            "栈空后前滚必须报错"
        );
        // 再次回退后写入新历史：废弃时间线断链，前滚被拒。
        table
            .rewind(&info.id, &TranscriptEntryId::new(u3.id()))
            .await
            .expect("再回退");
        table
            .append_message(&info.id, Message::user_text("新提问"), None, None)
            .await
            .expect("新历史");
        assert!(
            table
                .redo(&info.id, &TranscriptEntryId::new(u3.id()))
                .await
                .is_err(),
            "新历史落地后前滚必须报错"
        );
    }

    #[tokio::test]
    async fn redo_stack_survives_nested_rewinds() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        let u1 = table
            .append_message(&info.id, Message::user_text("一"), None, None)
            .await
            .expect("一");
        let a1 = table
            .append_message(&info.id, Message::assistant_text("答一"), None, None)
            .await
            .expect("答一");
        let u2 = table
            .append_message(&info.id, Message::user_text("二"), None, None)
            .await
            .expect("二");
        let a2 = table
            .append_message(&info.id, Message::assistant_text("答二"), None, None)
            .await
            .expect("答二");

        // 连续两次回退压两层：[[u2,a2], [u1,a1]]。
        table
            .rewind(&info.id, &TranscriptEntryId::new(u2.id()))
            .await
            .expect("回退到 u2");
        table
            .rewind(&info.id, &TranscriptEntryId::new(u1.id()))
            .await
            .expect("回退到 u1");
        let page = table
            .load_page(&info.id, None, 32, None)
            .await
            .expect("分页");
        assert_eq!(page.redo_entries.len(), 2, "只有栈顶一层可前滚：{page:?}");
        // 先接回 [u1,a1]：栈顶换成 [u2,a2]，它仍接在新 tip 上。
        let mid = table
            .redo(&info.id, &TranscriptEntryId::new(a1.id()))
            .await
            .expect("前滚一层");
        assert_eq!(mid.entries.len(), 2);
        let full = table
            .redo(&info.id, &TranscriptEntryId::new(a2.id()))
            .await
            .expect("再前滚一层");
        assert_eq!(full.entries.len(), 4);
        assert_eq!(full.entries[0].id, u1.id());
        assert_eq!(full.entries[3].id, a2.id());
    }

    #[tokio::test]
    async fn load_page_projects_large_messages_and_keeps_cursor_order() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .expect("创建会话");
        table
            .append_message(&info.id, Message::user_text("第一条"), None, None)
            .await
            .expect("追加第一条");
        table
            .append_message(
                &info.id,
                Message::user_text("x".repeat(200_000)),
                None,
                None,
            )
            .await
            .expect("追加大消息");
        let page = table
            .load_page(&info.id, None, 8, Some(1024))
            .await
            .expect("读取分页");
        assert_eq!(page.entries.len(), 2);
        assert!(page.content_truncated);
        assert!(page.entries[1].message.text().len() <= 1024);
        assert_eq!(page.entries[0].message.text(), "第一条");
        assert!(!page.has_more);
    }
}
