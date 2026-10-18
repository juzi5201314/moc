//! JSONL transcript 的存储和原子历史重写。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::error::CoreError;

use super::entries::Entry;

/// 一个会话的 transcript 和 sidecar 路径。
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

/// 单个会话的 JSONL 文件句柄；磁盘上还没有文件时延迟创建。
///
/// 新会话创建时只登记内存，第一次 append/rewrite 才把 header（以及后续 entry）真正
/// 写入磁盘。这样从未产生 entry 的空会话不会留下 JSONL 文件，也不会出现在
/// `session/list` 里。
pub struct SessionFile {
    path: PathBuf,
    file: tokio::sync::Mutex<Option<File>>,
    revision: RwLock<crate::hashline::SnapshotId>,
}

impl SessionFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 在目标路径创建 transcript 文件，并把 header 写成第一行。
    ///
    /// 供延迟写盘使用：目录按需创建，`create_new` 保证并发时只有一个写入者成功，
    /// 另一个因文件已存在而报错（上层互斥锁会避免这种情况）。
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
    /// 外部维护可能已经把日志归档成 `.jsonl.gz`：先解压恢复可写日志再打开，否则重新
    /// 打开的会话无法再追加记录。调用者要持有会话文件锁。
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

    /// 追加一条完整的 JSONL 记录并 flush；磁盘文件不存在时先用 header 重建。
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
        let handle = file.as_mut().expect("延迟创建之后一定已有句柄");
        handle.write_all(&line).await?;
        handle.flush().await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&handle.metadata().await?);
        Ok(())
    }

    /// 原子替换整个 transcript；rename 前先刷写并 fsync 临时文件。
    /// 磁盘文件不存在时同样适用：整份 entries 通过临时文件 rename 发布。
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
                    .expect("rename 之后一定已有句柄")
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

/// 会话文件的发现、创建、读取和删除。
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

    /// 只计算 transcript 和 sidecar 的路径，不访问磁盘。
    pub fn paths_for(&self, cwd: &Path, session_id: &str, created_at_ms: u64) -> SessionPaths {
        self.paths_in(&self.root.join(encode_cwd(cwd)), session_id, created_at_ms)
    }

    /// 子代理路径：transcript 和父会话的 transcript 同目录，父会话的 sidecar 目录
    /// 就是它的容器。代理树因此和目录树同构，删除父会话会一并删除整棵子树。
    ///
    /// 父会话在磁盘上不存在（从未写盘的父会话）时按顶层布局推导落点，保证子代理仍
    /// 写在自己的父目录里：sidecar 目录名永远等于会话 ID。
    pub async fn paths_for_child(
        &self,
        cwd: &Path,
        parent_session_id: &str,
        session_id: &str,
        created_at_ms: u64,
    ) -> Result<SessionPaths, CoreError> {
        let parent_sidecar = match self.find(parent_session_id).await? {
            Some(paths) => paths.sidecar,
            None => self.paths_for(cwd, parent_session_id, 0).sidecar,
        };
        Ok(self.paths_in(&parent_sidecar, session_id, created_at_ms))
    }

    /// 会话日志和 sidecar 的位置；`directory` 是它们共同的父目录。
    fn paths_in(&self, directory: &Path, session_id: &str, created_at_ms: u64) -> SessionPaths {
        SessionPaths {
            transcript: directory.join(format!(
                "{}_{}.jsonl",
                timestamp_name(created_at_ms),
                session_id
            )),
            sidecar: directory.join(session_id),
        }
    }

    /// 按会话 ID 查找路径，必要时扫描磁盘目录。
    pub async fn find(&self, session_id: &str) -> Result<Option<SessionPaths>, CoreError> {
        if let Some(paths) = self.index.read().get(session_id).cloned() {
            return Ok(Some(paths));
        }
        let suffix = format!("_{session_id}");
        for path in self.journals().await? {
            if path
                .file_stem()
                .is_some_and(|stem| stem.to_string_lossy().ends_with(&suffix))
            {
                let paths = SessionPaths {
                    transcript: path.clone(),
                    sidecar: path.parent().unwrap_or(&self.root).join(session_id),
                };
                self.index
                    .write()
                    .insert(session_id.to_owned(), paths.clone());
                return Ok(Some(paths));
            }
        }
        Ok(None)
    }

    /// 遍历目录树找出全部会话日志：第一层是工作目录分桶，再往下只进会话目录。
    ///
    /// 子代理日志落在父会话目录里，所以「再深一层」就等于「代理树的一层」；只进
    /// `ses_` 目录也能把 jobs、artifacts、plans、compaction 这些同级目录挡在外面。
    async fn journals(&self) -> Result<Vec<PathBuf>, CoreError> {
        let mut found = Vec::new();
        let mut pending = vec![(self.root.clone(), 0usize)];
        while let Some((directory, depth)) = pending.pop() {
            let mut entries = match fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if entry.file_type().await?.is_dir() {
                    if depth == 0 || is_session_directory(&path) {
                        pending.push((path, depth + 1));
                    }
                    continue;
                }
                if let Some(journal) = super::maintenance::journal_path(&path) {
                    found.push(journal);
                }
            }
        }
        Ok(found)
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

    /// 读取并反序列化完整的 JSONL 历史。
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

    /// 流式读取元信息，跳过消息内容的分配，并按更新时间从新到旧排序。
    /// 只有 header、从未产生 entry 的空会话不会出现在列表里。
    pub async fn list(&self) -> Result<Vec<moc_protocol::methods::SessionInfo>, CoreError> {
        let mut result = Vec::new();
        for path in self.journals().await? {
            let Some(mut info) = listed_session(&path).await else {
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
        result.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(result)
    }

    /// 删除 transcript 和 sidecar；锁文件由调用方按语义先释放。
    ///
    /// sidecar 目录同时是子代理 transcript 的容器，所以删除父会话会一并删除整棵
    /// 子代理子树（与代理树、目录树同构）。
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

/// 会话目录的目录名就是会话 ID。子代理按父会话目录归档，所以识别它就等于识别
/// 代理树的下一层。
fn is_session_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(crate::ids::SESSION_PREFIX))
}

