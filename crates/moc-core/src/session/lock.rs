//! 跨进程独占锁：POSIX `flock(2)` / Windows `LockFileEx`。
//!
//! 锁绑定打开的文件句柄，**进程死亡时由内核释放**，因此不需要 pid 存活检测、不需要
//! 过期回收，也不存在「删掉别人正在持有的锁」这种竞态。锁文件本身常驻不删：文件内容
//! 只是持有者投影（pid / frontend / 进程名 / 上锁时间），判定以内核锁为准。
//!
//! 会话使用它做**写租约**：一个 core 进程打开会话写模式即持有，直到卸载或进程退出；
//! 其它进程只能只读打开。`read` 通过尝试共享锁判断是否有人持有，而不是看文件是否存在。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use moc_protocol::methods::SessionLockInfo;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockRecord {
    pid: u32,
    frontend: String,
    process_name: Option<String>,
    acquired_at_ms: u64,
}

impl From<LockRecord> for SessionLockInfo {
    fn from(record: LockRecord) -> Self {
        Self {
            pid: record.pid,
            frontend: record.frontend,
            process_name: record.process_name,
            acquired_at_ms: record.acquired_at_ms,
        }
    }
}

/// 一个持有中的独占锁；`Drop` 关闭句柄即由内核释放。
pub struct SessionLockGuard {
    file: File,
    path: PathBuf,
    record: LockRecord,
}

impl SessionLockGuard {
    /// 非阻塞地获取独占锁；被其它进程持有时返回 `SESSION_LOCKED`。
    ///
    /// 同一进程对同一文件再次 `open` 得到的是另一个 open file description，
    /// 因此也会按「被持有」失败——会话层必须自己保证每个会话只取一次租约。
    pub fn acquire(path: impl AsRef<Path>, frontend: &str) -> Result<Self, CoreError> {
        let path = path.as_ref().to_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(fs4::TryLockError::WouldBlock) => {
                let holder = read_record(&mut file);
                return Err(conflict(holder));
            }
            Err(fs4::TryLockError::Error(error)) => return Err(CoreError::Io(error)),
        }
        let record = LockRecord {
            pid: std::process::id(),
            frontend: frontend.to_owned(),
            process_name: process_name(),
            acquired_at_ms: crate::session::store::now_ms(),
        };
        if let Err(error) = write_record(&mut file, &record) {
            let _ = fs4::FileExt::unlock(&file);
            return Err(error);
        }
        Ok(Self { file, path, record })
    }

    pub fn info(&self) -> SessionLockInfo {
        self.record.clone().into()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        // 关闭句柄同样释放；显式解锁让语义在 Drop 期间就确定下来。
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

/// 当前锁的持有者投影；无人持有（或文件不存在）时为 `None`。
///
/// 用共享锁探测而不是看文件是否存在：锁文件常驻，存在性不代表被持有。
pub fn read(path: impl AsRef<Path>) -> Option<SessionLockInfo> {
    let mut file = OpenOptions::new().read(true).open(path.as_ref()).ok()?;
    match fs4::FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let _ = fs4::FileExt::unlock(&file);
            None
        }
        Err(fs4::TryLockError::WouldBlock) => read_record(&mut file),
        Err(fs4::TryLockError::Error(_)) => None,
    }
}

/// 读取锁文件里的持有者记录；内容损坏时按「未知持有者」处理。
fn read_record(file: &mut File) -> Option<SessionLockInfo> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let line = bytes.split(|byte| *byte == b'\n').next()?;
    serde_json::from_slice::<LockRecord>(line)
        .ok()
        .map(Into::into)
}

/// 覆盖写入持有者记录并落盘；锁已经持有，截断期间不会有其它写者。
fn write_record(file: &mut File, record: &LockRecord) -> Result<(), CoreError> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    serde_json::to_writer(&mut *file, record)
        .map_err(|error| CoreError::Session(format!("写入会话锁失败: {error}")))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn conflict(holder: Option<SessionLockInfo>) -> CoreError {
    match holder {
        Some(info) => CoreError::Session(format!(
            "SESSION_LOCKED: pid {} ({}) 正在使用会话",
            info.pid, info.frontend
        )),
        None => CoreError::Session("SESSION_LOCKED: 会话正被其它进程使用".into()),
    }
}

fn process_name() -> Option<String> {
    std::fs::read_to_string("/proc/self/comm")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            std::env::current_exe().ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let guard = SessionLockGuard::acquire(&path, "tui").expect("取得锁");
        assert_eq!(guard.info().pid, std::process::id());
        assert_eq!(guard.info().frontend, "tui");
        assert!(
            SessionLockGuard::acquire(&path, "webui").is_err(),
            "同一文件只允许一个写租约"
        );
        drop(guard);
        assert!(
            SessionLockGuard::acquire(&path, "webui").is_ok(),
            "释放后可以重新取得"
        );
    }

    #[test]
    fn read_reports_the_live_holder_and_nothing_after_release() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let guard = SessionLockGuard::acquire(&path, "tui").expect("取得锁");
        let projected = read(&path).expect("有人持有时必须给出投影");
        assert_eq!(projected.pid, std::process::id());
        assert_eq!(projected.frontend, "tui");
        drop(guard);
        assert!(read(&path).is_none(), "释放后不得再报持有者");
        assert!(path.exists(), "锁文件应当常驻，不随释放删除");
    }

    #[test]
    fn leftover_record_without_a_holder_is_not_reported_as_locked() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let stale = LockRecord {
            pid: std::process::id().saturating_add(1_000_000),
            frontend: "crashed".into(),
            process_name: Some("dead".into()),
            acquired_at_ms: 1,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("写陈旧锁");
        write_record(&mut file, &stale).expect("写记录");
        drop(file);
        assert!(
            read(&path).is_none(),
            "没有内核锁的残留记录不得被当成持有者"
        );
        let guard = SessionLockGuard::acquire(&path, "webui").expect("残留文件不阻塞新租约");
        assert_eq!(read(&path).expect("新租约投影").frontend, "webui");
        drop(guard);
    }

    #[test]
    fn damaged_record_still_blocks_and_reports_unknown_holder() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let guard = SessionLockGuard::acquire(&path, "tui").expect("取得锁");
        std::fs::write(&path, b"not json").expect("写坏记录");
        let error = SessionLockGuard::acquire(&path, "webui")
            .err()
            .expect("锁仍然有效");
        assert!(error.to_string().contains("SESSION_LOCKED"), "{error}");
        drop(guard);
    }
}
