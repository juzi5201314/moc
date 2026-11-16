//! transcript 的存储和原子历史重写。
//!
//! 普通会话是逐行 JSONL；开启 zstd 压缩的新会话是 `.jsonl.zst`，由独立的完整帧拼接。
//! 两种格式的读写都封装在这里，上层只接触 [`Entry`]。

use rust_i18n::t;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::CoreError;

use super::entries::Entry;
use super::history_cache::HistoryCache;
use super::log;

/// 不超过该长度的记录直接在异步线程压缩：spawn_blocking 的调度开销比压缩本身还大。
const INLINE_COMPRESS_LIMIT: usize = 64 * 1024;
/// 每个会话中单个插件的状态序列化上限；读写都受此限制。
const PLUGIN_STATE_MAX_BYTES: usize = 1024 * 1024;

/// 一个会话的 transcript 和 sidecar 路径。
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub transcript: PathBuf,
    pub sidecar: PathBuf,
    history_cache: Arc<OnceLock<HistoryCache>>,
}

impl SessionPaths {
    pub fn new(transcript: PathBuf, sidecar: PathBuf) -> Self {
        Self {
            transcript,
            sidecar,
            history_cache: Arc::new(OnceLock::new()),
        }
    }

    fn history_cache(&self) -> &HistoryCache {
        // 初始化依赖本会话的 sidecar；克隆必须共享同一个尚未初始化的缓存。
        self.history_cache
            .get_or_init(|| HistoryCache::new(&self.sidecar))
    }

    pub(crate) fn history_cache_dir(&self) -> &Path {
        self.history_cache().directory()
    }

    pub(crate) fn ensure_history_cache(&self) -> std::io::Result<()> {
        self.history_cache().ensure()
    }

    pub(crate) fn clear_history_cache(&self) {
        if let Some(cache) = self.history_cache.get() {
            cache.clear();
        }
    }

    pub(crate) fn urlfs_root(&self) -> PathBuf {
        if self.compressed() {
            self.history_cache_dir().join("urlfs")
        } else {
            self.sidecar.join("urlfs")
        }
    }

    /// 会话日志是否使用 zstd 压缩；扩展名在会话创建时固定。
    pub fn compressed(&self) -> bool {
        self.transcript
            .extension()
            .is_some_and(|extension| extension == "zst")
    }

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

    /// workflow run 目录：`<sidecar>/workflows/<run_id>/`（run.json/journal/script/artifacts）。
    pub fn workflows(&self) -> PathBuf {
        self.sidecar.join("workflows")
    }
    pub fn compaction(&self) -> PathBuf {
        self.sidecar.join("compaction")
    }

    /// 一个插件在本会话里的 KV 状态文件；插件之间使用独立文件隔离。
    pub fn plugin_state(&self, plugin: &str) -> PathBuf {
        self.sidecar.join("plugins").join(format!("{plugin}.json"))
    }
}

/// 单个会话的 JSONL 文件句柄；磁盘上还没有文件时延迟创建。
///
/// 新会话创建时只登记内存，第一条消息 entry 才把 header（以及已缓冲的簿记
/// 条目）真正写入磁盘。这样只有配置变更、从未产生对话内容的空会话不会留下
/// JSONL 文件，也不会出现在 `session/list` 里。
pub struct SessionFile {
    path: PathBuf,
    format: log::Format,
    file: tokio::sync::Mutex<Option<File>>,
    revision: RwLock<crate::hashline::SnapshotId>,
}

mod listing;
mod transcript;
use listing::read_session_info;
use transcript::read_entries_sync;

type Listings = HashMap<
    PathBuf,
    (
        crate::hashline::SnapshotId,
        Option<moc_protocol::SessionInfo>,
    ),
>;
const LISTING_CACHE: &str = ".session-list-v3.json";
const LISTING_CACHE_BYTES: u64 = 16 * 1024 * 1024;

/// 会话文件的发现、创建、读取和删除。
pub struct SessionStore {
    root: PathBuf,
    index: RwLock<HashMap<String, SessionPaths>>,
    /// 元信息缓存只在日志版本完全相同时复用；跨进程修改仍以磁盘为准。
    listings: RwLock<Listings>,
    listing_loaded: std::sync::atomic::AtomicBool,
    listing_dirty: std::sync::atomic::AtomicBool,
    /// 多个前端同时刷新时只允许一次冷扫描。
    listing_gate: tokio::sync::Mutex<()>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            index: RwLock::new(HashMap::new()),
            listings: RwLock::new(HashMap::new()),
            listing_loaded: std::sync::atomic::AtomicBool::new(false),
            listing_dirty: std::sync::atomic::AtomicBool::new(false),
            listing_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn clear_history_views(&self) {
        for paths in self.index.read().values() {
            paths.clear_history_cache();
        }
    }

    /// 只计算明文 transcript 和 sidecar 的路径，不访问磁盘。
    pub fn paths_for(&self, cwd: &Path, session_id: &str, created_at_ms: u64) -> SessionPaths {
        self.paths_in(
            &self.root.join(encode_cwd(cwd)),
            session_id,
            created_at_ms,
            false,
        )
    }