/// 列表用的会话元信息；单个文件损坏时只跳过它并告警，不让整个列表不可用。
async fn listed_session(path: &Path) -> Option<moc_protocol::methods::SessionInfo> {
    match read_session_info(path).await {
        Ok(Some((info, true))) => Some(info),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "会话文件无法读取，已从 session/list 跳过");
            None
        }
    }
}

/// 读取 header 和少量元信息；`has_entries` 为 `false` 表示文件只有 header，
/// 该会话从未产生对话内容（写盘前就被丢弃的空会话）。
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
    // 忽略大消息字段，不必构造正文、工具参数或思维链的对象树。
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
        let metadata: Metadata = match serde_json::from_str(&line) {
            Ok(metadata) => metadata,
            Err(error) => {
                // 一条记录损坏只影响它自己的元信息，其余记录照常参与列表。
                tracing::warn!(path = %path.display(), %error, "跳过损坏的 transcript 记录");
                continue;
            }
        };
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
    use crate::session::entries::SessionOrigin;

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
            SessionOrigin::root(),
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
                agent: None,
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

    #[tokio::test]
    async fn list_skips_corrupted_records_without_failing_the_whole_page() {
        use super::super::entries::EntryPayload;
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SessionStore::new(directory.path());
        let cwd = Path::new("/tmp/project");

        let healthy = Entry::header(
            "ses_healthy0001".into(),
            "/tmp/project".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "健康".into(),
            1,
        );
        let healthy_paths = store.paths_for(cwd, "ses_healthy0001", 1);
        let file = SessionFile::create_with_header(&healthy_paths.transcript, &healthy)
            .await
            .expect("创建会话文件");
        let message = Entry::from_payload(
            EntryPayload::Message {
                message: moc_llm::Message::user_text("第一条"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            healthy.id().into(),
            2,
        );
        file.append(&message, None).await.expect("追加消息");

        // 损坏记录后面的合法记录仍然必须生效。
        let broken = Entry::header(
            "ses_broken00001".into(),
            "/tmp/project".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "原始标题".into(),
            3,
        );
        let broken_paths = store.paths_for(cwd, "ses_broken00001", 3);
        SessionFile::create_with_header(&broken_paths.transcript, &broken)
            .await
            .expect("创建会话文件");
        let title = Entry::from_payload(
            EntryPayload::TitleChange {
                title: "损坏行之后的标题".into(),
            },
            broken.id().into(),
            4,
        );
        let mut bytes = std::fs::read(&broken_paths.transcript).expect("读取 JSONL");
        bytes.extend_from_slice(b"{\"type\":\"title_change\",\n");
        bytes.extend_from_slice(serde_json::to_string(&title).expect("序列化").as_bytes());
        bytes.push(b'\n');
        std::fs::write(&broken_paths.transcript, &bytes).expect("写入损坏记录");

        // header 损坏的文件整条跳过，其它文件照常列出。
        let unreadable = store.paths_for(cwd, "ses_unread0001", 5);
        std::fs::write(&unreadable.transcript, b"{\"type\":\n").expect("写入损坏 header");

        let listed = store.list().await.expect("列表必须可用");
        assert_eq!(listed.len(), 2);
        assert!(!listed.iter().any(|info| info.id == "ses_unread0001"));
        let broken = listed
            .iter()
            .find(|info| info.id == "ses_broken00001")
            .expect("含损坏记录的会话仍在列表中");
        assert_eq!(broken.title, "损坏行之后的标题");
        assert_eq!(broken.updated_at_ms, 4);
        assert!(listed.iter().any(|info| info.id == "ses_healthy0001"));
    }

    /// 写一份最小可列出的会话日志：header 加一条消息。
    async fn write_session(paths: &SessionPaths, id: &str) {
        let header = Entry::header(
            id.into(),
            "/tmp/project".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            id.into(),
            10,
        );
        let file = SessionFile::create_with_header(&paths.transcript, &header)
            .await
            .expect("创建会话文件");
        let message = Entry::from_payload(
            crate::session::entries::EntryPayload::Message {
                message: moc_llm::Message::user_text("内容"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().into(),
            11,
        );
        file.append(&message, None).await.expect("追加消息");
    }

    /// 子代理落在父会话目录里：代理树和目录树同构，列表和查找都要穿过会话目录才能
    /// 发现它，删除父会话会一并删除整棵子树；非会话目录里的日志一律不算会话。
    #[tokio::test]
    async fn child_transcripts_nest_under_the_parent_session_directory() {
        let directory = tempfile::tempdir().expect("临时目录");
        let store = SessionStore::new(directory.path().join("sessions"));
        let cwd = Path::new("/tmp/project");
        let parent = "ses_parent000001";
        let child = "ses_child0000001";

        let parent_paths = store.paths_for(cwd, parent, 10);
        write_session(&parent_paths, parent).await;
        let child_paths = store
            .paths_for_child(cwd, parent, child, 20)
            .await
            .expect("子代理路径");
        assert_eq!(
            child_paths.transcript.parent(),
            Some(parent_paths.sidecar.as_path()),
            "transcript 必须和父会话 sidecar 同目录，urlfs 的相对链接才能继续成立"
        );
        assert_eq!(child_paths.sidecar, parent_paths.sidecar.join(child));
        write_session(&child_paths, child).await;
        // 同级的 jobs 不是会话目录，里面的日志不算会话。
        tokio::fs::create_dir_all(parent_paths.jobs())
            .await
            .expect("jobs 目录");
        write_session(
            &store.paths_in(&parent_paths.jobs(), "ses_fake00000001", 30),
            "ses_fake00000001",
        )
        .await;

        let listed = store.list().await.expect("列出会话");
        let mut ids: Vec<&str> = listed.iter().map(|info| info.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, [child, parent], "只有会话目录里的日志会被列出");
        assert_eq!(
            store
                .find(child)
                .await
                .expect("查找子代理")
                .expect("子代理存在")
                .transcript,
            child_paths.transcript
        );

        // 父会话还没写盘时也按父目录归档：sidecar 目录名永远等于会话 ID。
        let pending = store
            .paths_for_child(cwd, "ses_pending00001", "ses_pendingchild1", 40)
            .await
            .expect("未落盘父会话的落点");
        assert_eq!(
            pending.sidecar,
            store
                .paths_for(cwd, "ses_pending00001", 0)
                .sidecar
                .join("ses_pendingchild1")
        );

        store
            .delete(parent, &parent_paths)
            .await
            .expect("删除父会话");
        assert!(!parent_paths.sidecar.exists());
        assert!(!child_paths.transcript.exists());
        assert!(!child_paths.sidecar.exists());
    }
}
