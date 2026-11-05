//! 输入历史的持久化：全局一份 JSONL，启动读尾部，提交时追加。
//!
//! 存储不按项目分文件——所有记录写进同一个文件，每条带提交时的 cwd；
//! [`HistoryScope`] 只控制展示时是否按项目过滤，切换范围不用重读磁盘。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// 输入框保留的历史条数。
pub(crate) const HISTORY_MAX: usize = 200;
/// 启动时只读文件尾部这么多字节：历史无限追加，启动开销保持常数。
const TAIL_BYTES: u64 = 1 << 20;
/// 历史文件名，与 tui.toml / tui.log 同目录。
const FILE_NAME: &str = "tui-history.jsonl";

/// 输入历史的展示范围。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope {
    /// 所有项目共享。
    #[default]
    Global,
    /// 只看当前项目目录。
    Project,
}

/// 一条提交记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    text: String,
    /// 提交时的工作目录；旧版本或手工写入的记录可能没有。
    #[serde(default)]
    cwd: Option<String>,
}

/// 提示框与项目索引共享正文；大粘贴持久化后只保留私有文件句柄。
#[derive(Clone)]
pub(crate) struct HistoryText {
    text: Arc<Mutex<TextStorage>>,
    fingerprint: u128,
    len: usize,
}

enum TextStorage {
    Memory(Arc<str>),
    File(std::fs::File),
}

impl From<String> for HistoryText {
    fn from(text: String) -> Self {
        Self {
            fingerprint: xxhash_rust::xxh3::xxh3_128(text.as_bytes()),
            len: text.len(),
            text: Arc::new(Mutex::new(TextStorage::Memory(Arc::from(text)))),
        }
    }
}

impl HistoryText {
    pub(crate) fn read(&self) -> std::io::Result<String> {
        match &mut *self.text.lock().unwrap() {
            TextStorage::Memory(text) => Ok(text.to_string()),
            TextStorage::File(file) => {
                file.rewind()?;
                let mut text = String::with_capacity(self.len);
                file.read_to_string(&mut text)?;
                Ok(text)
            }
        }
    }

    pub(crate) fn matches(&self, text: &str) -> bool {
        if self.len != text.len()
            || self.fingerprint != xxhash_rust::xxh3::xxh3_128(text.as_bytes())
        {
            return false;
        }
        match &mut *self.text.lock().unwrap() {
            TextStorage::Memory(stored) => stored.as_ref() == text,
            TextStorage::File(file) => {
                if file.rewind().is_err() {
                    return false;
                }
                let mut buffer = [0; 8192];
                text.as_bytes().chunks(buffer.len()).all(|chunk| {
                    let read = &mut buffer[..chunk.len()];
                    file.read_exact(read).is_ok() && read == chunk
                })
            }
        }
    }

    fn spill(&self) -> std::io::Result<()> {
        if self.len <= 64 * 1024 {
            return Ok(());
        }
        let text = match &*self.text.lock().unwrap() {
            TextStorage::Memory(text) => Arc::clone(text),
            TextStorage::File(_) => return Ok(()),
        };
        let mut file = tempfile::tempfile()?;
        file.write_all(text.as_bytes())?;
        // 文件写完才发布；写失败时内存正文仍可召回。
        *self.text.lock().unwrap() = TextStorage::File(file);
        Ok(())
    }
}

struct WriteTask {
    text: HistoryText,
    cwd: String,
}

enum WriteCommand {
    Record(WriteTask),
    Flush(std::sync::mpsc::Sender<()>),
}

