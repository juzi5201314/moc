//! JSONL transcript 存储与原子历史重写。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::error::CoreError;

use super::entries::Entry;

/// 一个会话的 transcript 与 sidecar 路径。
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub transcript: PathBuf,
    pub sidecar: PathBuf,
}

impl SessionPaths {
    pub fn lock_file(&self, session_id: &str) -> PathBuf {
        self.transcript
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{session_id}.lock"))
    }

    pub fn plans(&self) -> PathBuf {
        self.sidecar.join("plans")
    }

    pub fn jobs(&self) -> PathBuf {
        self.sidecar.join("jobs")
    }

    pub fn artifacts(&self) -> PathBuf {
        self.sidecar.join("artifacts")
    }
}

/// 单会话 JSONL 文件句柄，写入与重写互斥。
pub struct SessionFile {
    path: PathBuf,
    file: tokio::sync::Mutex<File>,
    revision: RwLock<crate::hashline::SnapshotId>,
}

impl SessionFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn changed_on_disk(&self) -> Result<bool, CoreError> {
        let metadata = fs::metadata(&self.path).await?;
        Ok(*self.revision.read() != crate::hashline::SnapshotId::from_metadata(&metadata))
    }

    pub async fn reopen(&self) -> Result<(), CoreError> {
        let mut current = self.file.lock().await;
        let reopened = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&reopened.metadata().await?);
        *current = reopened;
        Ok(())
    }

    /// 追加一条完整 JSONL 记录并 flush。
    pub async fn append(&self, entry: &Entry) -> Result<(), CoreError> {
        let mut line = serde_json::to_vec(entry).map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line).await?;
        file.flush().await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&file.metadata().await?);
        Ok(())
    }

    /// 原子替换完整 transcript；rename 前刷写并 fsync 临时文件。
    pub async fn rewrite(&self, entries: &[Entry]) -> Result<(), CoreError> {
        let mut current = self.file.lock().await;
        let temp = self
            .path
            .with_extension(format!("jsonl.{}.tmp", crate::ids::entry_id()));
        let result = async {
            let mut replacement = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await?;
            for entry in entries {
                let line = serde_json::to_vec(entry).map_err(|error| {
                    CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
                })?;
                replacement.write_all(&line).await?;
                replacement.write_all(b"\n").await?;
            }
            replacement.flush().await?;
            replacement.sync_all().await?;
            let reopened = OpenOptions::new()
                .read(true)
                .append(true)
                .open(&temp)
                .await?;
            drop(replacement);
            fs::rename(&temp, &self.path).await?;
            *current = reopened;
            *self.revision.write() =
                crate::hashline::SnapshotId::from_metadata(&current.metadata().await?);
            Ok::<(), CoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp).await;
        }
        result
    }
}

