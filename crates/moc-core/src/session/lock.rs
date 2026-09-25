//! 基于 create_new 的跨进程会话锁。

use std::fs::{self, OpenOptions};
use std::io::Write;
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
                    if current.is_none() {
                        return Err(CoreError::Session(
                            "SESSION_LOCKED: 锁正在写入或已经损坏，需要显式 force 解锁".into(),
                        ));
                    }
                    if current.as_ref().is_some_and(|lock| pid_is_alive(lock.pid)) {
                        let lock = current.expect("存活锁记录已确认");
                        return Err(CoreError::Session(format!(
                            "SESSION_LOCKED: pid {} ({}) 正在使用会话",
                            lock.pid, lock.frontend
                        )));
                    }
                    match fs::remove_file(&path) {
                        Ok(()) => continue,
                        Err(remove_error)
                            if remove_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(remove_error) => {
                            return Err(CoreError::Io(remove_error));
                        }
                    }
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
        Err(CoreError::Session("SESSION_LOCKED: 无法回收会话锁".into()))
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