type Queue = std::sync::mpsc::SyncSender<WriteCommand>;
type Writers = Mutex<HashMap<PathBuf, Vec<std::sync::Weak<Queue>>>>;
static WRITERS: std::sync::LazyLock<Writers> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn flush_pending(path: &Path) {
    let queues: Vec<_> = WRITERS
        .lock()
        .unwrap()
        .get_mut(path)
        .map(|writers| {
            writers.retain(|writer| writer.strong_count() > 0);
            writers
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .collect()
        })
        .unwrap_or_default();
    for queue in queues {
        let (done, wait) = std::sync::mpsc::channel();
        if queue.send(WriteCommand::Flush(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}

struct Writer {
    sender: Option<Arc<Queue>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 每种范围最多保留 200 个可召回条目，索引共享正文；完整日志仍按顺序追加。
#[derive(Default)]
pub struct InputHistory {
    path: Option<PathBuf>,
    global: VecDeque<HistoryText>,
    projects: HashMap<String, VecDeque<HistoryText>>,
    writer: Option<Writer>,
}

impl InputHistory {
    /// 读取 `home` 下的历史文件；文件缺失时返回空历史，坏行跳过。
    pub fn load(home: &Path) -> Self {
        let path = home.join(FILE_NAME);
        flush_pending(&path);
        let records = match read_tail(&path) {
            Ok(records) => records,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %error, "读取输入历史失败");
                }
                Vec::new()
            }
        };
        let mut history = Self {
            path: Some(path),
            ..Self::default()
        };
        for record in records {
            history.remember(&record.text, record.cwd.as_deref());
        }
        history
    }

    fn remember(&mut self, text: &str, cwd: Option<&str>) -> HistoryText {
        let entry = HistoryText::from(text.to_owned());
        fn put(index: &mut VecDeque<HistoryText>, entry: &HistoryText, text: &str) {
            index.retain(|old| old.fingerprint != entry.fingerprint || !old.matches(text));
            index.push_front(entry.clone());
            index.truncate(HISTORY_MAX);
            debug_assert!(index.len() <= HISTORY_MAX);
        }
        put(&mut self.global, &entry, text);
        if let Some(cwd) = cwd {
            put(
                self.projects.entry(cwd.to_owned()).or_default(),
                &entry,
                text,
            );
        }
        entry
    }

    /// UI 只更新句柄索引；串行后台写者负责序列化和落盘，退出时排空。
    pub(crate) fn record_background(&mut self, text: &str, cwd: &str) -> HistoryText {
        let entry = self.remember(text, Some(cwd));
        if self.writer.is_none() {
            let (sender, receiver) = std::sync::mpsc::sync_channel::<WriteCommand>(4);
            let sender = Arc::new(sender);
            if let Some(path) = &self.path {
                let mut writers = WRITERS.lock().unwrap();
                writers.retain(|_, queues| {
                    queues.retain(|queue| queue.strong_count() > 0);
                    !queues.is_empty()
                });
                writers
                    .entry(path.clone())
                    .or_default()
                    .push(Arc::downgrade(&sender));
            }
            let path = self.path.clone();
            let thread = std::thread::spawn(move || {
                for command in receiver {
                    match command {
                        WriteCommand::Record(task) => persist(path.as_deref(), &task),
                        WriteCommand::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            });
            self.writer = Some(Writer {
                sender: Some(sender),
                thread: Some(thread),
            });
        }
        // 队列固定四项：磁盘落后时施加背压，不能无限保留尚未写出的长粘贴。
        self.writer
            .as_ref()
            .unwrap()
            .sender
            .as_ref()
            .unwrap()
            .send(WriteCommand::Record(WriteTask {
                text: entry.clone(),
                cwd: cwd.to_owned(),
            }))
            .unwrap();
        entry
    }

    pub fn record(&mut self, text: &str, cwd: &str) {
        if let Some(path) = &self.path {
            flush_pending(path);
        }
        if text.trim().is_empty() {
            return;
        }
        let entry = self.remember(text, Some(cwd));
        persist(
            self.path.as_deref(),
            &WriteTask {
                text: entry,
                cwd: cwd.to_owned(),
            },
        );
    }

    pub(crate) fn handles(&self, scope: HistoryScope, project: &str) -> Vec<HistoryText> {
        match scope {
            HistoryScope::Global => self.global.iter().cloned().collect(),
            HistoryScope::Project => self
                .projects
                .get(project)
                .map(|index| index.iter().cloned().collect())
                .unwrap_or_default(),
        }
    }

    pub fn visible(&self, scope: HistoryScope, project: &str) -> Vec<String> {
        self.handles(scope, project)
            .iter()
            .filter_map(|text| match text.read() {
                Ok(text) => Some(text),
                Err(error) => {
                    tracing::warn!(%error, "读取输入历史失败");
                    None
                }
            })
            .collect()
    }
}

fn persist(path: Option<&Path>, task: &WriteTask) {
    let result = (|| -> std::io::Result<()> {
        let text = task.text.read()?;
        if let Some(path) = path {
            #[derive(Serialize)]
            struct BorrowedRecord<'a> {
                text: &'a str,
                cwd: &'a str,
            }
            let mut line = serde_json::to_vec(&BorrowedRecord {
                text: &text,
                cwd: &task.cwd,
            })?;
            line.push(b'\n');
            append(path, &line)?;
        }
        task.text.spill()
    })();
    if let Err(error) = result {
        tracing::warn!(%error, "写入输入历史失败");
    }
}

/// 读文件尾部并按行解析；截断点落在行中间时丢掉那半行。
fn read_tail(path: &Path) -> std::io::Result<Vec<Record>> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(TAIL_BYTES);
    let mut skip_first = false;
    if start > 0 {
        // 回退一字节看截断点前是不是换行：是则首行完整，否则是半行。
        file.seek(SeekFrom::Start(start - 1))?;
        let mut previous = [0u8; 1];
        file.read_exact(&mut previous)?;
        skip_first = previous[0] != b'\n';
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if skip_first {
        lines.next();
    }
    let mut records = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(record) if !record.text.trim().is_empty() => records.push(record),
            Ok(_) => {}
            // 崩溃或并发追加留下的半行：跳过这一条，不拖垮其余历史。
            Err(error) => tracing::debug!(%error, "跳过损坏的输入历史记录"),
        }
    }
    Ok(records)
}

