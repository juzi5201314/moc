//! 连接级、只在内存中存在的上下文树。字符串只存一份，目录分页不复制正文。
use super::CoreInner;
#[cfg(test)]
mod tests;
use moc_protocol::methods::*;
use moc_protocol::{RequestContext, RpcError, RpcErrorCode};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(300);
const PAGE: usize = 128;
const TEXT_PAGE: usize = 64 * 1024;
pub(super) const TEXT_CACHE_BYTES: usize = 8 * 1024 * 1024;

struct TextSlice {
    text: Arc<str>,
    range: std::ops::Range<usize>,
}
impl TextSlice {
    fn as_str(&self) -> &str {
        &self.text[self.range.clone()]
    }
}
impl From<String> for TextSlice {
    fn from(text: String) -> Self {
        let len = text.len();
        Self {
            text: Arc::from(text),
            range: 0..len,
        }
    }
}

struct Node {
    meta: ContextContentSection,
    text: Option<TextSlice>,
    spilled: Option<(u64, usize)>,
    children: Vec<usize>,
    sources: Vec<ContextSource>,
}
pub(super) struct View {
    id: String,
    touched: Instant,
    usage: ContextUsageResult,
    model: String,
    protocol: String,
    error: Option<String>,
    nodes: Vec<Node>,
    roots: Vec<usize>,
    spool: Option<parking_lot::Mutex<std::fs::File>>,
    memory_permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl View {
    pub(super) fn new(
        usage: ContextUsageResult,
        model: String,
        protocol: String,
        inspection: Result<moc_llm::RequestInspection, moc_llm::LlmError>,
        messages: &[String],
        instructions: &[Vec<String>],
    ) -> Self {
        let mut view = Self {
            id: uuid::Uuid::now_v7().to_string(),
            touched: Instant::now(),
            usage,
            model,
            protocol,
            error: None,
            nodes: Vec::new(),
            roots: Vec::new(),
            spool: None,
            memory_permit: None,
        };
        match inspection {
            Err(error) => view.error = Some(format!("当前上下文尚未形成可发送请求：{error}")),
            Ok(inspection) => {
                let mut labels: HashMap<String, Vec<ContextSource>> = HashMap::new();
                for source in inspection.sources {
                    let label = source
                        .text_range
                        .as_ref()
                        .and_then(|_| {
                            instructions
                                .get(source.message_index)
                                .and_then(|parts| parts.get(source.part_index))
                        })
                        .or_else(|| messages.get(source.message_index))
                        .cloned()
                        .unwrap_or_else(|| "会话上下文".into());
                    let (start, end) = source.text_range.map_or((0, u64::MAX), |range| {
                        (range.start as u64, range.end as u64)
                    });
                    labels
                        .entry(source.pointer)
                        .or_default()
                        .push(ContextSource {
                            source: label,
                            start,
                            end,
                        });
                }
                let Value::Object(mut body) = inspection.body else {
                    unreachable!("编码请求体是对象")
                };
                // 数组内顺序是协议定义的；顶层字段只有展示顺序，不宣称模型处理的先后顺序。
                for field in ["system", "messages", "input", "tools"] {
                    if let Some(value) = body.remove(field) {
                        if matches!(field, "messages" | "input") {
                            if let Value::Array(items) = value {
                                for (index, item) in items.into_iter().enumerate() {
                                    let node = view.add(
                                        item,
                                        format!("/{field}/{index}"),
                                        format!("#{}", index + 1),
                                        &labels,
                                        None,
                                    );
                                    view.roots.push(node);
                                }
                            }
                        } else {
                            let node =
                                view.add(value, format!("/{field}"), field.into(), &labels, None);
                            view.roots.push(node);
                        }
                    }
                }
                for (field, value) in body {
                    let node =
                        view.add(value, format!("/{}", escape(&field)), field, &labels, None);
                    view.roots.push(node);
                }
            }
        }
        view
    }
    fn add(
        &mut self,
        value: Value,
        path: String,
        name: String,
        labels: &HashMap<String, Vec<ContextSource>>,
        inherited: Option<String>,
    ) -> usize {
        let index = self.nodes.len();
        let sources = labels.get(&path).cloned().unwrap_or_default();
        let label = if sources.len() == 1 {
            Some(sources[0].source.clone())
        } else if sources.is_empty() {
            inherited
        } else {
            None
        };
        let role = value.get("role").and_then(Value::as_str).map(str::to_owned);
        let kind = match &value {
            Value::Object(_) => "object",
            Value::Array(_) => "array",
            Value::String(_) => "text",
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
        };
        let detail = value
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| value.get("name").and_then(Value::as_str));
        let name = if let Some(detail) = detail {
            format!("{name} · {detail}")
        } else {
            name
        };
        self.nodes.push(Node {
            meta: ContextContentSection {
                id: index.to_string(),
                path: path.clone(),
                name,
                kind: kind.into(),
                role,
                source: label.clone(),
                ..Default::default()
            },
            text: None,
            spilled: None,
            children: Vec::new(),
            sources,
        });
        let mut children = Vec::new();
        let text = match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    children.push(self.add(
                        value,
                        format!("{path}/{}", escape(&key)),
                        key,
                        labels,
                        label.clone(),
                    ));
                }
                None
            }
            Value::Array(items) => {
                for (offset, value) in items.into_iter().enumerate() {
                    children.push(self.add(
                        value,
                        format!("{path}/{offset}"),
                        format!("[{offset}]"),
                        labels,
                        label.clone(),
                    ));
                }
                None
            }
            Value::String(text) => {
                if self.nodes[index].sources.len() > 1 {
                    children = self.source_sections(index, text);
                    None
                } else {
                    Some(TextSlice::from(text))
                }
            }
            scalar => Some(TextSlice::from(scalar.to_string())),
        };
        let (bytes, tokens) = if let Some(text) = &text {
            (
                text.as_str().len() as u64,
                crate::compact::estimate_tokens(text.as_str()),
            )
        } else {
            children
                .iter()
                .map(|child| {
                    let m = &self.nodes[*child].meta;
                    (m.bytes, m.tokens)
                })
                .fold((0, 0), |(a, b), (c, d)| (a + c, b + d))
        };
        let mut child_sources = Vec::new();
        for child in &children {
            if let Some(source) = &self.nodes[*child].meta.source
                && !child_sources.contains(source)
            {
                child_sources.push(source.clone());
            }
            if child_sources.len() == 3 {
                break;
            }
        }
        let inferred_source = match child_sources.as_slice() {
            [] => None,
            [source] => Some(source.clone()),
            sources => Some(format!("混合来源 · {}", sources.join(" / "))),
        };
        let node = &mut self.nodes[index];
        if node.meta.source.is_none() {
            node.meta.source = inferred_source;
        }
        node.meta.bytes = bytes;
        node.meta.tokens = tokens;
        node.meta.children = children.len() as u64;
        node.text = text;
        node.children = children;
        index
    }
    /// 来源子块只是原文本的展示切片，路径仍指向真实字段；拼接子块必须逐字还原父文本。
    fn source_sections(&mut self, parent: usize, text: String) -> Vec<usize> {
        let mut sources = std::mem::take(&mut self.nodes[parent].sources);
        sources.sort_by_key(|source| source.start);
        let text: Arc<str> = Arc::from(text);
        let path = self.nodes[parent].meta.path.clone();
        let mut children = Vec::with_capacity(sources.len() + 1);
        let first = sources[0].start as usize;
        if first > 0 {
            children.push(self.text_section(&text, &path, 0..first, "原文前缀".into(), None));
        }
        for (index, source) in sources.iter().enumerate() {
            let start = source.start as usize;
            let end = sources
                .get(index + 1)
                .map_or(text.len(), |next| next.start as usize);
            // 坐标来自编码器；块不重叠、以 UTF-8 字符为边界，块间分隔符归前一块。
            debug_assert!(start <= source.end as usize && source.end as usize <= end);
            debug_assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
            let name = source
                .source
                .strip_prefix("系统提示 · ")
                .unwrap_or(&source.source)
                .to_owned();
            let local = ContextSource {
                source: source.source.clone(),
                start: 0,
                end: source.end - source.start,
            };
            children.push(self.text_section(&text, &path, start..end, name, Some(local)));
        }
        children
    }

    fn text_section(
        &mut self,
        text: &Arc<str>,
        path: &str,
        range: std::ops::Range<usize>,
        name: String,
        source: Option<ContextSource>,
    ) -> usize {
        let index = self.nodes.len();
        let section = &text[range.clone()];
        self.nodes.push(Node {
            meta: ContextContentSection {
                id: index.to_string(),
                path: path.to_owned(),
                name,
                kind: "text".into(),
                text_range: Some([range.start as u64, range.end as u64]),
                source: source.as_ref().map(|source| source.source.clone()),
                bytes: section.len() as u64,
                tokens: crate::compact::estimate_tokens(section),
                ..Default::default()
            },
            text: Some(TextSlice {
                text: Arc::clone(text),
                range,
            }),
            spilled: None,
            children: Vec::new(),
            sources: source.into_iter().collect(),
        });
        index
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
            protocol: self.protocol.clone(),
            sections,
            next_cursor,
            error: self.error.clone(),
        })
    }
    fn text_bytes(&self) -> usize {
        self.nodes
            .iter()
            .filter_map(|node| node.text.as_ref())
            .fold(0usize, |bytes, text| bytes.saturating_add(text.range.len()))
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
            result.sources = node
                .sources
                .iter()
                .filter(|source| source.start < end as u64 && source.end > start as u64)
                .cloned()
                .map(|mut source| {
                    source.end = source.end.min(len as u64);
                    source
                })
                .collect();
        } else {
            (result.children, result.next_cursor) =
                self.directory(&node.children, params.cursor, params.limit)?;
        }
        Ok(result)
    }
}
fn escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
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
