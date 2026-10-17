//! 摘要 checkpoint 的校验：标签剥离、段落完整性与关键锚点覆盖。
//!
//! 压缩的失败模式不是请求报错，而是模型返回一句闲聊：旧实现照单全收，上下文
//! 从此归零。这里把"可用的 checkpoint"变成可判定的条件，并给出重试所需的
//! 具体缺口（缺哪些段落、漏了哪些标识符/路径/数字），让重试有方向。

use std::collections::HashSet;

use moc_llm::{ContentPart, Message, Role};

/// 合格的 checkpoint 至少要有这么多字符。
pub(crate) const MIN_CHECKPOINT_CHARS: usize = 600;
/// 关键锚点的目标覆盖率；低于它触发一次带缺口清单的重试。
pub(crate) const MIN_ANCHOR_COVERAGE: f64 = 0.85;
/// 重试全部失败时，仍可接受的锚点覆盖率下限；再低就改用确定性兜底。
pub(crate) const ACCEPTABLE_ANCHOR_COVERAGE: f64 = 0.75;
/// 锚点总量上限：超长会话里只校验最先出现的这些，避免校验本身变成负担。
const MAX_ANCHORS: usize = 240;
/// 一次修复提示里最多列出的缺失锚点。
pub(crate) const MAX_REPORTED_MISSING: usize = 24;

/// checkpoint 校验结果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    Ok,
    /// 文本过短（字符数）。
    Short(usize),
    /// 缺少的必需段落标题。
    Sections(Vec<String>),
    /// 锚点覆盖不足：覆盖率与缺失清单。
    Coverage(f64, Vec<String>),
}

impl Verdict {
    /// 重试提示词里的拒收原因。
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Ok => "the checkpoint is acceptable".to_owned(),
            Self::Short(chars) => format!(
                "your reply was only {chars} characters long; a usable checkpoint must be a full structured summary, not a chat reply"
            ),
            Self::Sections(missing) => format!(
                "your reply is missing these required sections: {}",
                missing.join(", ")
            ),
            Self::Coverage(coverage, _) => format!(
                "your reply omits too many concrete facts (anchor coverage {:.0}%)",
                coverage * 100.0
            ),
        }
    }

    /// 结构性不合格（再重试也未必能救）时返回 `true`。
    pub(crate) fn is_structural(&self) -> bool {
        matches!(self, Self::Short(_) | Self::Sections(_))
    }
}

/// 剥掉模型常加的外壳：开头 `<analysis>` 草稿、`<summary>` 标签与代码围栏。
pub(crate) fn unwrap_checkpoint(raw: &str) -> String {
    let mut text = raw.trim().to_owned();
    loop {
        // `to_ascii_lowercase` 只改 ASCII 字节，下标与原文一一对应。
        let lower = text.to_ascii_lowercase();
        let Some(start) = lower.find("<analysis") else {
            break;
        };
        // 只有位于第一个 `<summary>` 之前的 `<analysis>` 才算草稿。
        let summary_at = lower.find("<summary").unwrap_or(usize::MAX);
        if start > summary_at {
            break;
        }
        match lower[start..].find("</analysis>") {
            Some(close) => {
                let end = start + close + "</analysis>".len();
                text.replace_range(start..end, "");
                text = text.trim().to_owned();
            }
            None => {
                // 未闭合：砍到 `<summary>`，否则整段作废。
                text = if summary_at != usize::MAX && summary_at > start {
                    text[summary_at..].to_owned()
                } else {
                    String::new()
                };
                break;
            }
        }
    }
    let lower = text.to_ascii_lowercase();
    if let Some(open) = lower.rfind("<summary") {
        let inner_start = text[open..]
            .find('>')
            .map(|offset| open + offset + 1)
            .unwrap_or(open + "<summary".len());
        let tail = &text[inner_start..];
        let inner_end = tail
            .to_ascii_lowercase()
            .rfind("</summary>")
            .unwrap_or(tail.len());
        text = tail[..inner_end].trim().to_owned();
    }
    let mut text = strip_fence(&text).trim().to_owned();
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    text
}

/// 去掉整体包裹的 Markdown 代码围栏。
fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if !trimmed.starts_with("```") {
        return trimmed;
    }
    let body = trimmed.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

