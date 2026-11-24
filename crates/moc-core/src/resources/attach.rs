//! `@` 附件引用：工作区文件/目录的输入补全与注入块标记。
//!
//! 输入框里 `@` 开头的 token 走这条补全：按会话 cwd 用 `ignore` 遍历
//! （尊重 .gitignore、跳过隐藏项、不跟随符号链接），对相对路径做 nucleo
//! 模糊匹配。目录候选以 `/` 结尾且不补空格，选中后可以继续往下补。
//!
//! 发送时 core 把被引用内容注入用户消息，正文段包在 `<moc-attachment …>`
//! 外壳里（见 [`wrap_attachment`]）：转写把这段还原成附件 chip，模型读到的
//! 是 read 格式的注入内容。

use std::path::Path;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

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
    if !is_attachment(text) {
        return None;
    }
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

/// `@` 前缀补全：`prefix` 含开头的 `@`，返回值之外的部分是查询串；`kind` 为 `file` / `dir` 时只返回该类。
///
/// 打分用 nucleo（fzf 同款）按整条相对路径匹配；basename 也命中时叠加 basename 的得分，
/// 让 `@rs` 先给出 `fs.rs` 这类文件名命中，而不是靠目录名零散命中的 `crates`。
pub(super) fn file_candidates(
    context: &ResourceContext<'_>,
    prefix: &str,
    kind: Option<&str>,
) -> Vec<ResourceCandidate> {
    let query = &prefix[1..];
    // `@@` 是字面 `@` 的转义，不补全。
    if query.starts_with('@') {
        return Vec::new();
    }
    let entries = workspace_entries(context.cwd)
        .into_iter()
        .filter(|(_, is_dir)| kind.is_none_or(|kind| (kind == "dir") == *is_dir));
    if query.is_empty() {
        // 无查询：目录在前、浅层优先，便于逐级往下补。
        let mut entries: Vec<_> = entries.collect();
        entries.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| left.0.len().cmp(&right.0.len()))
                .then_with(|| left.0.cmp(&right.0))
        });
        entries.truncate(MATCH_LIMIT);
        return entries
            .into_iter()
            .map(|(path, is_dir)| candidate(path, is_dir, Vec::new()))
            .collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let atom = Atom::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut buffer = Vec::new();
    // (得分, 路径, 是否目录, basename 起始字节, basename 是否命中)
    let mut scored: Vec<(u32, String, bool, usize, bool)> = Vec::new();
    for (path, is_dir) in entries {
        let Some(path_score) = atom.score(Utf32Str::new(&path, &mut buffer), &mut matcher) else {
            continue;
        };
        let base = path
            .rfind(std::path::is_separator)
            .map_or(0, |slash| slash + 1);
        let base_score = atom.score(Utf32Str::new(&path[base..], &mut buffer), &mut matcher);
        let score = u32::from(path_score) + u32::from(base_score.unwrap_or(0));
        scored.push((score, path, is_dir, base, base_score.is_some()));
    }
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.len().cmp(&right.1.len()))
            .then_with(|| left.1.cmp(&right.1))
    });
    scored.truncate(MATCH_LIMIT);
    scored
        .into_iter()
        .map(|(_, path, is_dir, base, base_hit)| {
            // 高亮优先落在 basename 上；下标按字符计，basename 命中时整体平移到 basename 起点。
            let mut matches = Vec::new();
            let (haystack, offset) = if base_hit {
                (&path[base..], path[..base].chars().count() as u32)
            } else {
                (path.as_str(), 0)
            };
            atom.indices(
                Utf32Str::new(haystack, &mut buffer),
                &mut matcher,
                &mut matches,
            );
            matches.sort_unstable();
            matches.dedup();
            matches.iter_mut().for_each(|index| *index += offset);
            candidate(path, is_dir, matches)
        })
        .collect()
}

fn candidate(path: String, is_dir: bool, matches: Vec<u32>) -> ResourceCandidate {
    ResourceCandidate {
        uri: if is_dir {
            format!("@{path}/")
        } else {
            format!("@{path}")
        },
        label: path,
        // 路径本身就是全部信息，不再附「workspace file」这类说明。
        description: String::new(),
        kind: if is_dir { "dir" } else { "file" }.into(),
        // 目录选中后继续补下一级；文件补空格收尾。
        append_space: !is_dir,
        matches,
    }
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
