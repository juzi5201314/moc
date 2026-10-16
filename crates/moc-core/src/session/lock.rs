//! 基于 create_new 的跨进程会话锁。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use moc_protocol::methods::SessionLockInfo;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// 陈旧锁回收期间持有的认领文件后缀。
const RECLAIM_SUFFIX: &str = ".reclaim";
/// 认领被其它进程持有时重试读取锁记录的次数。
const RECLAIM_RETRIES: usize = 8;

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

/// 只在创建它的进程内释放对应的锁文件。
pub struct SessionLockGuard {
    path: PathBuf,
    record: LockRecord,
    identity: crate::hashline::SnapshotId,
}

impl SessionLockGuard {
    /// 原子创建锁；遇到已死亡进程遗留的锁时回收并重试一次。
    pub fn acquire(path: impl AsRef<Path>, frontend: &str) -> Result<Self, CoreError> {
        let path = path.as_ref().to_owned();
        for _ in 0..2 {
            let record = LockRecord {
                pid: std::process::id(),
                frontend: frontend.to_owned(),
                process_name: process_name(),
                acquired_at_ms: crate::session::store::now_ms(),
            };
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let current = read_record(&path)?;
                    let Some(current) = current else {
                        return Err(CoreError::Session(
                            "SESSION_LOCKED: 锁正在写入或已经损坏，需要显式 force 解锁".into(),
                        ));
                    };
                    if pid_is_alive(current.pid) {
                        return Err(CoreError::Session(format!(
                            "SESSION_LOCKED: pid {} ({}) 正在使用会话",
                            current.pid, current.frontend
                        )));
                    }
                    reclaim(&path)?;
                    continue;
                }
                Err(error) => return Err(CoreError::Io(error)),
            };
            let write_result = match serde_json::to_writer(&mut file, &record) {
                Ok(()) => file.write_all(b"\n").map_err(CoreError::Io),
                Err(error) => Err(CoreError::Session(format!("写入会话锁失败: {error}"))),
            }
            .and_then(|()| file.sync_all().map_err(CoreError::Io));
            if let Err(error) = write_result {
                drop(file);
                let _ = fs::remove_file(&path);
                return Err(error);
            }
            let identity = crate::hashline::SnapshotId::from_metadata(&file.metadata()?);
            return Ok(Self {
                path,
                record,
                identity,
            });
        }
        Err(CoreError::Session(
            "SESSION_LOCKED: 无法回收会话锁，需要显式 force 解锁".into(),
        ))
    }

    pub fn info(&self) -> SessionLockInfo {
        self.record.clone().into()
    }
}

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        let identity = fs::metadata(&self.path)
            .ok()
            .map(|metadata| crate::hashline::SnapshotId::from_metadata(&metadata));
        if identity == Some(self.identity)
            && read_record(&self.path).ok().flatten().as_ref() == Some(&self.record)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// 回收陈旧锁；调用前已确认当前锁记录属于已退出的进程。
///
/// 回收分两步：先用 `create_new` 建立固定名字的认领文件（同一时刻只有一个进程
/// 能成功），取得认领权后**重新读取**锁记录，确认它仍然陈旧才删除。只根据先前
/// 快照无条件 unlink 会让两个同时读到陈旧记录的进程依次删除对方刚建立的锁，
/// 随后同时持有同一会话锁；认领把「复查 + 删除」串行化，落败者只会重新读取到
/// 存活锁并报 `SESSION_LOCKED`。
///
/// 认领文件内容不可读（回收途中崩溃在写入进程号之前）时保守地按「认领仍有效」
/// 处理，避免误删他人正在使用的认领；此时取得锁会失败并要求显式 force 解锁。
fn reclaim(path: &Path) -> Result<(), CoreError> {
    let claim = claim_path(path);
    for _ in 0..RECLAIM_RETRIES {
        match OpenOptions::new().write(true).create_new(true).open(&claim) {
            Ok(mut file) => {
                // 认领文件记录持有者进程：回收途中崩溃时由其它进程按进程存活判定清理。
                let _ = file.write_all(std::process::id().to_string().as_bytes());
                let _ = file.sync_all();
                let result = reclaim_claimed(path);
                drop(file);
                let _ = fs::remove_file(&claim);
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // 认领者已死在回收途中时清理它的认领文件，其余情况让出本轮重试。
                if read_claim_pid(&claim).is_some_and(|pid| !pid_is_alive(pid)) {
                    let _ = fs::remove_file(&claim);
                }
                std::hint::spin_loop();
            }
            Err(error) => return Err(CoreError::Io(error)),
        }
    }
    // 认领仍被占用：交回外层重试，由它读取当前锁并报告持有者。
    Ok(())
}