/// 必需段落里缺失的标题；按行首 `#` 判定，避免正文里提到段落名就算数。
pub(crate) fn missing_sections(text: &str, sections: &[&str]) -> Vec<String> {
    let headings: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .map(|line| {
            line.trim_start_matches('#')
                .trim()
                .trim_end_matches(':')
                .trim()
                .to_ascii_lowercase()
        })
        .collect();
    sections
        .iter()
        .filter(|title| {
            let needle = title.to_ascii_lowercase();
            !headings.iter().any(|heading| heading == &needle)
        })
        .map(|title| (*title).to_owned())
        .collect()
}

/// 从会话正文里抽取"压缩后必须逐字保留"的锚点：路径、标识符、数字。
///
/// 只扫描用户/助手文本与工具调用参数：工具结果里是整份文件内容，把它算进
/// 覆盖率会让任何摘要都不合格。`previous` 是上一版 checkpoint，它的锚点排在最前，
/// 保证多次压缩时优先保住已经沉淀下来的事实。
pub(crate) fn salient_anchors<'a>(
    messages: impl Iterator<Item = &'a Message>,
    previous: Option<&str>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut anchors = Vec::new();
    if let Some(previous) = previous {
        scan(previous, &mut anchors, &mut seen);
    }
    for message in messages {
        if !matches!(message.role, Role::User | Role::Assistant)
            || super::replay::is_injected_context(message)
        {
            continue;
        }
        for part in &message.content {
            match part {
                ContentPart::Text(text) => scan(text, &mut anchors, &mut seen),
                ContentPart::ToolCall(call) => scan(&call.arguments, &mut anchors, &mut seen),
                _ => {}
            }
        }
        if anchors.len() >= MAX_ANCHORS {
            break;
        }
    }
    anchors
}

/// 锚点在文本里的出现比例。
pub(crate) fn coverage(anchors: &[String], text: &str) -> f64 {
    if anchors.is_empty() {
        return 1.0;
    }
    let hits = anchors
        .iter()
        .filter(|anchor| text.contains(*anchor))
        .count();
    hits as f64 / anchors.len() as f64
}

/// 文本里缺失的锚点，最多 `MAX_REPORTED_MISSING` 个。
pub(crate) fn missing_anchors(anchors: &[String], text: &str) -> Vec<String> {
    anchors
        .iter()
        .filter(|anchor| !text.contains(*anchor))
        .take(MAX_REPORTED_MISSING)
        .cloned()
        .collect()
}

/// 校验一个 checkpoint 是否可用。
pub(crate) fn evaluate(text: &str, anchors: &[String], sections: &[&str]) -> Verdict {
    let chars = text.chars().count();
    if chars < MIN_CHECKPOINT_CHARS {
        return Verdict::Short(chars);
    }
    let missing = missing_sections(text, sections);
    if !missing.is_empty() {
        return Verdict::Sections(missing);
    }
    let coverage = coverage(anchors, text);
    if coverage < MIN_ANCHOR_COVERAGE {
        return Verdict::Coverage(coverage, missing_anchors(anchors, text));
    }
    Verdict::Ok
}

/// 标记 token 的边界字符：只在 ASCII 字母数字与少量标识符字符内连续。
fn is_token_byte(byte: u8) -> bool {
    matches!(byte,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
        | b'_' | b'-' | b'.' | b'/' | b':' | b'~' | b'@' | b'+' | b'%')
}

/// 扫描文本并收集锚点；按出现顺序去重。
fn scan(text: &str, anchors: &mut Vec<String>, seen: &mut HashSet<String>) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if !is_token_byte(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_token_byte(bytes[index]) {
            index += 1;
        }
        let token = text[start..index].trim_matches(['.', ',', ';', ':', '+', '%']);
        if !classify(token) {
            continue;
        }
        if anchors.len() >= MAX_ANCHORS {
            return;
        }
        if seen.insert(token.to_owned()) {
            anchors.push(token.to_owned());
        }
    }
}

