//! 摘要 checkpoint 的校验：剥掉外层标签，检查段落完整性、必保留短语和关键锚点是否齐全。
//!
//! 压缩的失败方式不是请求报错，而是模型只回一句闲聊。这里把「能用的 checkpoint」写成
//! 可以判定的条件，并给出重试需要的具体缺口：缺了哪些段落、漏了哪些标识符、路径或数字、
//! 丢了哪条决策或步号，让重试有方向。

use std::collections::HashSet;

use moc_llm::{ContentPart, Message, Role};

/// 合格的 checkpoint 至少要有这么多字符。
pub(crate) const MIN_CHECKPOINT_CHARS: usize = 600;
/// 锚点总数上限：超长会话按等距抽样保留这些，避免校验本身成为负担。
const MAX_ANCHORS: usize = 240;
/// 达标需要的锚点命中数下限与上限。
///
/// 小输入要求 85%；大输入封顶到固定条数：几十万 token 的历史压成几千 token 的
/// checkpoint，不可能覆盖 85% 的锚点，硬要求会让重试和兜底误伤正常结果。
const MIN_REQUIRED_ANCHOR_HITS: usize = 8;
const MAX_REQUIRED_ANCHOR_HITS: usize = 64;
/// 一次修复提示里最多列出多少个缺失锚点。
pub(crate) const MAX_REPORTED_MISSING: usize = 24;

/// checkpoint 的校验结果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    Ok,
    /// 文本太短（字符数）。
    Short(usize),
    /// 缺少的必需段落标题。
    Sections(Vec<String>),
    /// 缺少的必须逐字出现的事实短语（被否决的方案名、计划步号等）。
    Required(Vec<String>),
    /// 锚点命中不足：命中数、达标线和缺失清单。
    Coverage {
        hits: usize,
        required: usize,
        missing: Vec<String>,
    },
}

impl Verdict {
    /// 重试提示词里写的拒收原因。
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
            Self::Required(missing) => format!(
                "your reply dropped these required facts: {}",
                missing.join(", ")
            ),
            Self::Coverage { hits, required, .. } => format!(
                "your reply preserves only {hits} of the {required} required concrete facts (paths, identifiers, numbers)"
            ),
        }
    }

    /// 结构性不合格（重试也未必能救）时返回 `true`。
    pub(crate) fn is_structural(&self) -> bool {
        matches!(self, Self::Short(_) | Self::Sections(_))
    }
}

/// 达标所需的锚点命中数。
pub(crate) fn required_hits(total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let scaled = total.saturating_mul(85).div_ceil(100);
    scaled.clamp(
        MIN_REQUIRED_ANCHOR_HITS.min(total),
        MAX_REQUIRED_ANCHOR_HITS.min(total),
    )
}

/// 重试全部失败时仍值得保留的命中数：达标线的四分之三。
pub(crate) fn acceptable_hits(total: usize) -> usize {
    let required = required_hits(total);
    required.saturating_sub(required / 4).max(1)
}

/// 必保留短语里缺失的那些；忽略大小写，`第 3 步` 的常见写法都算命中。
pub(crate) fn missing_required(required: &[String], text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    required
        .iter()
        .filter(|phrase| !phrase_present(&lower, phrase))
        .take(MAX_REPORTED_MISSING)
        .cloned()
        .collect()
}

/// 短语是否出现在（已转小写的）文本里。
///
/// 模型会把计划步号写成 `第 3 步`、`第3步` 或 `第三步`，逐字比较会误判，所以这里
/// 把步号展开成几种等价写法；其它短语按忽略大小写的子串匹配。
fn phrase_present(lower_text: &str, phrase: &str) -> bool {
    let phrase_lower = phrase.to_lowercase();
    let Some(number) = step_number(&phrase_lower) else {
        return lower_text.contains(&phrase_lower);
    };
    let step_hit = |digits: &str| {
        lower_text.contains(&format!("第 {digits} 步"))
            || lower_text.contains(&format!("第{digits}步"))
    };
    if step_hit(&number.to_string()) {
        return true;
    }
    // 步号从 1 开始，中文写法数组从 0 开始。
    number
        .checked_sub(1)
        .and_then(|index| CHINESE_NUMERALS.get(index))
        .is_some_and(|numerals| step_hit(numerals))
}

