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
    pub fn compaction(&self) -> PathBuf {
        self.sidecar.join("compaction")
    }
}

/// 单会话 JSONL 文件句柄；支持磁盘尚无文件的延迟初始化。
///
/// 新会话创建时只登记内存，首次 append/rewrite 才把 header（及后续 entry）
/// 真正写入磁盘。这样从未产生任何 entry 的空会话不会留下 JSONL 文件，
/// 也就不会出现在 `session/list` 中。
pub struct SessionFile {
    path: PathBuf,
    file: tokio::sync::Mutex<Option<File>>,
    revision: RwLock<crate::hashline::SnapshotId>,
}

impl SessionFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 在目标路径创建新的 transcript 文件并写入 header 作为第一行。
    ///
    /// 供延迟落盘使用：目录按需创建，`create_new` 保证并发下只有一个
    /// 写入者胜出，输者因文件已存在而报错（由上层互斥锁避免）。
    pub async fn create_with_header(path: &Path, header: &Entry) -> Result<Self, CoreError> {
        let parent = path
            .parent()
            .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
        fs::create_dir_all(parent).await?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(path)
            .await?;
        let mut revision = crate::hashline::SnapshotId {
            device: 0,
            inode: 0,
            modified_secs: 0,
            modified_nanos: 0,
            changed_secs: 0,
            changed_nanos: 0,
            size: 0,
            content_hash: 0,
        };
        let mut line = serde_json::to_vec(header).map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        line.push(b'\n');
        let write_result = async {
            file.write_all(&line).await?;
            file.flush().await?;
            revision = crate::hashline::SnapshotId::from_metadata(&file.metadata().await?);
            Ok::<(), CoreError>(())
        }
        .await;
        if let Err(error) = write_result {
            drop(file);
            let _ = fs::remove_file(path).await;
            return Err(error);
        }
        Ok(Self {
            path: path.to_owned(),
            file: tokio::sync::Mutex::new(Some(file)),
            revision: RwLock::new(revision),
        })
    }

    pub async fn changed_on_disk(&self) -> Result<bool, CoreError> {
        let metadata = match fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        Ok(*self.revision.read() != crate::hashline::SnapshotId::from_metadata(&metadata))
    }

    /// 重新打开磁盘上的 transcript。
    ///
    /// 外部维护可能已把日志归档成 `.jsonl.gz`：先解压恢复可写日志再打开，
    /// 否则重新打开的会话无法再追加记录。调用者持会话文件锁。
    pub async fn reopen(&self) -> Result<(), CoreError> {
        super::maintenance::restore(&self.path).await?;
        let mut current = self.file.lock().await;
        let reopened = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&reopened.metadata().await?);
        *current = Some(reopened);
        Ok(())
    }

    /// 追加一条完整 JSONL 记录并 flush；磁盘文件缺失时以 header 起步重建。
    pub async fn append(&self, entry: &Entry, header: Option<&Entry>) -> Result<(), CoreError> {
        let mut line = serde_json::to_vec(entry).map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        if file.is_none() {
            let parent = self
                .path
                .parent()
                .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
            fs::create_dir_all(parent).await?;
            let created = OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(&self.path)
                .await?;
            *file = Some(created);
            if let Some(header) = header {
                let mut header_line = serde_json::to_vec(header).map_err(|error| {
                    CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
                })?;
                header_line.push(b'\n');
                file.as_mut()
                    .expect("刚创建的句柄")
                    .write_all(&header_line)
                    .await?;
            }
        }
        let handle = file.as_mut().expect("延迟初始化后句柄必然存在");
        handle.write_all(&line).await?;
        handle.flush().await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&handle.metadata().await?);
        Ok(())
    }

    /// 原子替换完整 transcript；rename 前刷写并 fsync 临时文件。
    /// 磁盘文件不存在时同样适用：整份 entries 经临时文件 rename 发布。
    pub async fn rewrite(&self, entries: &[Entry]) -> Result<(), CoreError> {
        let mut current = self.file.lock().await;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
        fs::create_dir_all(parent).await?;
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
            *current = Some(reopened);
            *self.revision.write() = crate::hashline::SnapshotId::from_metadata(
                &current
                    .as_ref()
                    .expect("rename 后句柄必然存在")
                    .metadata()
                    .await?,
            );
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

    /// 只计算 transcript 与 sidecar 的路径，不触碰磁盘。
    pub fn paths_for(&self, cwd: &Path, session_id: &str, created_at_ms: u64) -> SessionPaths {
        let directory = self.root.join(encode_cwd(cwd));
        SessionPaths {
            transcript: directory.join(format!(
                "{}_{}.jsonl",
                timestamp_name(created_at_ms),
                session_id
            )),
            sidecar: directory.join(session_id),
        }
    }

    /// 登记一个尚未落盘的会话句柄；首次 append 时才创建 transcript 文件。
    pub fn open_deferred(&self, cwd: &Path, session_id: &str, created_at_ms: u64) -> SessionFile {
        let paths = self.paths_for(cwd, session_id, created_at_ms);
        self.index
            .write()
            .insert(session_id.to_owned(), paths.clone());
        SessionFile {
            path: paths.transcript.clone(),
            file: tokio::sync::Mutex::new(None),
            // 零值纪元仅表示「磁盘上还没有文件」；changed_on_disk 在
            // NotFound 时同样返回 true，两条路径语义一致。
            revision: RwLock::new(crate::hashline::SnapshotId {
                device: 0,
                inode: 0,
                modified_secs: 0,
                modified_nanos: 0,
                changed_secs: 0,
                changed_nanos: 0,
                size: 0,
                content_hash: 0,
            }),
        }
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
            file: tokio::sync::Mutex::new(Some(file)),
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
    /// 只有 header、从未产生任何 entry 的空会话不出现在列表中。
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
                let Some((mut info, has_entries)) = read_session_info(&path).await? else {
                    continue;
                };
                if !has_entries {
                    continue;
                }
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

/// 读取 header 与轻量元信息；`has_entries` 为 `false` 表示文件只有 header，
/// 该会话从未产生任何对话内容（延迟落盘前被抛弃的空会话）。
async fn read_session_info(
    path: &Path,
) -> Result<Option<(moc_protocol::methods::SessionInfo, bool)>, CoreError> {
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
    let mut has_entries = false;
    // 忽略大消息字段；无需构造正文、工具参数或思维链的对象树。
    #[derive(serde::Deserialize)]
    struct Metadata {
        #[serde(rename = "type")]
        kind: String,
        at_ms: u64,
        title: Option<String>,
        model: Option<String>,
        mode: Option<moc_protocol::methods::SessionMode>,
        #[serde(default)]
        permission: Option<moc_protocol::methods::SessionPermission>,
    }
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        has_entries = true;
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
            "permission_change" => {
                if let Some(permission) = metadata.permission {
                    info.permission = permission;
                }
            }
            _ => {}
        }
    }
    Ok(Some((info, has_entries)))
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
    use moc_protocol::methods::{SessionMode, SessionPermission};

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
    async fn jsonl_store_appends_metadata_lists_and_rewrites_atomically() {
        use super::super::entries::EntryPayload;
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SessionStore::new(directory.path());
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/old".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            None,
            "旧标题".into(),
            1_728_000_000_123,
        );
        let paths = store.paths_for(
            Path::new("/tmp/project"),
            "ses_123456789abc",
            1_728_000_000_123,
        );
        let file = SessionFile::create_with_header(&paths.transcript, &header)
            .await
            .expect("创建会话文件");
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: moc_llm::Message::user_text("第一条"),
                tool: None,
                finish_reason: None,
            },
            header.id().into(),
            1_728_000_000_124,
        );
        file.append(&message, None).await.expect("追加消息");
        let mut parent = message.id().to_owned();
        for (offset, payload) in [
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
            let entry = Entry::from_payload(payload, parent, 1_728_000_000_125 + offset as u64);
            parent = entry.id().to_owned();
            file.append(&entry, None).await.expect("追加元数据");
        }

        let listed = store.list().await.expect("列出会话");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "新标题");
        assert_eq!(listed[0].model, "p/new");
        assert_eq!(listed[0].mode, SessionMode::Plan);
        assert_eq!(listed[0].updated_at_ms, 1_728_000_000_127);

        let entries = store.read_entries(&paths).await.expect("读取 JSONL");
        assert_eq!(entries.len(), 5);
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
}