/// 持有认领权时的实际回收：复查锁记录仍然陈旧后才删除。
fn reclaim_claimed(path: &Path) -> Result<(), CoreError> {
    match read_record(path)? {
        Some(lock) if pid_is_alive(lock.pid) => Ok(()),
        Some(_) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CoreError::Io(error)),
        },
        // 文件已经消失：另一进程完成了回收。
        None => Ok(()),
    }
}

fn claim_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}{RECLAIM_SUFFIX}"))
}

fn read_claim_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// 当前锁元信息；损坏或缺失的文件不作为有效锁投影。
pub fn read(path: impl AsRef<Path>) -> Option<SessionLockInfo> {
    read_record(path.as_ref()).ok().flatten().map(Into::into)
}

/// 释放锁；非 force 模式只允许锁的拥有进程执行。
pub fn unlock(path: impl AsRef<Path>, force: bool) -> Result<bool, CoreError> {
    let path = path.as_ref();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(CoreError::Io(error)),
    };
    if force {
        // 回收途中崩溃会留下认领文件；force 解锁同时清理它，否则后续会话无法取得锁。
        let _ = fs::remove_file(claim_path(path));
        return match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(CoreError::Io(error)),
        };
    }
    let record: LockRecord = serde_json::from_slice(&bytes)
        .map_err(|_| CoreError::Session("SESSION_LOCKED: 锁记录损坏，需要 force 解锁".into()))?;
    if record.pid != std::process::id() {
        return Err(CoreError::Session(format!(
            "SESSION_LOCKED: 锁由进程 {} 持有",
            record.pid
        )));
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CoreError::Io(error)),
    }
}

fn read_record(path: &Path) -> Result<Option<LockRecord>, CoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CoreError::Io(error)),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn process_name() -> Option<String> {
    fs::read_to_string("/proc/self/comm")
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

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `pid` is positive and range-checked; signal 0 checks process existence without sending a signal.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
    };

    if pid == 0 {
        return false;
    }
    // SAFETY: 只请求等待权限，PID 来自锁文件并作为 Windows 进程标识传入。
    let raw_process = unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid) };
    if raw_process.is_null() {
        return std::io::Error::last_os_error().raw_os_error()
            != Some(ERROR_INVALID_PARAMETER as i32);
    }
    // SAFETY: OpenProcess 返回的非空句柄由当前函数独占，交给 OwnedHandle 接管。
    let process = unsafe { OwnedHandle::from_raw_handle(raw_process) };
    // 只有进程句柄变为 signaled 才能确认退出；等待失败时保守地保持锁有效。
    unsafe { WaitForSingleObject(process.as_raw_handle(), 0) != 0 }
}