/// 会话文件发现、创建、读取与删除。
pub struct SessionStore {
    root: PathBuf,
    index: RwLock<HashMap<String, SessionPaths>>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            index: RwLock::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 创建 sidecar 与 transcript，并把 header 写为第一行。
    pub async fn create(
        &self,
        cwd: &Path,
        session_id: &str,
        created_at_ms: u64,
        header: &Entry,
    ) -> Result<(SessionPaths, SessionFile), CoreError> {
        if header.header_fields().is_none() {
            return Err(CoreError::Session(
                "创建 transcript 时传入的第一条记录不是 header".into(),
            ));
        }
        let directory = self.root.join(encode_cwd(cwd));
        fs::create_dir_all(&directory).await?;
        let paths = SessionPaths {
            transcript: directory.join(format!(
                "{}_{}.jsonl",
                timestamp_name(created_at_ms),
                session_id
            )),
            sidecar: directory.join(session_id),
        };
        fs::create_dir_all(paths.plans()).await?;
        fs::create_dir_all(paths.jobs()).await?;
        fs::create_dir_all(paths.artifacts()).await?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&paths.transcript)
            .await?;
        let revision = RwLock::new(crate::hashline::SnapshotId::from_metadata(
            &file.metadata().await?,
        ));
        let session_file = SessionFile {
            path: paths.transcript.clone(),
            file: tokio::sync::Mutex::new(file),
            revision,
        };
        if let Err(error) = session_file.append(header).await {
            drop(session_file);
            let _ = fs::remove_file(&paths.transcript).await;
            let _ = fs::remove_dir_all(&paths.sidecar).await;
            return Err(error);
        }
        self.index
            .write()
            .insert(session_id.to_owned(), paths.clone());
        Ok((paths, session_file))
    }

    /// 通过会话 ID 查找记录路径，必要时扫描磁盘目录。
    pub async fn find(&self, session_id: &str) -> Result<Option<SessionPaths>, CoreError> {
        if let Some(paths) = self.index.read().get(session_id).cloned() {
            return Ok(Some(paths));
        }
        let mut directories = match fs::read_dir(&self.root).await {
            Ok(directories) => directories,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        while let Some(directory) = directories.next_entry().await? {
            if !directory.file_type().await?.is_dir() {
                continue;
            }
            let mut files = fs::read_dir(directory.path()).await?;
            while let Some(file) = files.next_entry().await? {
                let Some(path) = super::maintenance::journal_path(&file.path()) else {
                    continue;
                };
                if path
                    .file_stem()
                    .is_some_and(|stem| stem.to_string_lossy().ends_with(&format!("_{session_id}")))
                {
                    let paths = SessionPaths {
                        transcript: path,
                        sidecar: file.path().parent().unwrap_or(&self.root).join(session_id),
                    };
                    self.index
                        .write()
                        .insert(session_id.to_owned(), paths.clone());
                    return Ok(Some(paths));
                }
            }
        }
        Ok(None)
    }

    /// 打开 transcript 用于追加。
    pub async fn open(&self, paths: &SessionPaths) -> Result<SessionFile, CoreError> {
        super::maintenance::restore(&paths.transcript).await?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&paths.transcript)
            .await?;
        let revision = RwLock::new(crate::hashline::SnapshotId::from_metadata(
            &file.metadata().await?,
        ));
        Ok(SessionFile {
            path: paths.transcript.clone(),
            file: tokio::sync::Mutex::new(file),
            revision,
        })
    }

    /// 读取并反序列化完整 JSONL 历史。
    pub async fn read_entries(&self, paths: &SessionPaths) -> Result<Vec<Entry>, CoreError> {
        let mut lines = super::maintenance::reader(&paths.transcript).await?.lines();
        let mut entries = Vec::new();
        let mut line_number = 0usize;
        while let Some(line) = lines.next_line().await? {
            line_number += 1;
            let entry = serde_json::from_str(&line).map_err(|error| {
                CoreError::Session(format!(
                    "解析 {} 第 {line_number} 行失败: {error}",
                    paths.transcript.display()
                ))
            })?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// 流式投影元信息，跳过消息载荷分配，并按更新时间降序列会话。
    pub async fn list(&self) -> Result<Vec<moc_protocol::methods::SessionInfo>, CoreError> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut directories = match fs::read_dir(&self.root).await {
            Ok(directories) => directories,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(error) => return Err(error.into()),
        };
        while let Some(directory) = directories.next_entry().await? {
            if !directory.file_type().await?.is_dir() {
                continue;
            }
            let mut files = fs::read_dir(directory.path()).await?;
            while let Some(file) = files.next_entry().await? {
                let Some(path) = super::maintenance::journal_path(&file.path()) else {
                    continue;
                };
                if !seen.insert(path.clone()) {
                    continue;
                }
                let Some(mut info) = read_session_info(&path).await? else {
                    continue;
                };
                info.lock = super::lock::read(
                    path.parent()
                        .unwrap_or(&self.root)
                        .join(format!("{}.lock", info.id)),
                );
                self.index.write().insert(
                    info.id.clone(),
                    SessionPaths {
                        transcript: path.clone(),
                        sidecar: path.parent().unwrap_or(&self.root).join(&info.id),
                    },
                );
                result.push(info);
            }
        }
        result.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(result)
    }

    /// 删除 transcript 与 sidecar；锁文件由调用方先按语义释放。
    pub async fn delete(&self, session_id: &str, paths: &SessionPaths) -> Result<(), CoreError> {
        match fs::remove_file(&paths.transcript).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::remove_dir_all(&paths.sidecar).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.index.write().remove(session_id);
        Ok(())
    }
}

async fn read_session_info(
    path: &Path,
) -> Result<Option<moc_protocol::methods::SessionInfo>, CoreError> {
    let mut reader = super::maintenance::reader(path).await?;
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let header: Entry = serde_json::from_str(&line)
        .map_err(|error| CoreError::Session(format!("{}：{error}", path.display())))?;
    let Some(mut info) = header.session_info(None) else {
        return Ok(None);
    };
    // 忽略大消息字段；无需构造正文、工具参数或思维链的对象树。
    #[derive(serde::Deserialize)]
    struct Metadata {
        #[serde(rename = "type")]
        kind: String,
        at_ms: u64,
        title: Option<String>,
        model: Option<String>,
        mode: Option<moc_protocol::methods::SessionMode>,
    }
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let metadata: Metadata = serde_json::from_str(&line)
            .map_err(|error| CoreError::Session(format!("{}：{error}", path.display())))?;
        info.updated_at_ms = metadata.at_ms;
        match metadata.kind.as_str() {
            "title_change" => {
                if let Some(title) = metadata.title {
                    info.title = title;
                }
            }
            "model_change" => {
                if let Some(model) = metadata.model {
                    info.model = model;
                }
            }
            "mode_change" => {
                if let Some(mode) = metadata.mode {
                    info.mode = mode;
                }
            }
            _ => {}
        }
    }
    Ok(Some(info))
}

