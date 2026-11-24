//! 引用控件：输入框与转写里的有效引用显示成「图标 名称」的整体块（与 WebUI 的 RefChip 同义）。
//!
//! 文本模型不变——控件只是显示层的替换：输入框按控件整体折行、光标整体跨过、退格整体删除；
//! 发送与历史仍是原始文本。识别规则：
//! - 输入框：开头的已注册 `/命令`、存在的 `[image#N]` / `[paste#N]` 占位、经 core
//!   `resource/inspect` 确认有效的 `@路径` 与 `skill://` 等资源 URI；逐字键入中的 token 不成形。
//! - 转写：用户消息还原出的附件标记 `[image#N]`、`[file 路径]`、`[dir 路径]`、`[#name 路径]`。

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use moc_protocol::ResourceRefInfo;

use crate::app::App;
use crate::render::labels::chip as icon;

/// 引用类别：决定图标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Command,
    Image,
    Paste,
    File,
    Dir,
    /// 按路径引用的文件（`#name`，不内联内容）。
    FileRef,
    Skill,
    Local,
    History,
    Artifact,
    Tool,
}

impl RefKind {
    fn from_inspect(kind: &str) -> Self {
        match kind {
            "dir" => Self::Dir,
            "skill" => Self::Skill,
            "local" => Self::Local,
            "history" => Self::History,
            "artifact" => Self::Artifact,
            "tool" => Self::Tool,
            _ => Self::File,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Command => icon::COMMAND,
            Self::Image => icon::IMAGE,
            Self::Paste => icon::PASTE,
            Self::File => icon::FILE,
            Self::Dir => icon::DIR,
            Self::FileRef => icon::FILE_REF,
            Self::Skill => icon::SKILL,
            Self::Local => icon::PLAN,
            Self::History => icon::HISTORY,
            Self::Artifact => icon::ARTIFACT,
            Self::Tool => icon::TOOL,
        }
    }
}

/// 一个引用控件：`range` 是原文字节范围，`label` 是显示的短名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChip {
    pub range: Range<usize>,
    pub kind: RefKind,
    pub label: String,
}

/// 控件短名的最大显示宽度；超出时截断，完整内容仍在原文里。
const LABEL_MAX_CELLS: usize = 32;

impl RefChip {
    /// 显示形态：`␠图标␠名称␠`，空白用不换行空格，转写折行时控件不会从中间断开。
    pub fn display(&self) -> String {
        let label = crate::render::wrap::truncate(self.label.as_str(), LABEL_MAX_CELLS);
        format!("\u{a0}{}\u{a0}{label}\u{a0}", self.kind.icon())
    }
}

/// 把控件替换为显示串后的文本，以及「原文字节偏移 ↔ 显示字节偏移」的双向映射。
///
/// 渲染层只在显示文本上折行、画光标；输入框的光标、鼠标点击与选区都经这里换算回原文，
/// 落在控件内部的位置一律吸附到较近一侧的边界（控件是原子的）。
#[derive(Debug, Clone)]
pub struct RefMap {
    /// 显示文本（控件已替换为 `图标 名称`）。
    pub text: String,
    buffer_len: usize,
    /// 控件在显示文本里的字节段（升序不重叠），与 `buffer` 按下标一一对应。
    display: Vec<Range<usize>>,
    buffer: Vec<Range<usize>>,
}

impl RefMap {
    pub fn new(text: &str, chips: &[RefChip]) -> Self {
        let mut out = String::with_capacity(text.len());
        let mut display = Vec::with_capacity(chips.len());
        let mut buffer = Vec::with_capacity(chips.len());
        let mut at = 0;
        for chip in chips {
            out.push_str(&text[at..chip.range.start]);
            let start = out.len();
            out.push_str(&chip.display());
            display.push(start..out.len());
            buffer.push(chip.range.clone());
            at = chip.range.end;
        }
        out.push_str(&text[at..]);
        Self {
            text: out,
            buffer_len: text.len(),
            display,
            buffer,
        }
    }

    /// 控件显示段（升序不重叠）。
    pub fn display_spans(&self) -> &[Range<usize>] {
        &self.display
    }

    /// 原文字节偏移 → 显示字节偏移。
    pub fn to_display(&self, offset: usize) -> usize {
        let mut base = offset as isize;
        for (disp, buf) in self.display.iter().zip(&self.buffer) {
            if offset <= buf.start {
                break;
            }
            if offset < buf.end {
                return if offset - buf.start <= buf.end - offset {
                    disp.start
                } else {
                    disp.end
                };
            }
            base += disp.len() as isize - buf.len() as isize;
        }
        (base.max(0) as usize).min(self.text.len())
    }

    /// 显示字节偏移 → 原文字节偏移。
    pub fn to_buffer(&self, offset: usize) -> usize {
        let mut base = offset as isize;
        for (disp, buf) in self.display.iter().zip(&self.buffer) {
            if offset <= disp.start {
                break;
            }
            if offset < disp.end {
                return if offset - disp.start <= disp.end - offset {
                    buf.start
                } else {
                    buf.end
                };
            }
            base += buf.len() as isize - disp.len() as isize;
        }
        (base.max(0) as usize).min(self.buffer_len)
    }
}

