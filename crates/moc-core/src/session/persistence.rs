use super::SessionRecord;
use super::SessionTable;
use super::archive;
use super::entries;
use super::entries::Entry;
use super::entries::EntryPayload;
use super::state::SessionState;
use super::store::SessionFile;
use super::store::now_ms;
use super::stored::StoredEntry;
use crate::error::CoreError;
use moc_llm::FinishReason;
use moc_llm::Message;
use moc_protocol::methods::ToolExecution;
use moc_i18n::t;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl SessionTable {
    /// 把内存里已缓冲、尚未落盘的条目（header 之后的簿记链）补写到刚创建的
    /// transcript；后续 append 的 parent_id 才不会指向没落盘的条目。
    pub(super) async fn flush_buffered_entries(
        record: &Arc<SessionRecord>,
        file: &SessionFile,
    ) -> Result<(), CoreError> {
        let buffered: Vec<Entry> = {
            let state = record.state.lock().await;
            state
                .entries
                .iter()
                .skip(1)
                .map(StoredEntry::to_entry)
                .collect()
        };
        for stored in &buffered {
            file.append(stored, None).await?;
        }
        Ok(())
    }

    pub(super) async fn ensure_transcript_header(
        &self,
        record: &Arc<SessionRecord>,
    ) -> Result<(), CoreError> {
        let mut file = record.file.lock().await;
        if file.is_none() {
            let created =
                SessionFile::create_with_header(&record.paths.transcript, &record.header).await?;
            Self::flush_buffered_entries(record, &created).await?;
            *file = Some(created);
        }
        Ok(())
    }

    /// 追加新 entry；持有状态锁时不跨文件 I/O 的 await。
    ///
    /// transcript 文件还不存在时（新会话尚未写盘），非消息条目只应用到内存：
    /// 纯配置变更（换模型/权限/引擎等）的空会话不会在磁盘上留下 JSONL 文件，
    /// 也不会出现在 `session/list` 里。第一条消息到来时才把整链
    /// （header + 已缓冲的簿记条目 + 本条消息）一并落盘。
    pub async fn append(
        &self,
        session_id: &str,
        payload: EntryPayload,
    ) -> Result<Entry, CoreError> {
        let record = self.get(session_id).await?;
        let result_lease = self.locking && matches!(payload, EntryPayload::AgentResult { .. });
        if result_lease {
            self.ensure_write_lease(session_id, &record, "moc-core")
                .await?;
        }
        record.pinned.store(true, Ordering::Release);
        // 写入事务放在独立任务里：即使请求的 future 被丢弃，磁盘和内存的提交也必须完成。
        tokio::spawn(async move {
            let _append = record.append_lock.lock().await;
            let _lease = if result_lease {
                Some(record.lease.lock().clone().ok_or_else(|| {
                    CoreError::Session(t!("core.err.lease_released").into_owned())
                })?)
            } else {
                None
            };
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
            if file_slot.is_none() && matches!(entry, Entry::Message { .. }) {
                let file =
                    SessionFile::create_with_header(&record.paths.transcript, &record.header)
                        .await?;
                // 内存里已缓冲的簿记条目按原序补写，保持 parent_id 链完整。
                Self::flush_buffered_entries(&record, &file).await?;
                *file_slot = Some(file);
            }
            if let Some(file) = file_slot.as_ref() {
                file.append(&entry, None).await?;
            }
            record.state.lock().await.apply_entry(entry.clone());
            let freeze_old = {
                let state = record.state.lock().await;
                matches!(
                    &entry,
                    Entry::Compaction { .. } | Entry::ResetBoundary { .. }
                ) || (matches!(&entry, Entry::Message { .. })
                    && (state.unfrozen_bytes >= archive::FREEZE_BYTES
                        || (state.transcript.len() > archive::HOT_MESSAGES
                            && state.transcript.len() % 1024 == 0)))
            };
            if freeze_old {
                let snapshot = record.state.lock().await.clone();
                let frozen = tokio::task::spawn_blocking(move || {
                    archive::prepare(&snapshot, 0..archive::cold_end(&snapshot))
                })
                .await;
                match frozen {
                    Ok(Ok(batch)) => batch.apply(&mut *record.state.lock().await),
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "检查点已提交，正文冻结失败，保留完整内存正文")
                    }
                    Err(error) => tracing::warn!(%error, "检查点已提交，正文冻结任务失败"),
                }
            }
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
                goal: None,
            },
        )
        .await
    }

    /// 追加一次完整的模型响应 entry（带模型与正文段时间窗等展示元信息）。
    pub async fn append_response(
        &self,
        session_id: &str,
        message: Message,
        finish_reason: FinishReason,
        meta: moc_protocol::ResponseMeta,
    ) -> Result<Entry, CoreError> {
        self.append(
            session_id,
            EntryPayload::Response {
                message,
                finish_reason,
                meta,
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
                goal: None,
            },
        )
        .await
    }

    /// 追加一条 goal 续跑消息 entry（带来源信息）。
    pub async fn append_goal_message(
        &self,
        session_id: &str,
        message: Message,
        goal: moc_protocol::methods::GoalRoundMeta,
    ) -> Result<Entry, CoreError> {
        self.append(
            session_id,
            EntryPayload::Message {
                message,
                tool: None,
                finish_reason: None,
                agent: None,
                goal: Some(goal),
            },
        )
        .await
    }

    /// 追加一条目标变更 entry；`None` 载荷是清除墓碑。
    pub async fn append_goal_change(
        &self,
        session_id: &str,
        goal: Option<entries::GoalSnapshot>,
    ) -> Result<Entry, CoreError> {
        self.append(session_id, EntryPayload::GoalChange { goal })
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
        self.rewrite_current(session_id, move |_| async move { Ok((entries, ())) })
            .await
    }

    /// 在同一追加事务内读取当前状态、准备替换条目并重写，避免快照覆盖并发追加。
    /// `prepare` 只用于本地处理与归档，不应等待模型；返回的附带结果在提交成功后交还。
    pub(crate) async fn rewrite_current<T, F, Fut>(
        &self,
        session_id: &str,
        prepare: F,
    ) -> Result<T, CoreError>
    where
        T: Send + 'static,
        F: FnOnce(SessionState) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(Vec<Entry>, T), CoreError>> + Send,
    {
        let record = self.get(session_id).await?;
        let session_id = session_id.to_owned();
        // 与 append 一样，提交开始后不随调用方取消，避免磁盘与内存只更新一边。
        tokio::spawn(async move {
            let _append = record.append_lock.lock().await;
            if !record.active.load(Ordering::Acquire) {
                return Err(CoreError::Session(
                    "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
                ));
            }
            let snapshot = record.state.lock().await.clone();
            let (entries, output) = prepare(snapshot).await?;
            let replay_entries = entries.clone();
            let mut state = tokio::task::spawn_blocking(move || archive::replay(replay_entries))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))??;
            if state.session.id != session_id {
                return Err(CoreError::Session(
                    "重写 transcript 的会话 ID 不匹配".into(),
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
            *current = state;
            Ok(output)
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }

    /// 读回全部条目；`/retry` 用它裁剪上一轮没跑完的尾部。
    ///
    /// 返回原始 [`Entry`]（含 `finish_reason` 与条目 ID），这是内存里那份投影掉的
    /// 信息；重写 transcript 需要按原样写回其余条目。
    /// 会话尚未落盘时磁盘上没有 transcript，退回内存里已缓冲的条目链。
    pub async fn read_entries(&self, session_id: &str) -> Result<Vec<Entry>, CoreError> {
        let record = self.get(session_id).await?;
        if record.file.lock().await.is_none() {
            let state = record.state.lock().await;
            return Ok(state.entries.iter().map(StoredEntry::to_entry).collect());
        }
        self.store.read_entries(&record.paths).await
    }
}
