//! `@` 附件引用：工作区文件/目录的输入补全与注入块标记。
//!
//! 输入框里 `@` 开头的 token 走这条补全：按会话 cwd 用 `ignore` 遍历
//! （尊重 .gitignore、跳过隐藏项、不跟随符号链接），对相对路径做子序列
//! 模糊匹配。目录候选以 `/` 结尾且不补空格，选中后可以继续往下补。
//!
//! 发送时 core 把被引用内容注入用户消息，正文段包在 `<moc-attachment …>`
//! 外壳里（见 [`wrap_attachment`]）：转写把这段还原成附件 chip，模型读到的
//! 是 read 格式的注入内容。

use std::path::Path;

use super::{ResourceCandidate, ResourceContext};

/// 注入块的外壳标记。
pub const ATTACHMENT_OPEN: &str = "<moc-attachment";
pub const ATTACHMENT_CLOSE: &str = "</moc-attachment>";

/// 生成一个注入块：`path` 用解析后的绝对路径，`kind` 是 `file`/`dir`。
pub fn wrap_attachment(path: &str, kind: &str, body: &str) -> String {
    let escaped = path.replace('"', "");
    format!("{ATTACHMENT_OPEN} path=\"{escaped}\" kind=\"{kind}\">\n{body}\n{ATTACHMENT_CLOSE}")
}

/// 剥掉注入块外壳；返回 `(path, kind, body)`，非注入块返回 `None`。
pub fn parse_attachment(text: &str) -> Option<(&str, &str, &str)> {
    let text = text.strip_prefix(ATTACHMENT_OPEN)?;
    let (header, rest) = text.split_once(">\n")?;
    let body = rest.strip_suffix(&format!("\n{ATTACHMENT_CLOSE}"))?;
    let path = attr(header, "path")?;
    let kind = attr(header, "kind")?;
    Some((path, kind, body))
}

/// 是不是注入块（不要求内容完整可解析）。
pub fn is_attachment(text: &str) -> bool {
    text.starts_with(ATTACHMENT_OPEN) && text.ends_with(ATTACHMENT_CLOSE)
}

/// 分页投影用的占位 stub：保留头尾外壳让附件仍可被解析成 chip，
/// 正文压成一行省略号，避免注入内容挤占整个消息的字节预算。
pub fn stub_attachment(text: &str) -> Option<String> {
    let header_end = text.find(">\n")?;
    let header = &text[..header_end + 1];
    Some(format!("{header}\n…\n{ATTACHMENT_CLOSE}"))
}

fn attr<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let start = header.find(&key)? + key.len();
    let end = header[start..].find('"')? + start;
    Some(&header[start..end])
}

/// 遍历收集的条目数上限；超出即停，保证补全始终快速返回。
const WALK_ENTRY_LIMIT: usize = 40_000;
/// `@` 前缀返回的候选条数上限。
const MATCH_LIMIT: usize = 20;

/// `@` 前缀补全：`prefix` 含开头的 `@`，返回值之外的部分是查询串。
pub(super) fn file_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
) -> Vec<ResourceCandidate> {
    let query = &prefix[1..];
    // `@@` 是字面 `@` 的转义，不补全。
    if query.starts_with('@') {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let mut scored: Vec<(i64, bool, String)> = Vec::new();
    for entry in workspace_entries(context.cwd) {
        let (path, is_dir) = entry;
        let haystack = path.to_lowercase();
        let Some(score) = fuzzy_score(&query, &haystack, is_dir) else {
            continue;
        };
        scored.push((score, is_dir, path));
    }
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.2.len().cmp(&right.2.len()))
            .then_with(|| left.2.cmp(&right.2))
    });
    scored.truncate(MATCH_LIMIT);
    scored
        .into_iter()
        .map(|(_, is_dir, path)| {
            let uri = if is_dir {
                format!("@{path}/")
            } else {
                format!("@{path}")
            };
            ResourceCandidate {
                label: path,
                uri,
                description: if is_dir {
                    "workspace directory".into()
                } else {
                    "workspace file".into()
                },
                kind: if is_dir { "dir" } else { "file" }.into(),
                // 目录选中后继续补下一级；文件补空格收尾。
                append_space: !is_dir,
            }
        })
        .collect()
}

/// 收集 cwd 下的相对路径；`(相对路径, 是否目录)`。
fn workspace_entries(cwd: &Path) -> Vec<(String, bool)> {
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(cwd)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let Some(name) = entry.path().strip_prefix(cwd).ok().and_then(|path| {
            let text = path.to_string_lossy();
            (!text.is_empty()).then(|| text.into_owned())
        }) else {
            continue;
        };
        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        entries.push((name, is_dir));
        if entries.len() >= WALK_ENTRY_LIMIT {
            break;
        }
    }
    entries
}

/// 子序列模糊匹配；`query` 与 `path` 均已小写。目录获得轻微加权，让 `@src`
/// 先命中目录本身而不是目录里更深的路径。
fn fuzzy_score(query: &str, path: &str, is_dir: bool) -> Option<i64> {
    if query.is_empty() {
        return Some(if is_dir { 2 } else { 1 });
    }
    let mut score = 0i64;
    let mut query_chars = query.chars().peekable();
    let mut previous_matched = false;
    let mut first_match = None;
    for (index, ch) in path.chars().enumerate() {
        let Some(&want) = query_chars.peek() else {
            break;
        };
        if ch != want {
            previous_matched = false;
            continue;
        }
        query_chars.next();
        if first_match.is_none() {
            first_match = Some(index);
        }
        score += 1;
        if previous_matched {
            score += 2;
        }
        // 段首（跟在 `/` 后）命中加权，倾向 basename 命中。
        if index == 0 || path.as_bytes().get(index.wrapping_sub(1)) == Some(&b'/') {
            score += 3;
        }
        previous_matched = true;
    }
    if query_chars.peek().is_some() {
        return None;
    }
    // 整体越靠后命中越差；候选路径越长越差。
    let score = score * 8 - first_match.unwrap_or(0) as i64 - path.len() as i64 / 8;
    Some(if is_dir { score + 4 } else { score })
}