/// 资源 token 的形状：`@路径`（`@@` 是字面转义）或具体资源 URI（不含 `skill://` 这类根）。
pub fn is_resource_token(token: &str) -> bool {
    if let Some(rest) = token.strip_prefix('@') {
        return !rest.is_empty() && !rest.starts_with('@');
    }
    [
        "artifact://",
        "history://",
        "local://",
        "skill://",
        "tool://",
    ]
    .iter()
    .any(|scheme| token.len() > scheme.len() && token.starts_with(scheme))
}

/// 空白分隔的资源 token 及其字节范围。
fn resource_tokens(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    text.split_whitespace()
        .filter(|token| is_resource_token(token))
        .map(move |token| {
            let start = token.as_ptr() as usize - text.as_ptr() as usize;
            (start..start + token.len(), token)
        })
}

/// `[image#N]` / `[paste#N]` 占位：返回 (范围, 类别, 序号)。
fn placeholders(text: &str) -> Vec<(Range<usize>, RefKind, u32)> {
    let mut out = Vec::new();
    for (marker, kind) in [("[image#", RefKind::Image), ("[paste#", RefKind::Paste)] {
        for (start, _) in text.match_indices(marker) {
            let rest = &text[start + marker.len()..];
            let Some(end) = rest.find(']') else { continue };
            if let Ok(id) = rest[..end].parse::<u32>() {
                out.push((start..start + marker.len() + end + 1, kind, id));
            }
        }
    }
    out
}

/// 输入框里的全部引用控件，按位置升序、互不重叠。
pub fn prompt_refs(app: &App) -> Vec<RefChip> {
    let text = app.prompt_text();
    if text.starts_with('!') {
        return Vec::new();
    }
    let prompt = &app.prompt;
    // 光标停在逐字键入的位置上时，覆盖它的 token 还在输入中，不成形。
    let typing = prompt.typed_at == Some(prompt.cursor);
    let live = |range: &Range<usize>| {
        !(typing && range.start <= prompt.cursor && prompt.cursor <= range.end)
    };
    let mut chips = Vec::new();
    let token = text.split_whitespace().next().unwrap_or_default();
    if token.starts_with('/') && app.commands.resolve(token).is_some() {
        let start = text.len() - text.trim_start().len();
        chips.push(RefChip {
            range: start..start + token.len(),
            kind: RefKind::Command,
            label: token.trim_start_matches('/').to_owned(),
        });
    }
    for (range, kind, id) in placeholders(text) {
        let label = match kind {
            RefKind::Image if prompt.images.iter().any(|chip| chip.id == id) => {
                format!("image {id}")
            }
            RefKind::Paste => match prompt.paste_lines(id) {
                Some(lines) => format!("paste {id} · {lines} lines"),
                None => continue,
            },
            _ => continue,
        };
        chips.push(RefChip { range, kind, label });
    }
    let session = app
        .session
        .as_ref()
        .map(|session| session.id.as_str())
        .unwrap_or_default();
    for (range, token) in resource_tokens(text) {
        if let Some(Some(info)) = app.refs.get(session, token) {
            chips.push(RefChip {
                range,
                kind: RefKind::from_inspect(&info.kind),
                label: info.label.clone(),
            });
        }
    }
    chips.retain(|chip| live(&chip.range));
    normalize(chips)
}

/// 转写里用户消息的附件标记（由 [`crate::transcript_restore::content_text`] 生成，必然有效）。
pub fn marker_refs(text: &str) -> Vec<RefChip> {
    let mut chips: Vec<RefChip> = placeholders(text)
        .into_iter()
        .filter(|(_, kind, _)| *kind == RefKind::Image)
        .map(|(range, kind, id)| RefChip {
            range,
            kind,
            label: format!("image {id}"),
        })
        .collect();
    for (marker, kind) in [
        ("[file ", RefKind::File),
        ("[dir ", RefKind::Dir),
        ("[#", RefKind::FileRef),
    ] {
        for (start, _) in text.match_indices(marker) {
            let rest = &text[start + marker.len()..];
            let Some(end) = rest.find(']') else { continue };
            let body = &rest[..end];
            // `[#name 路径]` 显示引用名，`[file 路径]` / `[dir 路径]` 显示末段文件名。
            let label = if marker == "[#" {
                let Some((name, _path)) = body.split_once(' ') else {
                    continue;
                };
                name.to_owned()
            } else {
                let trimmed = body.trim_end_matches(['/', '\\']);
                trimmed
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(trimmed)
                    .to_owned()
            };
            if label.is_empty() {
                continue;
            }
            chips.push(RefChip {
                range: start..start + marker.len() + end + 1,
                kind,
                label,
            });
        }
    }
    normalize(chips)
}

fn normalize(mut chips: Vec<RefChip>) -> Vec<RefChip> {
    chips.sort_by_key(|chip| (chip.range.start, std::cmp::Reverse(chip.range.end)));
    let mut out: Vec<RefChip> = Vec::with_capacity(chips.len());
    for chip in chips {
        if out
            .last()
            .is_none_or(|last| chip.range.start >= last.range.end)
        {
            out.push(chip);
        }
    }
    out
}

