//! 连接级上下文快照：分类目录按页读取，隐藏内容从不进入可读节点或落盘缓存。
use super::CoreInner;
#[cfg(test)]
mod tests;
use moc_protocol::methods::*;
use moc_protocol::{RequestContext, RpcError, RpcErrorCode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(300);
const PAGE: usize = 128;
const TEXT_PAGE: usize = 64 * 1024;
pub(super) const TEXT_CACHE_BYTES: usize = 8 * 1024 * 1024;

struct Node {
    meta: ContextContentSection,
    text: Option<String>,
    spilled: Option<(u64, usize)>,
    children: Vec<usize>,
}
pub(super) struct View {
    id: String,
    touched: Instant,
    usage: ContextUsageResult,
    model: String,
    error: Option<String>,
    nodes: Vec<Node>,
    roots: Vec<usize>,
    spool: Option<parking_lot::Mutex<std::fs::File>>,
    memory_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl View {
    pub(super) fn new(usage: ContextUsageResult, model: String) -> Self {
        let mut view = Self {
            id: uuid::Uuid::now_v7().to_string(),
            touched: Instant::now(),
            usage,
            model,
            error: None,
            nodes: Vec::new(),
            roots: Vec::new(),
            spool: None,
            memory_permit: None,
        };
        for section in &view.usage.sections {
            let index = view.nodes.len();
            view.nodes.push(Node {
                meta: ContextContentSection {
                    id: index.to_string(),
                    name: section.name.clone(),
                    kind: "category".into(),
                    tokens: section.tokens,
                    ..Default::default()
                },
                text: None,
                spilled: None,
                children: Vec::new(),
            });
            view.roots.push(index);
        }
        view
    }

    pub(super) fn set_error(&mut self, error: impl std::fmt::Display) {
        self.error = Some(format!("当前上下文尚未形成可发送请求：{error}"));
    }

    pub(super) fn add(
        &mut self,
        category: &str,
        mut meta: ContextContentSection,
        text: Option<String>,
    ) {
        let parent = self
            .roots
            .iter()
            .copied()
            .find(|index| self.nodes[*index].meta.name == category)
            .expect("条目所属分类已由同一快照的用量生成");
        debug_assert!(
            meta.hidden.is_none() || text.is_none(),
            "隐藏正文不能进入快照"
        );
        let index = self.nodes.len();
        meta.id = index.to_string();
        self.nodes.push(Node {
            meta,
            text,
            spilled: None,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(index);
        self.nodes[parent].meta.children += 1;
    }

    fn directory(
        &self,
        ids: &[usize],
        cursor: Option<u64>,
        limit: Option<u32>,
    ) -> Result<(Vec<ContextContentSection>, Option<u64>), RpcError> {
        let start = cursor.unwrap_or(0) as usize;
        if start > ids.len() {
            return Err(invalid("目录游标越界"));
        }
        let end = start
            .saturating_add(limit.unwrap_or(PAGE as u32).clamp(1, PAGE as u32) as usize)
            .min(ids.len());
        Ok((
            ids[start..end]
                .iter()
                .map(|index| self.nodes[*index].meta.clone())
                .collect(),
            (end < ids.len()).then_some(end as u64),
        ))
    }
    pub(super) fn page(
        &self,
        cursor: Option<u64>,
        limit: Option<u32>,
    ) -> Result<ContextContentResult, RpcError> {
        let (sections, next_cursor) = self.directory(&self.roots, cursor, limit)?;
        Ok(ContextContentResult {
            view_id: self.id.clone(),
            context_window: self.usage.context_window,
            total_tokens: self.usage.total_tokens,
            model: self.model.clone(),
            sections,
            next_cursor,
            error: self.error.clone(),
        })
    }
    fn text_bytes(&self) -> usize {
        self.nodes
            .iter()
            .filter_map(|node| node.text.as_ref())
            .fold(0usize, |bytes, text| bytes.saturating_add(text.len()))
    }

    /// 在后台构建完成后执行；大视图的字符串不常驻内存，节点与来源仍完整保留。
    pub(super) fn bound_text(&mut self, limit: usize) -> std::io::Result<()> {
        use std::io::Write;
        if self.text_bytes() <= limit {
            return Ok(());
        }
        let mut file = tempfile::tempfile()?;
        let mut offset = 0u64;
        for node in &mut self.nodes {
            if let Some(text) = node.text.take() {
                let text = text.as_str();
                file.write_all(text.as_bytes())?;
                node.spilled = Some((offset, text.len()));
                offset += text.len() as u64;
            }
        }
        self.spool = Some(parking_lot::Mutex::new(file));
        self.memory_permit = None;
        Ok(())
    }

    fn read(&self, params: &SessionContextReadParams) -> Result<ContextReadResult, RpcError> {
        let index = params
            .node_id
            .parse::<usize>()
            .map_err(|_| invalid("无效节点"))?;
        let node = self.nodes.get(index).ok_or_else(|| invalid("节点不存在"))?;
        if let Some(reason) = &node.meta.hidden {
            return Err(invalid(reason));
        }
        let mut result = ContextReadResult {
            view_id: self.id.clone(),
            node_id: params.node_id.clone(),
            ..Default::default()
        };
        if node.text.is_some() || node.spilled.is_some() {
            let start = params.cursor.unwrap_or(0) as usize;
            let limit = params
                .limit
                .unwrap_or(TEXT_PAGE as u32)
                .clamp(4, TEXT_PAGE as u32) as usize;
            let (text, len) = if let Some(text) = &node.text {
                let text = text.as_str();
                if start > text.len() || !text.is_char_boundary(start) {
                    return Err(invalid("正文游标必须位于 UTF-8 字符边界"));
                }
                let mut end = start.saturating_add(limit).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                (text[start..end].to_owned(), text.len())
            } else {
                use std::io::{Read, Seek, SeekFrom};
                let (offset, len) = node.spilled.expect("正文已写入快照");
                if start > len {
                    return Err(invalid("正文游标必须位于 UTF-8 字符边界"));
                }
                let mut bytes = vec![0; limit.min(len - start)];
                let mut file = self.spool.as_ref().expect("正文快照文件").lock();
                file.seek(SeekFrom::Start(offset + start as u64))
                    .map_err(io_error)?;
                file.read_exact(&mut bytes).map_err(io_error)?;
                if bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
                    return Err(invalid("正文游标必须位于 UTF-8 字符边界"));
                }
                if let Err(error) = std::str::from_utf8(&bytes) {
                    if error.error_len().is_some() {
                        return Err(io_error(error));
                    }
                    bytes.truncate(error.valid_up_to());
                }
                (String::from_utf8(bytes).map_err(io_error)?, len)
            };
            let end = start + text.len();
            result.text = Some(text);
            result.next_cursor = (end < len).then_some(end as u64);
        } else {
            (result.children, result.next_cursor) =
                self.directory(&node.children, params.cursor, params.limit)?;
        }
        Ok(result)
    }
}
fn io_error(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        RpcErrorCode::InternalError.code(),
        format!("读取上下文快照失败：{error}"),
    )
}
fn invalid(message: &str) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidParams.code(), message)
}