    /// 只计算压缩 transcript 和 sidecar 的路径，不访问磁盘。
    pub fn paths_for_compressed(
        &self,
        cwd: &Path,
        session_id: &str,
        created_at_ms: u64,
    ) -> SessionPaths {
        self.paths_in(
            &self.root.join(encode_cwd(cwd)),
            session_id,
            created_at_ms,
            true,
        )
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
        compressed: bool,
    ) -> Result<SessionPaths, CoreError> {
        let parent_sidecar = match self.find(parent_session_id).await? {
            Some(paths) => paths.sidecar,
            None => self.paths_for(cwd, parent_session_id, 0).sidecar,
        };
        Ok(self.paths_in(&parent_sidecar, session_id, created_at_ms, compressed))
    }

    /// 会话日志和 sidecar 的位置；`directory` 是它们共同的父目录。
    fn paths_in(
        &self,
        directory: &Path,
        session_id: &str,
        created_at_ms: u64,
        compressed: bool,
    ) -> SessionPaths {
        SessionPaths::new(
            directory.join(format!(
                "{}_{}.jsonl{}",
                timestamp_name(created_at_ms),
                session_id,
                if compressed { ".zst" } else { "" }
            )),
            directory.join(session_id),
        )
    }

    /// 读取单个插件状态；文件缺失视为空，损坏只影响该插件调用。
    pub async fn read_plugin_state(
        &self,
        paths: &SessionPaths,
        plugin: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>, CoreError> {
        let path = paths.plugin_state(plugin);
        let file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => {
                return Err(CoreError::Session(
                    t!("core.err.plugin_state_read", error = error).into_owned(),
                ));
            }
        };
        let mut bytes = Vec::new();
        file.take((PLUGIN_STATE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > PLUGIN_STATE_MAX_BYTES {
            return Err(CoreError::Session(
                t!("core.err.plugin_state_too_big").into_owned(),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            CoreError::Session(
                t!(
                    "core.err.plugin_state_corrupt",
                    path = path.display().to_string(),
                    error = error
                )
                .into_owned(),
            )
        })
    }

    /// 原子替换单个插件状态文件；写入前检查序列化大小。
    pub async fn write_plugin_state(
        &self,
        paths: &SessionPaths,
        plugin: &str,
        state: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), CoreError> {
        let path = paths.plugin_state(plugin);
        let bytes = serde_json::to_vec(state).map_err(|error| {
            CoreError::Session(t!("core.err.plugin_state_serialize", error = error).into_owned())
        })?;
        if bytes.len() > PLUGIN_STATE_MAX_BYTES {
            return Err(CoreError::Session(
                t!(
                    "core.err.plugin_state_write_big",
                    mib = PLUGIN_STATE_MAX_BYTES / (1024 * 1024)
                )
                .into_owned(),
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            CoreError::Session(t!("core.err.plugin_state_no_parent").into_owned())
        })?;
        fs::create_dir_all(parent).await?;
        let temp = parent.join(format!(".{plugin}.tmp-{}", crate::ids::entry_id()));
        let result = async {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temp).await?;
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.sync_data().await?;
            drop(file);
            fs::rename(&temp, &path).await?;
            Ok::<(), CoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp).await;
        }
        result
    }

    /// 按会话 ID 查找路径，必要时扫描磁盘目录。
    pub async fn find(&self, session_id: &str) -> Result<Option<SessionPaths>, CoreError> {
        if let Some(paths) = self.index.read().get(session_id).cloned() {
            return Ok(Some(paths));
        }
        let suffix = format!("_{session_id}");
        for path in self.journals().await? {
            let matches = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| {
                    name.strip_suffix(".jsonl.zst")
                        .or_else(|| name.strip_suffix(".jsonl"))
                })
                .is_some_and(|stem| stem.ends_with(&suffix));
            if matches {
                let paths = SessionPaths::new(
                    path.clone(),
                    path.parent().unwrap_or(&self.root).join(session_id),
                );
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
            format: log::format(&paths.transcript),
            file: tokio::sync::Mutex::new(Some(file)),
            revision,
        })
    }

    /// 读取并反序列化完整历史。
    ///
    /// 明文最后一段不以换行结束时视为写者追加到一半，直接丢弃；压缩会话忽略不完整的
    /// 最后一帧。中间记录或完整帧损坏仍然报错。
    pub async fn read_entries(&self, paths: &SessionPaths) -> Result<Vec<Entry>, CoreError> {
        let path = paths.transcript.clone();
        tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            read_entries_sync(&path, &mut |entry| {
                entries.push(entry);
                Ok(())
            })?;
            Ok(entries)
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }

    pub async fn read_state(
        &self,
        paths: &SessionPaths,
    ) -> Result<super::state::SessionState, CoreError> {
        let path = paths.transcript.clone();
        tokio::task::spawn_blocking(move || {
            let mut replay = super::archive::Replayer::default();
            read_entries_sync(&path, &mut |entry| replay.push(entry))?;
            replay.finish()
        })
        .await
        .map_err(|error| CoreError::Session(error.to_string()))?
    }

    pub(super) fn cache_listing(
        &self,
        path: &Path,
        revision: crate::hashline::SnapshotId,
        info: Option<moc_protocol::SessionInfo>,
    ) {
        let mut listings = self.listings.write();
        if listings
            .get(path)
            .is_some_and(|(stored_revision, stored_info)| {
                *stored_revision == revision && stored_info == &info
            })
        {
            return;
        }
        listings.insert(path.to_owned(), (revision, info));
        self.listing_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 只在日志版本变化时扫描元信息，并按更新时间从新到旧排序。
    /// 只有 header、或只有簿记条目（换模型/权限/引擎等，没有任何消息）的
    /// 会话不会出现在列表里。
    pub async fn list(&self) -> Result<Vec<moc_protocol::methods::SessionInfo>, CoreError> {
        let _listing = self.listing_gate.lock().await;
        if !self
            .listing_loaded
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            // v1 会列出只有簿记的幽灵会话，v2 可能采纳 checksum 未通过的帧；旧投影作废。
            for obsolete in [".session-list-v1.json", ".session-list-v2.json"] {
                let _ = tokio::fs::remove_file(self.root.join(obsolete)).await;
            }
            let path = self.root.join(LISTING_CACHE);
            let cached = tokio::task::spawn_blocking(move || read_listing_cache(&path))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))?;
            let mut listings = self.listings.write();
            for (path, info) in cached {
                listings.entry(path).or_insert(info);
            }
        }
        let paths = self.journals().await?;
        let existing: std::collections::HashSet<&PathBuf> = paths.iter().collect();
        {
            let mut listings = self.listings.write();
            let before = listings.len();
            listings.retain(|path, _| existing.contains(path));
            if listings.len() != before {
                self.listing_dirty
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let mut result = Vec::with_capacity(paths.len());
        for path in paths {
            let revision = match fs::metadata(&path).await {
                Ok(metadata) => crate::hashline::SnapshotId::from_metadata(&metadata),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "会话文件无法读取，已从 session/list 跳过");
                    continue;
                }
            };
            let cached = self
                .listings
                .read()
                .get(&path)
                .filter(|(stored, _)| *stored == revision)
                .map(|(_, info)| info.clone());
            let info = match cached {
                Some(info) => info,
                None => {
                    let info = listed_session(&path).await;
                    // 扫描期间写者可能追加；只缓存前后版本一致的结果，不能把旧投影
                    // 贴到新版本上，否则最新一条元信息会一直消失。
                    if fs::metadata(&path).await.is_ok_and(|metadata| {
                        crate::hashline::SnapshotId::from_metadata(&metadata) == revision
                    }) {
                        self.cache_listing(&path, revision, info.clone());
                    }
                    info
                }
            };
            let Some(mut info) = info else { continue };
            info.lock = super::lock::read(
                path.parent()
                    .unwrap_or(&self.root)
                    .join(format!("{}.lock", info.id)),
            );
            self.index.write().insert(
                info.id.clone(),
                SessionPaths::new(
                    path.clone(),
                    path.parent().unwrap_or(&self.root).join(&info.id),
                ),
            );
            result.push(info);
        }
        if self
            .listing_dirty
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            let cached = self.listings.read().clone();
            let path = self.root.join(LISTING_CACHE);
            let saved = tokio::task::spawn_blocking(move || write_listing_cache(&path, &cached))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))?;
            if let Err(error) = saved {
                self.listing_dirty
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(%error, "会话列表缓存写回失败，下次仍可从日志重建");
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

/// 这是派生缓存而非日志；不完整、旧版本或损坏的内容都不影响会话恢复。
fn read_listing_cache(path: &Path) -> Listings {
    use std::io::Read;
    let result = (|| -> std::io::Result<Listings> {
        let file = std::fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take(LISTING_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LISTING_CACHE_BYTES {
            return Ok(Listings::new());
        }
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    })();
    match result {
        Ok(cached) => cached,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(%error, "会话列表缓存无效，将从日志重建");
            }
            Listings::new()
        }
    }
}

fn write_listing_cache(path: &Path, cached: &Listings) -> std::io::Result<()> {
    use std::io::Write;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("列表缓存位于会话根目录"))?;
    {
        let mut writer = std::io::BufWriter::new(temp.as_file_mut());
        serde_json::to_writer(&mut writer, cached).map_err(std::io::Error::other)?;
        writer.flush()?;
    }
    // 极端数量的会话仍可使用内存索引，不生成下次无法有界读取的缓存文件。
    if temp.as_file().metadata()?.len() <= LISTING_CACHE_BYTES {
        temp.persist(path).map_err(|error| error.error)?;
    }
    Ok(())
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

pub(crate) fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
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
mod tests;
