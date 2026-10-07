//! 会话表、追加式 transcript 与每会话提示互斥。

pub mod entries;
pub mod lock;
pub mod maintenance;
pub mod state;
pub mod store;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use moc_llm::{FinishReason, Message, Role};
use moc_protocol::methods::{
    SessionInfo, SessionLoadResult, SessionMode, SessionPermission, SessionRenameParams,
    ToolExecution,
};
use parking_lot::RwLock;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::CoreError;
use crate::ids;

use self::entries::{Entry, EntryPayload};
use self::state::SessionState;
use self::store::{SessionFile, SessionPaths, SessionStore, now_ms};

/// transcript 条目标识，避免与会话标识在内部调用中混淆。
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
    /// 磁盘上还没有 transcript 文件时为 `None`；首次 append 延迟落盘。
    file: Mutex<Option<SessionFile>>,
    header: Entry,
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

    /// 只登记内存状态；transcript 由首次 append 延迟落盘，空会话不残留文件。
    pub async fn create(
        &self,
        cwd: impl AsRef<Path>,
        model: &str,
        mode: SessionMode,
        title: Option<&str>,
        parent_session_id: Option<String>,
    ) -> Result<SessionInfo, CoreError> {
        let cwd = cwd.as_ref();
        let session_id = ids::session_id();
        let created_at_ms = now_ms();
        let title = title
            .map(sanitize_title)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "新会话".into());
        let header = Entry::header(
            session_id.clone(),
            cwd.to_string_lossy().into_owned(),
            model.to_owned(),
            mode,
            parent_session_id,
            title,
            created_at_ms,
        );
        let state = SessionState::new(header.clone())?;
        let paths = self.store.paths_for(cwd, &session_id, created_at_ms);
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
        // 归档恢复同样是写操作；与其他进程的归档/追加共享会话锁。
        let _disk = lock::SessionLockGuard::acquire(paths.lock_file(session_id), "load")?;
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
        let record = Arc::new(SessionRecord {
            state: Arc::new(Mutex::new(state)),
            prompt_lock: Arc::new(Mutex::new(())),
            append_lock: Mutex::new(()),
            active: AtomicBool::new(true),
            file: Mutex::new(Some(file)),
            header,
            paths,
        });
        let mut records = self.records.write();
        Ok(Arc::clone(
            records.entry(session_id.to_owned()).or_insert(record),
        ))
    }

    /// 磁盘日志为跨进程的唯一真相源，不用缓存状态覆盖外部更新。
    pub async fn list(&self) -> Result<Vec<SessionInfo>, CoreError> {
        self.store.list().await
    }

    /// 加载消息投影；非 transcript entries 只更新状态，不进入 entries 列表。
    pub async fn load(
        &self,
        session_id: &str,
        max_entries: Option<u32>,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let state = record.state.lock().await;
        let count = state.transcript.len();
        let start = max_entries
            .map(|limit| count.saturating_sub(limit as usize))
            .unwrap_or(0);
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
    /// 创建一个回退分支，并从目标消息重建该分支的上下文。
    ///
    /// 用户消息作为待重新编辑的起点，不写入回退分支；助手工具调用会连同
    /// 紧随其后的工具结果一并保留，避免留下不完整的工具历史。原会话保持不变。
    pub async fn rewind(
        &self,
        session_id: &str,
        entry_id: &TranscriptEntryId,
    ) -> Result<SessionLoadResult, CoreError> {
        let record = self.get(session_id).await?;
        let entries = {
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
            let Entry::Message { message, .. } = &state.entries[target] else {
                return Err(CoreError::Session(
                    "INVALID_PARAMS: 回退目标不是 transcript 消息".into(),
                ));
            };
            let mut end = if message.role == Role::User {
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
            state.entries[..end].to_vec()
        };
        let source = self.get(session_id).await?;
        let source_info = source.state.lock().await.session_info();
        let child_title = format!("回退：{}", source_info.title);
        let child_info = self
            .create(
                &source_info.cwd,
                &source_info.model,
                source_info.mode,
                Some(&child_title),
                Some(source_info.id.clone()),
            )
            .await?;
        let child = self.get(&child_info.id).await?;
        let child_header = child
            .state
            .lock()
            .await
            .entries
            .first()
            .cloned()
            .ok_or_else(|| CoreError::Session("回退分支缺少会话头部".into()))?;
        let mut child_entries = Vec::with_capacity(entries.len());
        child_entries.push(child_header);
        child_entries.extend(entries.into_iter().skip(1));
        let mut parent_id = child_entries[0].id().to_owned();
        for entry in &mut child_entries[1..] {
            entry.set_parent_id(parent_id.clone());
            parent_id = entry.id().to_owned();
        }
        self.rewrite(&child_info.id, child_entries).await?;
        self.load(&child_info.id, None).await
    }

    /// 追加新 entry；状态锁不跨越文件 I/O await。
    ///
    /// transcript 文件缺失时（新会话尚未落盘）先写入 header 再写 entry，
    /// 完成延迟落盘；因此从未产生 entry 的会话不会在磁盘上留下文件。
    pub async fn append(
        &self,
        session_id: &str,
        payload: EntryPayload,
    ) -> Result<Entry, CoreError> {
        let record = self.get(session_id).await?;
        // 独立任务拥有写入事务；请求 future 被丢弃也必须完成磁盘与内存提交。
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
                .expect("延迟落盘或已打开的句柄必然存在")
                .append(&entry, None)
                .await?;
            record.state.lock().await.apply_entry(entry.clone());
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
            },
        )
        .await
    }

    /// 原子替换 transcript，并同步重建内存投影。
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
                .expect("延迟落盘或已打开的句柄必然存在")
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

    /// 改名并追加可重放的标题事件。
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
        self.reject_foreign_lock(session_id, &record.paths)?;
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

    /// 追加会话模式变化。
    pub async fn set_mode(
        &self,
        session_id: &str,
        mode: SessionMode,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.reject_foreign_lock(session_id, &record.paths)?;
        self.append(session_id, EntryPayload::ModeChange { mode })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 追加会话命令执行权限变化。
    pub async fn set_permission(
        &self,
        session_id: &str,
        permission: SessionPermission,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.reject_foreign_lock(session_id, &record.paths)?;
        self.append(session_id, EntryPayload::PermissionChange { permission })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }
    /// 追加会话模型变化并清除旧模型专属的回放上下文。
    pub async fn set_model(
        &self,
        session_id: &str,
        model: String,
    ) -> Result<SessionInfo, CoreError> {
        let record = self.get(session_id).await?;
        self.reject_foreign_lock(session_id, &record.paths)?;
        self.append(session_id, EntryPayload::ModelChange { model })
            .await?;
        let mut session = record.state.lock().await.session_info();
        session.lock = lock::read(record.paths.lock_file(session_id));
        Ok(session)
    }

    /// 尝试取得跨进程写锁。
    pub async fn acquire_lock(
        &self,
        session_id: &str,
        frontend: &str,
    ) -> Result<Option<lock::SessionLockGuard>, CoreError> {
        let record = self.get(session_id).await?;
        if !self.locking {
            return Ok(None);
        }
        // 锁文件与 transcript 同目录；延迟落盘时该目录可能还不存在，先补建。
        if let Some(directory) = record.paths.transcript.parent() {
            tokio::fs::create_dir_all(directory).await?;
        }
        let guard = lock::SessionLockGuard::acquire(record.paths.lock_file(session_id), frontend)?;
        let _append = record.append_lock.lock().await;
        let file_slot = record.file.lock().await;
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
        Ok(Some(guard))
    }

    /// 释放当前进程锁或强制清除陈旧/他人锁。
    pub async fn unlock(&self, session_id: &str, force: bool) -> Result<bool, CoreError> {
        let paths = self
            .store
            .find(session_id)
            .await?
            .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {session_id}")))?;
        lock::unlock(paths.lock_file(session_id), force)
    }

    /// 从内存表卸载会话，不删除 JSONL 或 sidecar。
    pub async fn unload(&self, session_id: &str) -> Result<(), CoreError> {
        let record = self.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        self.unload_locked(session_id, &record);
        Ok(())
    }

    /// 调用者必须持有该 record 的提示_lock，防止已取得句柄的请求继续写入。
    pub(crate) fn unload_locked(&self, session_id: &str, record: &SessionRecord) {
        record.active.store(false, Ordering::Release);
        self.records.write().remove(session_id);
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), CoreError> {
        let record = self.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        let _file_lock = self.acquire_lock(session_id, "moc-core").await?;
        self.delete_locked(session_id, &record).await
    }

    pub(crate) async fn delete_locked(
        &self,
        session_id: &str,
        record: &SessionRecord,
    ) -> Result<(), CoreError> {
        let _append = record.append_lock.lock().await;
        self.reject_foreign_lock(session_id, &record.paths)?;
        lock::unlock(record.paths.lock_file(session_id), false)?;
        self.store.delete(session_id, &record.paths).await?;
        self.unload_locked(session_id, record);
        Ok(())
    }

    fn reject_foreign_lock(&self, session_id: &str, paths: &SessionPaths) -> Result<(), CoreError> {
        if let Some(info) = lock::read(paths.lock_file(session_id))
            && info.pid != std::process::id()
        {
            return Err(CoreError::Session(format!(
                "SESSION_LOCKED: pid {} 正在使用会话",
                info.pid
            )));
        }
        Ok(())
    }

    pub fn locking_enabled(&self) -> bool {
        self.locking
    }
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

#[cfg(test)]
mod tests {
    use moc_llm::{ContentPart, Role, ToolCall};
    use moc_protocol::methods::{SessionMode, SessionRenameParams, ToolStatus};

    use super::*;

    #[tokio::test]
    async fn fresh_session_is_deferred_to_disk_until_first_entry() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create("/tmp/project", "p/model", SessionMode::Normal, None, None)
            .await
            .expect("创建会话");
        // 未产生任何 entry 前：磁盘上既无 transcript 也无 sidecar，列表不收录。
        let paths = table.store.paths_for(
            std::path::Path::new("/tmp/project"),
            &info.id,
            info.created_at_ms,
        );
        assert!(!paths.transcript.exists());
        assert!(table.list().await.expect("列出会话").is_empty());
        // append 之后：header 与 entry 一并落盘，列表收录。
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
    async fn session_lifecycle_persists_transcript_and_true_truncation() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create(
                "/tmp/project",
                "p/model:high",
                SessionMode::Ask,
                Some("  标题  "),
                None,
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
        assert_eq!(loaded.session.mode, SessionMode::Ask);
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
            .create("/tmp/project", "p/model", SessionMode::Normal, None, None)
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
            .create("/tmp/project", "p/model", SessionMode::Normal, None, None)
            .await
            .expect("创建会话");
        table
            .append_message(&info.id, Message::user_text("落盘"), None, None)
            .await
            .expect("追加消息");
        let record = table.get(&info.id).await.expect("取会话");
        let _prompt = record.try_prompt_lock().expect("取得提示锁");
        assert!(record.try_prompt_lock().is_err());
        let guard = table
            .acquire_lock(&info.id, "tui")
            .await
            .expect("取得文件锁");
        assert!(guard.is_some());
        assert!(table.acquire_lock(&info.id, "webui").await.is_err());
        drop(guard);
        assert!(table.acquire_lock(&info.id, "webui").await.is_ok());
    }

    #[tokio::test]
    async fn lock_refreshes_external_appends_and_replaced_transcript_handles() {
        let directory = tempfile::tempdir().unwrap();
        let first = SessionTable::new(directory.path(), true);
        let info = first
            .create("/tmp/project", "p/model", SessionMode::Normal, None, None)
            .await
            .unwrap();
        first
            .append_message(&info.id, Message::user_text("落盘"), None, None)
            .await
            .unwrap();
        let second = SessionTable::new(directory.path(), true);
        second.load(&info.id, None).await.unwrap();
        let guard = first.acquire_lock(&info.id, "first").await.unwrap();
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
        let entries = first
            .get(&info.id)
            .await
            .unwrap()
            .state
            .lock()
            .await
            .entries
            .clone();
        first.rewrite(&info.id, entries).await.unwrap();
        drop(guard);
        let guard = second.acquire_lock(&info.id, "second").await.unwrap();
        let loaded = second.load(&info.id, None).await.unwrap();
        assert_eq!(
            loaded.entries[1].message.text(),
            "外部追加",
            "锁刷新后应看到 other 进程追加的消息"
        );
        second
            .append_message(&info.id, Message::assistant_text("续写"), None, None)
            .await
            .unwrap();
        drop(guard);
        let restored = SessionTable::new(directory.path(), true)
            .load(&info.id, None)
            .await
            .unwrap();
        assert_eq!(restored.entries.len(), 3);
        assert_eq!(restored.entries[2].message.text(), "续写");
    }
    #[tokio::test]
    async fn rewind_truncates_user_message_and_keeps_tool_history_complete() {
        let directory = tempfile::tempdir().expect("临时目录");
        let table = SessionTable::new(directory.path().join("sessions"), true);
        let info = table
            .create("/tmp/project", "p/model", SessionMode::Normal, None, None)
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
        assert_ne!(rewound.session.id, info.id);
        let original = table.load(&info.id, None).await.expect("读取原会话");
        assert_eq!(original.entries.len(), 4);

        let rewound_again = table
            .rewind(&info.id, &assistant_id)
            .await
            .expect("回退到助手工具调用");
        assert_eq!(rewound_again.entries.len(), 3);
        assert_eq!(rewound_again.entries[0].id, first_user.id());
    }
}