/// 判断一个 token 是否值得作为必须保留的锚点。
fn classify(token: &str) -> bool {
    if token.len() < 3 {
        return false;
    }
    let digits = token.bytes().filter(u8::is_ascii_digit).count();
    if token
        .bytes()
        .all(|b| b.is_ascii_digit() || b == b'_' || b == b'.' || b == b',')
    {
        // 数字：至少三位，或像 3.2 这样的小数。
        return digits >= 3
            || (token.contains('.')
                && digits >= 2
                && token
                    .split_once('.')
                    .is_some_and(|(left, right)| !left.is_empty() && !right.is_empty()));
    }
    // 只收相对路径：绝对路径（含临时目录）在换机、换目录后没有复述价值。
    let starts_with_name = token
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
    let path_like = starts_with_name
        && token.contains('/')
        && (token.contains('.') || token.matches('/').count() >= 2 || token.contains("://"));
    if path_like {
        return true;
    }
    let has_extension = token.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && matches!(
                extension,
                "rs" | "md"
                    | "toml"
                    | "json"
                    | "lock"
                    | "sh"
                    | "yml"
                    | "yaml"
                    | "py"
                    | "ts"
                    | "js"
                    | "go"
                    | "c"
                    | "h"
                    | "txt"
            )
    });
    if has_extension && token.len() >= 6 {
        return true;
    }
    // 标识符：下划线或内部大写才算"具体名字"，普通英文单词一律忽略。
    let identifier = token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && token
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
    if !identifier || token.len() < 4 {
        return false;
    }
    let internal_upper = token
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase());
    token.contains('_') || internal_upper
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTIONS: &[&str] = &["Primary Request and Intent", "Current Work", "Next Step"];

    fn sections_text() -> String {
        let mut text = String::from("## Primary Request and Intent\n");
        text.push_str(&"x".repeat(600));
        text.push_str("\n## Current Work\nwork\n## Next Step\nstep\n");
        text
    }

    #[test]
    fn unwrap_strips_analysis_and_summary_tags() {
        let raw = "<analysis>draft</analysis>\n<summary>\n## A\nbody\n</summary>";
        assert_eq!(unwrap_checkpoint(raw), "## A\nbody");
    }

    #[test]
    fn unwrap_keeps_plain_text() {
        assert_eq!(unwrap_checkpoint("  ## A\nbody  "), "## A\nbody");
    }

    #[test]
    fn unwrap_strips_code_fence() {
        let raw = "```markdown\n## A\nbody\n```";
        assert_eq!(unwrap_checkpoint(raw), "## A\nbody");
    }

    #[test]
    fn unwrap_handles_unclosed_analysis() {
        assert_eq!(unwrap_checkpoint("<analysis>only draft"), "");
        assert_eq!(
            unwrap_checkpoint("<analysis>draft<summary>## A\nkeep</summary>"),
            "## A\nkeep"
        );
    }

    #[test]
    fn missing_sections_requires_real_headings() {
        let text = "## Primary Request and Intent\nmentions Current Work in prose\n";
        assert_eq!(
            missing_sections(text, SECTIONS),
            vec!["Current Work".to_owned(), "Next Step".to_owned()]
        );
    }

    #[test]
    fn anchors_pick_paths_names_and_numbers() {
        let message = Message::user_text(
            "把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000，p99 从 8.7 掉到 3.2；\
             行锚点 110jy 与普通单词 window 都不算。",
        );
        let anchors = salient_anchors(std::iter::once(&message), None);
        assert!(anchors.contains(&"crates/ingest/src/window.rs".to_owned()));
        assert!(anchors.contains(&"WINDOW_TTL_MS".to_owned()));
        assert!(anchors.contains(&"45_000".to_owned()));
        assert!(anchors.contains(&"3.2".to_owned()));
        assert!(!anchors.iter().any(|anchor| anchor == "window"));
        assert!(!anchors.iter().any(|anchor| anchor == "110jy"));
    }

    #[test]
    fn anchors_prioritize_previous_checkpoint() {
        let message = Message::user_text("rotate_windows");
        let anchors = salient_anchors(std::iter::once(&message), Some("legacy.rs"));
        assert_eq!(anchors.first().map(String::as_str), Some("legacy.rs"));
    }

    #[test]
    fn evaluate_flags_short_sections_and_coverage() {
        let anchors = vec!["45_000".to_owned(), "rotate_windows".to_owned()];
        assert!(matches!(
            evaluate("好，已停在这里", &anchors, SECTIONS),
            Verdict::Short(_)
        ));
        let sections = sections_text();
        assert!(matches!(
            evaluate(&sections, &anchors, SECTIONS),
            Verdict::Coverage(_, _)
        ));
        let good = format!("{sections}\n45_000 rotate_windows");
        assert_eq!(evaluate(&good, &anchors, SECTIONS), Verdict::Ok);
    }

    #[test]
    fn coverage_reports_missing_anchors() {
        let anchors = vec!["a_b".to_owned(), "c_d".to_owned()];
        assert_eq!(coverage(&anchors, "a_b here"), 0.5);
        assert_eq!(
            missing_anchors(&anchors, "a_b here"),
            vec!["c_d".to_owned()]
        );
    }
}