/// 同一 token 的校验结果保留时长；过期后重新查询，文件新建或删除后会跟上。
const REF_TTL: Duration = Duration::from_secs(30);

/// `resource/inspect` 的会话级缓存。
#[derive(Default)]
pub struct RefCache {
    session: String,
    entries: HashMap<String, (Option<ResourceRefInfo>, Instant)>,
    pending: HashSet<String>,
}

impl RefCache {
    /// 当前会话里 token 的校验结果；`None` 表示还没有结果。会话串不符时视为空，
    /// 防止切换会话后、下一轮 `take_missing` 清场前读到旧会话的结论。
    pub fn get(&self, session: &str, token: &str) -> Option<&Option<ResourceRefInfo>> {
        if self.session != session {
            return None;
        }
        self.entries.get(token).map(|(info, _)| info)
    }

    /// 取出需要查询的 token 并登记为在途；切换会话时先清空旧会话的结果。
    pub fn take_missing(&mut self, session: &str, text: &str) -> Vec<String> {
        if self.session != session {
            self.session = session.to_owned();
            self.entries.clear();
            self.pending.clear();
        }
        let now = Instant::now();
        let mut missing = Vec::new();
        for (_, token) in resource_tokens(text) {
            let fresh = self
                .entries
                .get(token)
                .is_some_and(|(_, at)| now.duration_since(*at) < REF_TTL);
            if !fresh && !self.pending.contains(token) {
                self.pending.insert(token.to_owned());
                missing.push(token.to_owned());
            }
        }
        missing
    }

    /// 写入查询结果；会话已切换时丢弃。
    pub fn store(
        &mut self,
        session: &str,
        tokens: Vec<String>,
        infos: Vec<Option<ResourceRefInfo>>,
    ) {
        if self.session != session {
            return;
        }
        let now = Instant::now();
        let mut infos = infos.into_iter();
        for token in tokens {
            self.pending.remove(&token);
            self.entries.insert(token, (infos.next().flatten(), now));
        }
    }
}

/// 发出缺失 token 的 `resource/inspect` 查询；没有会话时不查（引用校验需要会话）。
pub fn request_inspection(app: &mut App, connection: &crate::protocol::EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let tokens = app.refs.take_missing(&session_id, &app.prompt.text);
    if tokens.is_empty() {
        return;
    }
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let infos = client
            .call::<moc_protocol::ResourceInspect>(moc_protocol::ResourceInspectParams {
                session_id: session_id.clone(),
                refs: tokens.clone(),
            })
            .await
            .map(|result| result.refs)
            .unwrap_or_default();
        let _ = events.send(crate::protocol::AppEvent::RefInspection {
            session_id,
            tokens,
            infos,
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_become_chips_with_short_labels() {
        let text = "看 @a.rs\n[file /w/src/a.rs] 与 [dir /w/pkg/] [#report.pdf /u/report.pdf] [image#2] [x]";
        let chips = marker_refs(text);
        let labels: Vec<_> = chips
            .iter()
            .map(|chip| (chip.kind, chip.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                (RefKind::File, "a.rs"),
                (RefKind::Dir, "pkg"),
                (RefKind::FileRef, "report.pdf"),
                (RefKind::Image, "image 2"),
            ]
        );
        assert_eq!(&text[chips[0].range.clone()], "[file /w/src/a.rs]");
    }

    #[test]
    fn ref_map_maps_boundaries_and_snaps_inside() {
        let text = "ab [file /w/src/a.rs] cd";
        let chips = marker_refs(text);
        assert_eq!(chips.len(), 1);
        let map = RefMap::new(text, &chips);
        let (disp, buf) = (&map.display[0], &map.buffer[0]);
        assert_eq!(buf.clone(), 3..21);
        assert_eq!(&map.text[disp.clone()], "\u{a0}▤\u{a0}a.rs\u{a0}");
        // 边界一一对应；段内吸附到较近一侧。
        assert_eq!(map.to_display(buf.start), disp.start);
        assert_eq!(map.to_display(buf.end), disp.end);
        assert_eq!(map.to_buffer(disp.start), buf.start);
        assert_eq!(map.to_buffer(disp.end), buf.end);
        assert_eq!(map.to_display(buf.start + 1), disp.start);
        assert_eq!(map.to_display(buf.end - 1), disp.end);
        assert_eq!(map.to_buffer(disp.start + 1), buf.start);
        assert_eq!(map.to_buffer(disp.end - 1), buf.end);
        // 段外偏移双向往返。
        for offset in [0usize, 2, text.len() - 1, text.len()] {
            assert_eq!(map.to_buffer(map.to_display(offset)), offset, "{offset}");
        }
    }

    #[test]
    fn resource_token_shapes() {
        for (token, ok) in [
            ("@a.rs", true),
            ("@@a", false),
            ("@", false),
            ("skill://review", true),
            ("skill://", false),
            ("http://x", false),
        ] {
            assert_eq!(is_resource_token(token), ok, "{token}");
        }
    }
}