/// 把 cwd 编码成 omp 风格的可读目录名。
pub fn encode_cwd(cwd: &Path) -> String {
    if let Some(home) = crate::paths::home_dir()
        && let Ok(relative) = cwd.strip_prefix(home)
    {
        return format!("-{}", encode_path(relative));
    }
    #[cfg(unix)]
    if let Ok(relative) = cwd.strip_prefix("/tmp") {
        return format!("-tmp-{}", encode_path(relative));
    }
    format!("--{}--", encode_path(cwd))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for character in path.to_string_lossy().chars() {
        match character {
            '/' | '\\' => encoded.push('-'),
            ':' => encoded.push_str("%3A"),
            '%' => encoded.push_str("%25"),
            '-' => encoded.push_str("%2D"),
            character => encoded.push(character),
        }
    }
    encoded
}

#[cfg(not(windows))]
fn encode_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

fn timestamp_name(at_ms: u64) -> String {
    let seconds = at_ms / 1000;
    let millis = at_ms % 1000;
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}-{:02}-{:02}-{millis:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day / 60) % 60,
        seconds_of_day % 60
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use moc_protocol::methods::SessionMode;

    use super::*;

    #[test]
    fn cwd_and_timestamp_paths_are_stable() {
        assert_eq!(
            encode_cwd(Path::new("/tmp/project/moc")),
            "-tmp-project-moc"
        );
        assert_eq!(timestamp_name(0), "1970-01-01T00-00-00-000Z");
        assert_eq!(
            timestamp_name(1_728_000_000_123),
            "2024-10-04T00-00-00-123Z"
        );
    }

    #[tokio::test]
    async fn jsonl_store_appends_lists_loads_and_rewrites_atomically() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SessionStore::new(directory.path());
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionMode::Normal,
            None,
            "会话".into(),
            1_728_000_000_123,
        );
        let (paths, file) = store
            .create(
                Path::new("/tmp/project"),
                "ses_123456789abc",
                1_728_000_000_123,
                &header,
            )
            .await
            .expect("创建会话文件");
        let message = Entry::from_payload(
            super::super::entries::EntryPayload::Message {
                message: moc_llm::Message::user_text("第一条"),
                tool: None,
                finish_reason: None,
            },
            header.id().into(),
            1_728_000_000_124,
        );
        file.append(&message).await.expect("追加消息");
        let listed = store.list().await.expect("列出会话");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].updated_at_ms, 1_728_000_000_124);
        let entries = store.read_entries(&paths).await.expect("读取 JSONL");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].parent_id(), Some(entries[0].id()));
        file.rewrite(&entries[..1]).await.expect("原子重写");
        assert_eq!(
            store.read_entries(&paths).await.expect("重读 JSONL").len(),
            1
        );
        assert!(
            paths
                .transcript
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("2024-10-04T00-00-00-123Z_")
        );
    }

    #[tokio::test]
    async fn fresh_store_lists_latest_title_model_and_mode() {
        use super::super::entries::EntryPayload;
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path());
        let header = Entry::header(
            "ses_metadata".into(),
            "/tmp/project".into(),
            "p/old".into(),
            SessionMode::Normal,
            None,
            "旧标题".into(),
            100,
        );
        let (_, file) = store
            .create(Path::new("/tmp/project"), "ses_metadata", 100, &header)
            .await
            .unwrap();
        let mut parent = header.id().to_owned();
        for (index, payload) in [
            EntryPayload::TitleChange {
                title: "新标题".into(),
            },
            EntryPayload::ModelChange {
                model: "p/new".into(),
            },
            EntryPayload::ModeChange {
                mode: SessionMode::Plan,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let entry = Entry::from_payload(payload, parent, 101 + index as u64);
            parent = entry.id().to_owned();
            file.append(&entry).await.unwrap();
        }
        let listed = SessionStore::new(directory.path()).list().await.unwrap();
        assert_eq!(listed[0].title, "新标题");
        assert_eq!(listed[0].model, "p/new");
        assert_eq!(listed[0].mode, SessionMode::Plan);
        assert_eq!(listed[0].updated_at_ms, 103);
    }
}