#[cfg(all(not(unix), not(windows)))]
fn pid_is_alive(pid: u32) -> bool {
    pid == std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_only_its_owner_can_unlock() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let guard = SessionLockGuard::acquire(&path, "tui").expect("取得锁");
        assert_eq!(read(&path).expect("锁投影").pid, std::process::id());
        assert!(SessionLockGuard::acquire(&path, "webui").is_err());
        assert!(unlock(&path, false).expect("拥有者释放"));
        assert!(read(&path).is_none());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn stale_lock_reclaim_never_removes_a_lock_created_meanwhile() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let stale = LockRecord {
            pid: std::process::id().saturating_add(1_000_000),
            frontend: "crashed".into(),
            process_name: None,
            acquired_at_ms: 1,
        };
        fs::write(&path, serde_json::to_vec(&stale).expect("锁记录 JSON")).expect("写陈旧锁");
        // 进程 A 读到陈旧记录；随后进程 B 完成回收并建立自己的锁。
        let observed = read_record(&path).expect("读取").expect("陈旧记录");
        assert!(!pid_is_alive(observed.pid));
        let guard = SessionLockGuard::acquire(&path, "webui").expect("进程 B 取得锁");
        assert_eq!(read(&path).expect("锁投影").pid, std::process::id());
        // 进程 A 依据旧快照发起回收：必须识别出 B 的存活锁并放弃。
        reclaim(&path).expect("回收请求完成");
        assert!(
            read(&path).is_some_and(|lock| lock.pid == std::process::id()),
            "落败的回收不得删除持有者的锁文件"
        );
        let error = SessionLockGuard::acquire(&path, "tui")
            .err()
            .expect("B 持锁时 A 不得取得锁");
        assert!(error.to_string().contains("SESSION_LOCKED"), "{error}");
        assert!(read(&path).is_some(), "失败的取得不得删除持有者的锁文件");
        drop(guard);
        assert!(!path.exists());
        assert!(!claim_path(&path).exists());
    }

    #[test]
    fn force_unlock_removes_a_claim_left_by_a_crashed_reclaim() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let stale = LockRecord {
            pid: std::process::id().saturating_add(1_000_000),
            frontend: "crashed".into(),
            process_name: None,
            acquired_at_ms: 1,
        };
        fs::write(&path, serde_json::to_vec(&stale).expect("锁记录 JSON")).expect("写陈旧锁");
        fs::write(claim_path(&path), stale.pid.to_string()).expect("写认领文件");
        assert!(unlock(&path, true).expect("强制释放"));
        assert!(!path.exists());
        assert!(
            !claim_path(&path).exists(),
            "force 解锁必须清理残留认领文件"
        );
    }

    #[test]
    fn reclaim_leaves_a_stale_lock_that_cannot_be_claimed_untouched() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let stale = LockRecord {
            pid: std::process::id().saturating_add(1_000_000),
            frontend: "crashed".into(),
            process_name: None,
            acquired_at_ms: 1,
        };
        fs::write(&path, serde_json::to_vec(&stale).expect("锁记录 JSON")).expect("写陈旧锁");
        // 认领文件内容不可读时保守失败：宁可要求 force 解锁，也不能误删他人认领。
        fs::write(claim_path(&path), b"").expect("写空认领文件");
        reclaim(&path).expect("回收请求完成");
        assert!(read(&path).is_some(), "无法认领时不得删除陈旧锁");
        fs::remove_file(claim_path(&path)).expect("清理认领文件");
        reclaim(&path).expect("回收请求完成");
        assert!(read(&path).is_none(), "认领成功后必须删除陈旧锁");
    }

    #[test]
    fn force_unlock_removes_foreign_and_stale_records() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("ses_123.lock");
        let foreign = LockRecord {
            pid: std::process::id().saturating_add(1_000_000),
            frontend: "desktop".into(),
            process_name: Some("dead".into()),
            acquired_at_ms: 1,
        };
        fs::write(&path, serde_json::to_vec(&foreign).expect("锁记录 JSON")).expect("写陈旧锁");
        assert!(unlock(&path, false).is_err());
        assert!(unlock(&path, true).expect("强制释放"));
        fs::write(&path, b"not json").expect("写损坏锁");
        assert!(unlock(&path, false).is_err());
        assert!(unlock(&path, true).expect("强制删除损坏锁"));
        assert!(!path.exists());
        assert!(read(&path).is_none());
    }
}
