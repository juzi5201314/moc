//! 会话表、追加式 transcript，以及每个会话的提示互斥。

mod archive;
pub mod entries;
pub mod lock;
pub(crate) mod log;
pub mod maintenance;
pub(crate) mod search;
pub mod state;
pub mod store;
pub mod stored;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::error::CoreError;

use self::entries::Entry;
use self::state::SessionState;
use self::store::{SessionFile, SessionPaths, SessionStore};

/// transcript 条目 ID，避免内部调用时和会话 ID 混淆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntryId(String);

impl TranscriptEntryId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 栈层在被前滚切分前保持不变；稀疏字符串 ID 用哈希索引定位。
struct RewindTail {
    entries: Vec<Entry>,
    positions: HashMap<String, usize>,
    message_tip: Option<String>,
}

impl From<Vec<Entry>> for RewindTail {
    fn from(entries: Vec<Entry>) -> Self {
        let mut positions = HashMap::with_capacity(entries.len());
        let mut message_tip = None;
        for (index, entry) in entries.iter().enumerate() {
            positions.entry(entry.id().to_owned()).or_insert(index);
            if matches!(entry, Entry::Message { .. }) {
                message_tip = Some(entry.id().to_owned());
            }
        }
        Self {
            entries,
            positions,
            message_tip,
        }
    }
}

impl std::ops::Deref for RewindTail {
    type Target = [Entry];
    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl RewindTail {
    fn position(&self, id: &str) -> Option<usize> {
        let position = self.positions.get(id).copied()?;
        debug_assert!(position < self.entries.len());
        Some(position)
    }
}

/// 一份加载中的会话。
pub struct SessionRecord {
    pub state: Arc<Mutex<SessionState>>,
    pub prompt_lock: Arc<Mutex<()>>,
    pub(crate) skills_compacting: AtomicBool,
    append_lock: Mutex<()>,
    active: AtomicBool,
    /// 曾打开写模式的会话拥有运行态，不能作为只读缓存自动回收。
    pinned: AtomicBool,
    accessed: std::sync::atomic::AtomicU64,
    cache_bytes: usize,
    pub paths: SessionPaths,
    /// 磁盘上还没有 transcript 文件时为 `None`；第一次 append 时才写入磁盘。
    file: Mutex<Option<SessionFile>>,
    header: Entry,
    /// 跨进程写租约；`None` 表示本进程只读打开或尚未打开写模式。
    ///
    /// 用同步锁包装：只在取/放租约的短临界区里访问，不跨 `await`。
    /// 结果事务共享租约句柄，卸载不能在在途写入完成前释放内核锁。
    lease: parking_lot::Mutex<Option<Arc<lock::SessionLockGuard>>>,
    /// 串行化「检查 → 取租约 → 对齐磁盘」的整个流程，避免两个并发请求同时去抢内核锁。
    lease_gate: Mutex<()>,
    /// 回退截掉的尾部，栈式存放供 `redo` 前滚；每次回退压一层，栈顶是当前可接回的
    /// 那段。栈顶首条的 `parent_id` 接不上当前 tip 时整条废弃时间线作废（清空）。
    ///
    /// 用同步锁包装：只在取/放栈的短临界区里访问，不跨 `await`。
    rewind_stash: parking_lot::Mutex<Vec<RewindTail>>,
    /// 被回退孤立的子代理 id：截掉的尾部引用过、但保留前缀不引用的那些。
    /// redo 把引用接回时摘除对应标记；新历史落盘作废 stash 后标记仍保留——
    /// 那条分支已永久死去，名册继续对它们不可见。
    ///
    /// 与 transcript 截断同为永久事实，所以写进父会话 sidecar 的
    /// `orphaned_agents` 文件（每行一个 id）持久化：进程重启后孤儿标记仍有效，
    /// 这点和仅限进程内的 `rewind_stash` 不同。用同步锁包装：只在取/放集合的
    /// 短临界区里访问，不跨 `await`。
    pub(crate) orphaned_agents: parking_lot::Mutex<BTreeSet<String>>,
}

impl SessionRecord {
    pub fn try_prompt_lock(&self) -> Result<OwnedMutexGuard<()>, CoreError> {
        let guard = Arc::clone(&self.prompt_lock)
            .try_lock_owned()
            .map_err(|_| CoreError::Session("SESSION_BUSY: 会话已有进行中的提示".into()))?;
        if !self.active.load(Ordering::Acquire) {
            return Err(CoreError::Session(
                "SESSION_NOT_FOUND: 会话已卸载或删除".into(),
            ));
        }
        Ok(guard)
    }
}

/// 会话生命周期管理器。
pub struct SessionTable {
    store: SessionStore,
    locking: bool,
    records: RwLock<HashMap<String, Arc<SessionRecord>>>,
    /// 只合并同一个 ID 的冷加载；弱引用让完成或取消的加载不留下永久锁表。
    loading: parking_lot::Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
}

mod configuration;
mod history;
mod lease;
mod lifecycle;
mod persistence;
mod plugin_state;
mod rewind;

impl SessionTable {
    pub fn new(root: impl Into<PathBuf>, locking: bool) -> Self {
        Self {
            store: SessionStore::new(root),
            locking,
            records: RwLock::new(HashMap::new()),
            loading: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

fn validate_plugin_state_component(value: &str, label: &str) -> Result<(), CoreError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || value.as_bytes()[0].is_ascii_digit()
        || value.as_bytes()[0] == b'-'
    {
        return Err(CoreError::Session(format!(
            "插件状态 {label} 不合法：需为 1-64 位小写字母、数字或连字符，且以字母开头"
        )));
    }
    Ok(())
}

fn validate_plugin_state_key(plugin: &str, key: &str) -> Result<(), CoreError> {
    validate_plugin_state_component(plugin, "名")?;
    if key.is_empty() || key.len() > 128 {
        return Err(CoreError::Session(
            "插件状态 key 必须是 1..128 UTF-8 字节".into(),
        ));
    }
    Ok(())
}

mod cache_budget;
mod projection;
use cache_budget::message_storage_bytes;

fn sanitize_title(title: &str) -> String {
    title
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(60)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// 孤儿标记文件路径：父会话 sidecar 下每行一个被孤立子代理的 id。
fn orphans_path(paths: &SessionPaths) -> PathBuf {
    paths.sidecar.join("orphaned_agents")
}

/// 读取持久化的孤儿标记；文件不存在或读不出都按空集合处理——最坏情况是
/// 孤儿重新出现在名册里，与未打标记的行为一致，不会误伤正常子代理。
async fn load_orphans(paths: &SessionPaths) -> BTreeSet<String> {
    match tokio::fs::read_to_string(orphans_path(paths)).await {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// 把孤儿标记写回磁盘：空集合删除文件；非空按临时文件 + rename 原子发布，
/// 和 transcript rewrite 同一套崩溃语义。
async fn persist_orphans(paths: &SessionPaths, ids: &BTreeSet<String>) -> Result<(), CoreError> {
    let file = orphans_path(paths);
    if ids.is_empty() {
        match tokio::fs::remove_file(&file).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(CoreError::Io(error)),
        }
    }
    tokio::fs::create_dir_all(&paths.sidecar).await?;
    let temp = file.with_extension(format!("tmp-{}", crate::ids::entry_id()));
    let mut text = ids.iter().cloned().collect::<Vec<_>>().join("\n");
    text.push('\n');
    tokio::fs::write(&temp, text).await?;
    tokio::fs::rename(&temp, &file).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
