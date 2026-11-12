//! 任务日志：容量内全部落盘；超出后保留开头一半与最近一半，中段丢弃，进程不受影响。

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

/// `read_tail` 最多回看的字节数；只用于取最后几行，不需要整份日志。
const TAIL_WINDOW: u64 = 1024 * 1024;

/// `usize` 在受支持平台上不超过 64 位，转换不会失败；失败时饱和以保持单调。
fn len64(length: usize) -> u64 {
    u64::try_from(length).unwrap_or(u64::MAX)
}

#[derive(Debug)]
struct JobLogState {
    file: File,
    /// 文件当前长度；溢出前等于写入总量，溢出后停在 `capacity`。
    file_len: u64,
    /// 溢出后的最近输出，最多 `tail_cap` 字节；更早的字节被挤出即计入丢弃。
    ring: VecDeque<u8>,
    /// 累计收到的字节数。
    seen: u64,
    finished: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct JobLog {
    path: PathBuf,
    capacity: u64,
    /// 溢出后保留的开头字节数；结尾保留 `capacity - head_cap`。
    head_cap: u64,
    state: Arc<Mutex<JobLogState>>,
    /// 每次追加后唤醒；`job wait` 据此实现「出新输出即返回」。
    output_notify: Arc<Notify>,
}

impl JobLog {
    pub(crate) fn create(path: PathBuf, capacity: u64) -> std::io::Result<Self> {
        if capacity < 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "job log capacity must be at least 2 bytes",
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
            capacity,
            head_cap: capacity / 2,
            state: Arc::new(Mutex::new(JobLogState {
                file,
                file_len: 0,
                ring: VecDeque::new(),
                seen: 0,
                finished: false,
            })),
            output_notify: Arc::new(Notify::new()),
        })
    }

    fn tail_cap(&self) -> usize {
        usize::try_from(self.capacity - self.head_cap).unwrap_or(usize::MAX)
    }

    pub(crate) fn append(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut state = self.state.lock();
        // 收尾后才到的字节（管道读取被放弃时的残留）不再改写已定稿的日志。
        if state.finished {
            return Ok(());
        }
        state.seen = state.seen.saturating_add(len64(bytes.len()));
        let room = self.capacity.saturating_sub(state.file_len);
        let direct = usize::try_from(room).map_or(bytes.len(), |room| room.min(bytes.len()));
        if direct > 0 {
            state.file.write_all(&bytes[..direct])?;
            state.file_len += len64(direct);
        }
        let rest = &bytes[direct..];
        if !rest.is_empty() {
            let cap = self.tail_cap();
            let rest = &rest[rest.len().saturating_sub(cap)..];
            state.ring.extend(rest.iter().copied());
            let excess = state.ring.len().saturating_sub(cap);
            state.ring.drain(..excess);
        }
        self.output_notify.notify_waiters();
        Ok(())
    }

    /// 定稿：溢出过的日志改写成「开头 + 丢弃标记 + 结尾」。可重复调用。
    pub(crate) fn finish(&self) -> std::io::Result<()> {
        let mut state = self.state.lock();
        if state.finished {
            return Ok(());
        }
        state.finished = true;
        let elided = state.seen.saturating_sub(self.capacity);
        if state.seen > self.capacity {
            let tail = self.tail_bytes(&state, self.capacity - self.head_cap)?;
            state.ring = VecDeque::new();
            state.file.set_len(self.head_cap)?;
            state.file.seek(SeekFrom::Start(self.head_cap))?;
            let marker = format!("\n[... {elided} bytes of output elided ...]\n");
            state.file.write_all(marker.as_bytes())?;
            state.file.write_all(&tail)?;
            state.file_len = self.head_cap + len64(marker.len()) + len64(tail.len());
        }
        state.file.flush()?;
        state.file.sync_all()
    }

    /// 逻辑输出的最后 `window` 字节：溢出中为文件尾段接上环形缓冲，否则直接读文件。
    fn tail_bytes(&self, state: &JobLogState, window: u64) -> std::io::Result<Vec<u8>> {
        let from_ring =
            usize::try_from(window).map_or(state.ring.len(), |window| window.min(state.ring.len()));
        let from_file = (window - len64(from_ring)).min(state.file_len);
        let mut bytes = Vec::with_capacity(
            usize::try_from(from_file)
                .unwrap_or(0)
                .saturating_add(from_ring),
        );
        if from_file > 0 {
            // 克隆句柄与原句柄共享文件偏移：读完必须把偏移放回末尾，后续写入才不会覆盖。
            let mut file = state.file.try_clone()?;
            file.seek(SeekFrom::Start(state.file_len - from_file))?;
            (&mut file).take(from_file).read_to_end(&mut bytes)?;
            file.seek(SeekFrom::Start(state.file_len))?;
        }
        let skip = state.ring.len() - from_ring;
        bytes.extend(state.ring.iter().skip(skip).copied());
        Ok(bytes)
    }

    /// 有新输出时完成的 future；等待方要先 `enable` 再检查 `bytes_seen`，避免睡过已到的输出。
    pub(crate) fn output_notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.output_notify.notified()
    }

    /// `read_tail` 同时返回当时的 `seen`，让「展示即已读」的游标推进不丢也不重。
    pub(crate) fn read_tail_and_seen(
        &self,
        max_lines: usize,
    ) -> std::io::Result<(String, bool, u64)> {
        let state = self.state.lock();
        let seen = state.seen;
        let bytes = self.tail_bytes(&state, TAIL_WINDOW)?;
        let elided = seen > self.capacity;
        drop(state);
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        Ok((
            lines[lines.len().saturating_sub(max_lines)..].join("\n"),
            elided,
            seen,
        ))
    }

    /// 自 `offset`（此前返回的 `seen` 游标）之后收到的输出。
    ///
    /// 返回增量文本、新游标（当前 `seen`）与请求范围内因容量被丢弃的字节数。
    /// 溢出日志的中间一段不保留：请求穿过丢弃段时返回两端保留内容并计入丢弃数。
    pub(crate) fn read_since(&self, offset: u64) -> std::io::Result<(String, u64, u64)> {
        let state = self.state.lock();
        let seen = state.seen;
        if offset >= seen {
            return Ok((String::new(), seen, 0));
        }
        // 保留区间：逻辑位置 [0, head_end) 存于文件开头，[tail_start, seen) 存于尾部；
        // 运行中尾部在 ring，定稿溢出后尾部是文件末段（中段标记不算数据，跳过）。
        let (head_end, tail_start) = if seen <= self.capacity {
            (state.file_len, state.file_len)
        } else if state.finished {
            (
                self.head_cap,
                self.capacity.max(seen - len64(self.tail_cap())),
            )
        } else {
            (self.capacity, seen - len64(state.ring.len()))
        };
        let mut bytes = Vec::new();
        if offset < head_end {
            // 克隆句柄共享文件偏移；读完恢复末尾，后续追加才不会被截断。
            let mut file = state.file.try_clone()?;
            file.seek(SeekFrom::Start(offset))?;
            (&mut file)
                .take(head_end - offset)
                .read_to_end(&mut bytes)?;
            file.seek(SeekFrom::Start(state.file_len))?;
        }
        if tail_start < seen {
            let tail = self.tail_bytes(&state, seen - tail_start)?;
            let from = usize::try_from(offset.max(tail_start) - tail_start).unwrap_or(tail.len());
            bytes.extend_from_slice(&tail[from.min(tail.len())..]);
        }
        let skipped = tail_start.saturating_sub(offset.max(head_end));
        Ok((String::from_utf8_lossy(&bytes).into_owned(), seen, skipped))
    }

    /// 最后 `max_lines` 行；第二个值表示中段是否已被丢弃。
    pub(crate) fn read_tail(&self, max_lines: usize) -> std::io::Result<(String, bool)> {
        let (text, elided, _) = self.read_tail_and_seen(max_lines)?;
        Ok((text, elided))
    }

    /// 界面用的实时尾部：最后 `max_lines` 行，`\r` 覆盖的进度行只取最终显示，单行截到 `line_chars` 字符。
    pub(crate) fn live_tail(&self, max_lines: usize, line_chars: usize) -> String {
        // UTF-8 单字符至多 4 字节；窗口刚好装下 `max_lines` 个截断后的整行。
        let window =
            len64(max_lines.saturating_mul(line_chars.saturating_mul(4).saturating_add(1)));
        let bytes = match self.tail_bytes(&self.state.lock(), window) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::debug!(path = %self.path.display(), %error, "读取实时尾部失败");
                return String::new();
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text
            .lines()
            .map(|line| {
                line.rsplit('\r')
                    .find(|part| !part.is_empty())
                    .unwrap_or("")
            })
            .filter(|line| !line.trim().is_empty())
            .collect();
        lines[lines.len().saturating_sub(max_lines)..]
            .iter()
            .map(|line| line.chars().take(line_chars).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 累计收到的字节数；监视器据此判断有没有新输出。
    pub(crate) fn bytes_seen(&self) -> u64 {
        self.state.lock().seen
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn output_elided(&self) -> bool {
        self.state.lock().seen > self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_keeps_head_and_latest_tail_and_marks_the_gap() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("job.log");
        let log = JobLog::create(path.clone(), 8).expect("创建日志");
        log.append(b"abcdef").expect("写入");
        log.append(b"ghij").expect("写入");
        log.append(b"klmnop").expect("写入");
        // 运行中：file 前 capacity 字节 + ring 最新 tail_cap 字节，中段 ijkl 已被挤出。
        let (tail, elided) = log.read_tail(10).expect("读取");
        assert!(elided);
        assert!(tail.ends_with("mnop"), "tail={tail}");
        log.finish().expect("定稿");
        let text = std::fs::read_to_string(&path).expect("读取文件");
        assert_eq!(text, "abcd\n[... 8 bytes of output elided ...]\nmnop");
        // 定稿后的追加被忽略，日志不再变化。
        log.append(b"zzz").expect("写入");
        log.finish().expect("重复定稿");
        assert_eq!(std::fs::read_to_string(&path).expect("读取文件"), text);
    }

    #[test]
    fn output_within_capacity_is_written_verbatim() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("job.log");
        let log = JobLog::create(path.clone(), 64).expect("创建日志");
        log.append(b"one\ntwo\n").expect("写入");
        log.finish().expect("定稿");
        assert!(!log.output_elided());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        assert_eq!(log.read_tail(1).unwrap().0, "two");
    }

    #[test]
    fn read_since_returns_only_new_bytes() {
        let directory = tempfile::tempdir().expect("临时目录");
        let log = JobLog::create(directory.path().join("job.log"), 64).expect("创建日志");
        log.append(b"one\ntwo\n").expect("写入");
        let (text, next, skipped) = log.read_since(0).expect("首次读取");
        assert_eq!(text, "one\ntwo\n");
        assert_eq!(next, 8);
        assert_eq!(skipped, 0);
        log.append(b"three\n").expect("追加");
        let (text, next, skipped) = log.read_since(next).expect("增量读取");
        assert_eq!(text, "three\n");
        assert_eq!(next, 14);
        assert_eq!(skipped, 0);
        let (text, next, skipped) = log.read_since(next).expect("无新输出");
        assert_eq!(text, "");
        assert_eq!(next, 14);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn read_since_reports_skipped_bytes_across_dropped_middle() {
        let directory = tempfile::tempdir().expect("临时目录");
        // capacity 8：文件保留前 8 字节，ring 留最后 4 字节；写 16 字节后逻辑 [8, 12) 被挤出。
        let log = JobLog::create(directory.path().join("job.log"), 8).expect("创建日志");
        log.append(b"aaaabbbbccccdddd").expect("写入");
        // 游标在文件保留区：返回头部剩余 + ring，中段丢弃计入 skipped。
        let (text, _, skipped) = log.read_since(2).expect("读取");
        assert_eq!(text, "aabbbbdddd");
        assert_eq!(skipped, 4);
        // 游标落在丢弃段：只拿尾部，跳过的字节照常上报。
        let (text, _, skipped) = log.read_since(10).expect("读取");
        assert_eq!(text, "dddd");
        assert_eq!(skipped, 2);
        // 游标在尾部内部：只给更靠后的字节，无丢弃。
        let (text, _, skipped) = log.read_since(14).expect("读取");
        assert_eq!(text, "dd");
        assert_eq!(skipped, 0);
    }

    #[test]
    fn read_since_works_after_finish_rewrote_log() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("job.log");
        let log = JobLog::create(path.clone(), 8).expect("创建日志");
        log.append(b"aaaabbbbccccdddd").expect("写入");
        log.finish().expect("定稿");
        // 定稿文件 = aaaa + 标记 + dddd；旧游标照样能增量读。
        let (text, next, skipped) = log.read_since(2).expect("读取");
        assert_eq!(text, "aadddd");
        assert_eq!(next, 16);
        assert_eq!(skipped, 8);
        let (text, _, skipped) = log.read_since(12).expect("读取");
        assert_eq!(text, "dddd");
        assert_eq!(skipped, 0);
    }
}
