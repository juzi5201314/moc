use super::SessionRecord;
use super::SessionTable;
use super::lock;
use crate::error::CoreError;
use moc_protocol::methods::SessionLockInfo;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl SessionTable {
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
            record.pinned.store(true, Ordering::Release);
            return Ok(());
        }
        if record.lease.lock().is_some() {
            return Ok(());
        }
        self.acquire_write_lease(session_id, record, frontend).await
    }

    // 已持有租约时只走上面的短路径，文件修复和外部写入重放共享同一实现。
    #[inline(never)]
    pub(super) async fn acquire_write_lease(
        &self,
        session_id: &str,
        record: &Arc<SessionRecord>,
        frontend: &str,
    ) -> Result<(), CoreError> {
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
            let changed = file.changed_on_disk().await?;
            if changed {
                file.reopen().await?;
            }
            // 崩溃可能留下半条记录：拿到写租约后先截掉残尾，否则下一次追加会把它粘成坏行。
            // 只读打开绝不修改文件，所以修复只发生在这里。
            file.truncate_partial_tail().await?;
            if changed {
                let mut fresh = self.store.read_state(&record.paths).await?;
                let mut current = record.state.lock().await;
                fresh.notices = std::mem::take(&mut current.notices);
                fresh.job_sequence = current.job_sequence;
                *current = fresh;
            }
        }
        record.pinned.store(true, Ordering::Release);
        *record.lease.lock() = Some(Arc::new(lease));
        Ok(())
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
        let held = record.lease.lock().as_ref().map(|lease| lease.info());
        held.or_else(|| lock::read(record.paths.lock_file(session_id)))
    }

    pub fn locking_enabled(&self) -> bool {
        self.locking
    }
}