/// 取出 `第 3 步` 这类短语里的步号。
fn step_number(phrase: &str) -> Option<usize> {
    let rest = phrase
        .strip_prefix("第 ")
        .or_else(|| phrase.strip_prefix("第"))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    rest[digits.len()..].trim_start().strip_prefix('步')?;
    digits.parse().ok()
}

/// 步号的中文写法，用于 `第三步` 和 `第 3 步` 的等价匹配。
const CHINESE_NUMERALS: [&str; 9] = ["一", "二", "三", "四", "五", "六", "七", "八", "九"];

/// 剥掉模型常加的外壳：开头的 `<analysis>` 草稿、`<summary>` 标签和代码围栏。
pub(crate) fn unwrap_checkpoint(raw: &str) -> String {
    let mut text = raw.trim().to_owned();
    loop {
        // `to_ascii_lowercase` 只改 ASCII 字节，下标和原文一一对应。
        let lower = text.to_ascii_lowercase();
        let Some(start) = lower.find("<analysis") else {
            break;
        };
        // 只有出现在第一个 `<summary>` 之前的 `<analysis>` 才算草稿。
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
                // 没闭合：从 `<summary>` 开始保留，否则整段作废。
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

/// 必需段落里缺了哪些标题；只认行首的 `#`，正文里提到段落名不算。
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

/// 从会话正文里抽取「压缩后必须逐字保留」的锚点：路径、标识符、数字。
///
/// 只扫描用户和助手的文本以及工具调用参数：工具结果里是整份文件内容，算进覆盖率
/// 会让任何摘要都不合格。`previous` 是上一版 checkpoint，它的锚点排在最前，保证多次
/// 压缩时优先保住已经沉淀下来的事实。
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
    }
    sample_even(&mut anchors);
    anchors
}

/// 锚点过多时按等距抽样：超长会话里开头、中段和结尾都要有代表，
/// 只留最先出现的 240 个会让校验偏向会话早期。
fn sample_even(anchors: &mut Vec<String>) {
    let len = anchors.len();
    if len <= MAX_ANCHORS {
        return;
    }
    let mut sampled = Vec::with_capacity(MAX_ANCHORS);
    for index in 0..MAX_ANCHORS {
        sampled.push(anchors[index * len / MAX_ANCHORS].clone());
    }
    *anchors = sampled;
}

/// 文本命中了多少个锚点。
pub(crate) fn matching(anchors: &[String], text: &str) -> usize {
    anchors
        .iter()
        .filter(|anchor| text.contains(*anchor))
        .count()
}

/// 文本里缺了哪些锚点，最多 `MAX_REPORTED_MISSING` 个。
pub(crate) fn missing_anchors(anchors: &[String], text: &str) -> Vec<String> {
    anchors
        .iter()
        .filter(|anchor| !text.contains(*anchor))
        .take(MAX_REPORTED_MISSING)
        .cloned()
        .collect()
}

/// 校验一个 checkpoint 是否可用。
pub(crate) fn evaluate(
    text: &str,
    anchors: &[String],
    required: &[String],
    sections: &[&str],
) -> Verdict {
    let chars = text.chars().count();
    if chars < MIN_CHECKPOINT_CHARS {
        return Verdict::Short(chars);
    }
    let missing = missing_sections(text, sections);
    if !missing.is_empty() {
        return Verdict::Sections(missing);
    }
    // 必保留短语先于覆盖率检查：被否决的方案名和计划步号决定「压缩后还能不能接着做」，
    // 锚点覆盖只是防止整体丢失细节。
    let missing = missing_required(required, text);
    if !missing.is_empty() {
        return Verdict::Required(missing);
    }
    let hits = matching(anchors, text);
    let needed = required_hits(anchors.len());
    if hits < needed {
        return Verdict::Coverage {
            hits,
            required: needed,
            missing: missing_anchors(anchors, text),
        };
    }
    Verdict::Ok
}

