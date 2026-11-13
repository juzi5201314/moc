use super::SessionRecord;
use super::SessionTable;
use super::cache_budget::state_cache_bytes;
use super::entries::Entry;
use super::entries::SessionOrigin;
use super::load_orphans;
use super::sanitize_title;
use super::state::SessionState;
use super::store::now_ms;
use super::stored::StoredEntry;
use crate::error::CoreError;
use crate::ids;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionPermission;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;

impl SessionTable {
    /// 只登记内存状态；transcript 在第一次 append 时才写盘，空会话不会留下文件。
    /// `compress` 由调用者传入本工作区的有效配置，格式在创建时固定。
    #[allow(clippy::too_many_arguments)]
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
                skills_compacting: AtomicBool::new(false),
                append_lock: Mutex::new(()),
                active: AtomicBool::new(true),
                pinned: AtomicBool::new(true),
                accessed: std::sync::atomic::AtomicU64::new(now_ms()),
                cache_bytes: 0,
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
            record.accessed.store(now_ms(), Ordering::Relaxed);
            return Ok(record);
        }
        self.load_record(session_id).await
    }

    // 热命中不携带磁盘加载和缓存驱逐代码；冷状态机不再内联到每个会话调用点。
    #[inline(never)]
    pub(super) async fn load_record(
        &self,
        session_id: &str,
    ) -> Result<Arc<SessionRecord>, CoreError> {
        let gate = {
            let mut loading = self.loading.lock();
            loading.retain(|_, gate| gate.strong_count() > 0);
            let slot = loading.entry(session_id.to_owned()).or_default();
            if let Some(gate) = slot.upgrade() {
                gate
            } else {
                let gate = Arc::new(Mutex::new(()));
                *slot = Arc::downgrade(&gate);
                gate
            }
        };
        let _loading = gate.lock().await;
        if let Some(record) = self.records.read().get(session_id).cloned() {
            record.accessed.store(now_ms(), Ordering::Relaxed);
            return Ok(record);
        }
        let paths = self
            .store
            .find(session_id)
            .await?
            .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {session_id}")))?;
        // 只读加载不碰写租约：写者持锁期间读者仍可查看已落盘的完整行。
        // 版本先于重放取样：读入期间有外部追加时，后续租约升级/列表查询必须失效。
        let file = self.store.open(&paths).await?;
        let state = self.store.read_state(&paths).await?;
        if state.session.id != session_id {
            return Err(CoreError::Session(format!(
                "transcript session_id {} 与文件名 {session_id} 不匹配",
                state.session.id
            )));
        }
        let header = state
            .entries
            .first()
            .map(StoredEntry::to_entry)
            .ok_or_else(|| CoreError::Session("transcript 缺少会话头部".into()))?;
        let orphaned_agents = load_orphans(&paths).await;
        let (state, cache_bytes) = tokio::task::spawn_blocking(move || {
            let bytes = state_cache_bytes(&state);
            (state, bytes)
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?;
        let record = Arc::new(SessionRecord {
            state: Arc::new(Mutex::new(state)),
            prompt_lock: Arc::new(Mutex::new(())),
            skills_compacting: AtomicBool::new(false),
            append_lock: Mutex::new(()),
            active: AtomicBool::new(true),
            pinned: AtomicBool::new(false),
            accessed: std::sync::atomic::AtomicU64::new(now_ms()),
            cache_bytes,
            file: Mutex::new(Some(file)),
            header,
            paths,
            lease: parking_lot::Mutex::new(None),
            lease_gate: Mutex::new(()),
            rewind_stash: parking_lot::Mutex::new(Vec::new()),
            orphaned_agents: parking_lot::Mutex::new(orphaned_agents),
        });
        let mut records = self.records.write();
        let record = Arc::clone(records.entry(session_id.to_owned()).or_insert(record));
        let mut idle: Vec<_> = records
            .iter()
            .filter(|(_, record)| !record.pinned.load(Ordering::Acquire))
            .map(|(id, record)| {
                (
                    id.clone(),
                    record.accessed.load(Ordering::Relaxed),
                    record.cache_bytes,
                )
            })
            .collect();
        idle.sort_unstable_by_key(|(_, accessed, _)| *accessed);
        let mut bytes = idle
            .iter()
            .fold(0usize, |sum, (_, _, size)| sum.saturating_add(*size));
        let mut count = idle.len();
        for (id, _, size) in idle {
            if count <= 4 && bytes <= 64 * 1024 * 1024 {
                break;
            }
            // Arc 只由表持有才可移除：在途 RPC、提示锁或工具仍使用时不能产生第二份 record。
            if records.get(&id).is_some_and(|record| {
                Arc::strong_count(record) == 1
                    && Arc::strong_count(&record.state) == 1
                    && Arc::strong_count(&record.prompt_lock) == 1
            }) {
                records.remove(&id);
                bytes = bytes.saturating_sub(size);
                count -= 1;
            }
        }
        Ok(record)
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
        let records: Vec<_> = self.records.read().values().cloned().collect();
        for record in records {
            // 与 append/rewrite 同一锁顺序，版本和状态必须来自同一次提交。
            let file = record.file.lock().await;
            if let Some(file) = file.as_ref() {
                let state = record.state.lock().await;
                self.store.cache_listing(
                    &record.paths.transcript,
                    file.revision(),
                    (state
                        .entries
                        .iter()
                        .any(|stored| stored.message_entry().is_some()))
                    .then(|| state.session_info()),
                );
            }
        }
        self.store.list().await
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
}