pub(crate) struct Views {
    entries: HashMap<(u64, String), View>,
    connections: HashSet<u64>,
    text_budget: Arc<tokio::sync::Semaphore>,
}
impl Default for Views {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            connections: HashSet::new(),
            text_budget: Arc::new(tokio::sync::Semaphore::new(TEXT_CACHE_BYTES)),
        }
    }
}
impl Views {
    fn get(&mut self, connection: u64, session: &str, id: &str) -> Result<&mut View, RpcError> {
        self.entries.retain(|_, view| view.touched.elapsed() < TTL);
        let view = self
            .entries
            .get_mut(&(connection, session.to_owned()))
            .filter(|view| view.id == id)
            .ok_or_else(|| invalid("上下文视图已过期或已释放，请刷新"))?;
        view.touched = Instant::now();
        Ok(view)
    }
    pub(super) fn page(
        &mut self,
        connection: u64,
        session: &str,
        id: &str,
        cursor: Option<u64>,
        limit: Option<u32>,
    ) -> Result<ContextContentResult, RpcError> {
        self.get(connection, session, id)?.page(cursor, limit)
    }
    pub(crate) fn remove_session(&mut self, session: &str) {
        self.entries.retain(|(_, id), _| id != session);
    }
}
pub(super) async fn install(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session: &str,
    mut view: View,
) -> Result<(), RpcError> {
    let budget = Arc::clone(&inner.context_views.lock().text_budget);
    let bytes = view.text_bytes();
    debug_assert!(bytes <= TEXT_CACHE_BYTES, "单视图正文已在后台收窄");
    if bytes > 0 {
        match budget.try_acquire_many_owned(bytes as u32) {
            Ok(permit) => view.memory_permit = Some(permit),
            Err(_) => {
                view = tokio::task::spawn_blocking(move || {
                    view.bound_text(0)?;
                    Ok::<_, std::io::Error>(view)
                })
                .await
                .map_err(io_error)?
                .map_err(io_error)?;
            }
        }
    }
    let connection = ctx.connection_id();
    let fresh = {
        let mut views = inner.context_views.lock();
        views.entries.insert((connection, session.to_owned()), view);
        views.connections.insert(connection)
    };
    if !fresh {
        return Ok(());
    }
    let weak = Arc::downgrade(inner);
    let updates = ctx.updates();
    let shutdown = inner.shutdown.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _=updates.closed()=>break,
                _=shutdown.cancelled()=>break,
                _=interval.tick()=>{
                    let Some(inner)=weak.upgrade() else {return;};
                    inner.context_views.lock().entries.retain(|_,view|view.touched.elapsed()<TTL);
                }
            }
        }
        if let Some(inner) = weak.upgrade() {
            let mut views = inner.context_views.lock();
            views.entries.retain(|(id, _), _| *id != connection);
            views.connections.remove(&connection);
        }
    });
    Ok(())
}
pub(crate) async fn read(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextReadParams,
) -> Result<ContextReadResult, RpcError> {
    let inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        inner
            .context_views
            .lock()
            .get(ctx.connection_id(), &params.session_id, &params.view_id)?
            .read(&params)
    })
    .await
    .map_err(io_error)?
}
pub(crate) async fn close(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextCloseParams,
) -> Result<Empty, RpcError> {
    let mut views = inner.context_views.lock();
    let key = (ctx.connection_id(), params.session_id);
    if views
        .entries
        .get(&key)
        .is_some_and(|view| view.id == params.view_id)
    {
        views.entries.remove(&key);
    }
    Ok(Empty {})
}
