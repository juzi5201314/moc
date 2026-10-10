use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobLogMode {
    Monitored,
    Persistent,
}

#[derive(Debug)]
struct JobLogState {
    file: File,
    bytes_written: u64,
    ring: VecDeque<u8>,
    truncated: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct JobLog {
    path: PathBuf,
    mode: JobLogMode,
    capacity: usize,
    state: Arc<Mutex<JobLogState>>,
}

impl JobLog {
    pub(crate) fn create(path: PathBuf, mode: JobLogMode, capacity: u64) -> std::io::Result<Self> {
        if capacity == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "job log capacity must be greater than zero",
            ));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            path,
            mode,
            capacity: usize::try_from(capacity).unwrap_or(usize::MAX),
            state: Arc::new(Mutex::new(JobLogState {
                file,
                bytes_written: 0,
                ring: VecDeque::new(),
                truncated: false,
            })),
        })
    }

    pub(crate) fn mode(&self) -> JobLogMode {
        self.mode
    }

    pub(crate) fn append(&self, bytes: &[u8]) -> std::io::Result<bool> {
        let mut state = self.state.lock();
        match self.mode {
            JobLogMode::Monitored => {
                let remaining = (self.capacity as u64).saturating_sub(state.bytes_written);
                let kept = remaining.min(bytes.len() as u64) as usize;
                if kept > 0 {
                    state.file.write_all(&bytes[..kept])?;
                    state.bytes_written += kept as u64;
                }
                if kept < bytes.len() {
                    state.truncated = true;
                }
                Ok(state.truncated)
            }
            JobLogMode::Persistent => {
                if bytes.len() >= self.capacity {
                    state.ring.clear();
                    state
                        .ring
                        .extend(bytes[bytes.len() - self.capacity..].iter().copied());
                    state.truncated = true;
                } else {
                    state.ring.extend(bytes.iter().copied());
                    while state.ring.len() > self.capacity {
                        state.ring.pop_front();
                        state.truncated = true;
                    }
                }
                Ok(false)
            }
        }
    }

    pub(crate) fn finish(&self) -> std::io::Result<()> {
        let mut state = self.state.lock();
        if self.mode == JobLogMode::Persistent {
            state.file.set_len(0)?;
            state.file.seek(SeekFrom::Start(0))?;
            let bytes: Vec<u8> = state.ring.iter().copied().collect();
            state.file.write_all(&bytes)?;
            state.bytes_written = bytes.len() as u64;
        }
        state.file.flush()?;
        state.file.sync_all()
    }

    pub(crate) fn read_tail(&self, max_lines: usize) -> std::io::Result<(String, bool)> {
        let state = self.state.lock();
        let bytes = if self.mode == JobLogMode::Persistent {
            state.ring.iter().copied().collect()
        } else {
            let length = state.file.metadata()?.len();
            let start = length.saturating_sub(1024 * 1024);
            let mut file = state.file.try_clone()?;
            file.seek(SeekFrom::Start(start))?;
            let mut bytes = Vec::with_capacity((length - start) as usize);
            file.read_to_end(&mut bytes)?;
            bytes
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let text = lines[lines.len().saturating_sub(max_lines)..].join("\n");
        Ok((text, state.truncated))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn log_truncated(&self) -> bool {
        self.state.lock().truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_log_overwrites_oldest_bytes_without_truncating_process() {
        let directory = tempfile::tempdir().expect("临时目录");
        let log = JobLog::create(directory.path().join("job.log"), JobLogMode::Persistent, 4)
            .expect("创建环形日志");
        assert!(!log.append(b"abc").expect("写入日志"));
        assert!(!log.append(b"def").expect("写入日志"));
        let (text, truncated) = log.read_tail(10).expect("读取日志");
        assert_eq!(text, "cdef");
        assert!(truncated);
        log.finish().expect("压平日志");
        assert_eq!(
            std::fs::read(directory.path().join("job.log")).expect("读取日志文件"),
            b"cdef"
        );
    }
}