/// 追加一行；O_APPEND 保证多窗口并发写不会互相覆盖。
fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // 历史里可能出现粘贴的密钥与私有路径，只给自己读写。
        options.mode(0o600);
    }
    options.open(path)?.write_all(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_reload_and_filter_by_project() {
        let root = tempfile::tempdir().unwrap();
        let mut history = InputHistory::load(root.path());
        history.record("继续", "/project/a");
        history.record("跑一下测试", "/project/a");
        history.record("继续", "/project/b");

        let mut reloaded = InputHistory::load(root.path());
        assert_eq!(
            reloaded.visible(HistoryScope::Global, "/project/a"),
            vec!["继续".to_owned(), "跑一下测试".to_owned()],
            "全局视图按最新位置去重"
        );
        assert_eq!(
            reloaded.visible(HistoryScope::Project, "/project/a"),
            vec!["跑一下测试".to_owned(), "继续".to_owned()]
        );
        assert_eq!(
            reloaded.visible(HistoryScope::Project, "/project/b"),
            vec!["继续".to_owned()]
        );

        // 同一进程里继续写仍然只追加，不覆盖已读内容。
        reloaded.record("新提示", "/project/b");
        let later = InputHistory::load(root.path());
        assert_eq!(
            later.visible(HistoryScope::Project, "/project/b"),
            vec!["新提示".to_owned(), "继续".to_owned()]
        );
        for index in 0..HISTORY_MAX + 1 {
            reloaded.record_background(&format!("条目 {index}"), "/project/b");
        }
        let long = "完整历史正文\n".repeat(8000);
        let entry = reloaded.record_background(&long, "/project/a");
        drop(reloaded); // 退出时排空后台写队列，句柄仍能召回完整正文。
        assert!(matches!(*entry.text.lock().unwrap(), TextStorage::File(_)));
        assert_eq!(entry.read().unwrap(), long);
        let later = InputHistory::load(root.path());
        assert_eq!(later.handles(HistoryScope::Global, "").len(), HISTORY_MAX);
        assert_eq!(
            later.visible(HistoryScope::Project, "/project/a"),
            vec![long, "跑一下测试".into(), "继续".into()]
        );
    }

    #[test]
    fn broken_lines_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(FILE_NAME),
            "{\"text\":\"旧提示\",\"cwd\":\"/p\"}\n{\"text\":\"半\n{\"text\":\"新提示\",\"cwd\":\"/p\"}\n",
        )
        .unwrap();
        let history = InputHistory::load(root.path());
        assert_eq!(
            history.visible(HistoryScope::Global, "/p"),
            vec!["新提示".to_owned(), "旧提示".to_owned()]
        );
    }

    #[test]
    fn tail_read_stays_bounded_when_start_lands_mid_line() {
        let root = tempfile::tempdir().unwrap();
        let filler = "x".repeat(usize::try_from(TAIL_BYTES).unwrap() + 64);
        let mut text = format!("{{\"text\":\"{filler}\",\"cwd\":\"/p\"}}\n");
        text.push_str("{\"text\":\"较早\",\"cwd\":\"/p\"}\n");
        text.push_str("{\"text\":\"最近\",\"cwd\":\"/p\"}\n");
        std::fs::write(root.path().join(FILE_NAME), text).unwrap();

        let history = InputHistory::load(root.path());
        assert_eq!(
            history.visible(HistoryScope::Global, "/p"),
            vec!["最近".to_owned(), "较早".to_owned()],
            "只读尾部，被截断的首行不能污染结果"
        );
    }

    #[cfg(unix)]
    #[test]
    fn history_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let mut history = InputHistory::load(root.path());
        history.record("含密钥的提示", "/p");
        let mode = std::fs::metadata(root.path().join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
