//! 输入历史的持久化：全局一份 JSONL，启动读尾部，提交时追加。
//!
//! 存储不按项目分文件——所有记录写进同一个文件，每条带提交时的 cwd；
//! [`HistoryScope`] 只控制展示时是否按项目过滤，切换范围不用重读磁盘。

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

/// 输入历史的内存与磁盘视图；记录时同步更新两者。
#[derive(Default)]
pub struct InputHistory {
    path: Option<PathBuf>,
    /// 最新在前。
    records: Vec<Record>,
}

impl InputHistory {
    /// 读取 `home` 下的历史文件；文件缺失时返回空历史，坏行跳过。
    pub fn load(home: &Path) -> Self {
        let path = home.join(FILE_NAME);
        let mut records = match read_tail(&path) {
            Ok(records) => records,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %error, "读取输入历史失败");
                }
                Vec::new()
            }
        };
        records.reverse();
        Self {
            path: Some(path),
            records,
        }
    }

    /// 记一条提交；写盘失败只记日志，不影响发送。
    pub fn record(&mut self, text: &str, cwd: &str) {
        if text.trim().is_empty() {
            return;
        }
        let record = Record {
            text: text.to_owned(),
            cwd: Some(cwd.to_owned()),
        };
        match serde_json::to_vec(&record) {
            Ok(mut line) => {
                line.push(b'\n');
                if let Some(path) = &self.path
                    && let Err(error) = append(path, &line)
                {
                    tracing::warn!(path = %path.display(), %error, "写入输入历史失败");
                }
            }
            Err(error) => tracing::warn!(%error, "序列化输入历史失败"),
        }
        self.records.insert(0, record);
    }

    /// 当前展示范围下的历史文本：最新在前，按正文去重，最多 [`HISTORY_MAX`] 条。
    pub fn visible(&self, scope: HistoryScope, project: &str) -> Vec<String> {
        let mut seen = HashSet::with_capacity(HISTORY_MAX);
        let mut visible = Vec::with_capacity(HISTORY_MAX);
        for record in &self.records {
            if scope == HistoryScope::Project && record.cwd.as_deref() != Some(project) {
                continue;
            }
            if !seen.insert(record.text.as_str()) {
                continue;
            }
            visible.push(record.text.clone());
            if visible.len() == HISTORY_MAX {
                break;
            }
        }
        visible
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