/// 标记词边界的字节：词只在 ASCII 字母数字和少量标识符字符内连续。
fn is_token_byte(byte: u8) -> bool {
    matches!(byte,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
        | b'_' | b'-' | b'.' | b'/' | b':' | b'~' | b'@' | b'+' | b'%')
}

/// 扫描文本收集锚点，按出现顺序去重。
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
        if seen.insert(token.to_owned()) {
            anchors.push(token.to_owned());
        }
    }
}

/// 判断一个词是否值得作为必须保留的锚点。
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
    // 只收相对路径：绝对路径（含临时目录）换机器、换目录后没有复述价值。
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
    // 标识符：要有下划线或内部大写才算「具体名字」，普通英文单词一律忽略。
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
            evaluate("好，已停在这里", &anchors, &[], SECTIONS),
            Verdict::Short(_)
        ));
        let sections = sections_text();
        assert!(matches!(
            evaluate(&sections, &anchors, &[], SECTIONS),
            Verdict::Coverage { .. }
        ));
        let good = format!("{sections}\n45_000 rotate_windows");
        assert_eq!(evaluate(&good, &anchors, &[], SECTIONS), Verdict::Ok);
    }

    #[test]
    fn required_phrases_survive_spelling_variants() {
        let sections = sections_text();
        let required = vec!["Redis".to_owned(), "第 3 步".to_owned()];
        // 大小写和步号写法都放宽：redis 与 第三步 都算命中。
        let good = format!("{sections}\n否决 redis；当前在第三步。");
        assert_eq!(evaluate(&good, &[], &required, SECTIONS), Verdict::Ok);

        let missing = format!("{sections}\n否决了跨进程方案；当前在第二步。");
        assert_eq!(
            evaluate(&missing, &[], &required, SECTIONS),
            Verdict::Required(vec!["Redis".to_owned(), "第 3 步".to_owned()])
        );
    }

    #[test]
    fn step_phrases_parse_and_match_numerals() {
        assert_eq!(step_number("第 3 步"), Some(3));
        assert_eq!(step_number("第3步"), Some(3));
        assert_eq!(step_number("第三步"), None, "中文写法不会作为短语输入");
        assert!(phrase_present("当前在第三步", "第 3 步"));
        assert!(phrase_present("第5步已完成", "第 5 步"));
        assert!(!phrase_present("当前在第二步", "第 3 步"));
    }

    #[test]
    fn matching_counts_anchors_and_reports_missing() {
        let anchors = vec!["a_b".to_owned(), "c_d".to_owned()];
        assert_eq!(matching(&anchors, "a_b here"), 1);
        assert_eq!(
            missing_anchors(&anchors, "a_b here"),
            vec!["c_d".to_owned()]
        );
    }

    #[test]
    fn required_hits_scale_with_a_cap() {
        assert_eq!(required_hits(0), 0);
        assert_eq!(required_hits(2), 2);
        assert_eq!(required_hits(10), 9);
        assert_eq!(required_hits(100), 64, "大输入直接封顶，不再要求 85%");
        assert_eq!(required_hits(240), 64);
        assert!(acceptable_hits(240) < required_hits(240));
    }

    #[test]
    fn long_histories_sample_anchors_evenly() {
        // 3000 个互不相同的锚点必须抽样到 240 个，且开头和结尾都要有代表。
        let mut messages = Vec::new();
        for index in 0..3000 {
            messages.push(Message::user_text(format!(
                "crates/ingest/src/module_{index}.rs 的第 {index} 步"
            )));
        }
        let anchors = salient_anchors(messages.iter(), None);
        assert_eq!(anchors.len(), MAX_ANCHORS);
        assert!(anchors.iter().any(|anchor| anchor.contains("module_0.rs")));
        assert!(
            anchors.iter().any(|anchor| anchor.contains("module_2")),
            "抽样必须覆盖后半段"
        );
    }
}
