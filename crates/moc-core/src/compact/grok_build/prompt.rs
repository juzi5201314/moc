//! Grok Build 的摘要提示词、输出清洗，以及两遍压缩之间的笔记传递。

/// Grok Build 的 9 段摘要模板；`{user_context_section}` 由调用方填充。
const GROK_BUILD_SUMMARY_PROMPT: &str = include_str!("grok_build_summary_prompt.txt");

/// 视为可用摘要的最小字符数；低于这个值按退化处理并重试。
pub(crate) const MIN_SUMMARY_CHARS: usize = 500;

/// 第二遍可用的最小 NOTE₁ 长度；太短的 `<summary>` 块只是残片。
pub(crate) const MIN_NOTE1_CHARS: usize = 1000;

/// 第二遍输入的 NOTE₁ 字符上限。
pub(crate) const MAX_NOTE1_CHARS: usize = 60_000;

/// 控制标签一旦留在摘要里，回放时就会被当成真正的标签，必须中和。
const CONTROL_TOKENS: [&str; 6] = [
    "</summary>",
    "<summary>",
    "</analysis>",
    "<analysis>",
    "</summary_request>",
    "<summary_request>",
];

/// 组装本次压缩请求的提示词；用户的手动指令作为上下文插入。
pub(crate) fn build_summary_prompt(instructions: Option<&str>) -> String {
    let section = match instructions {
        Some(context) => format!(
            "\n\n**User-provided context for this compaction:**\n{context}\n\nPlease incorporate this context into your summary, ensuring it is prominently addressed in the relevant sections.\n\n"
        ),
        None => String::new(),
    };
    GROK_BUILD_SUMMARY_PROMPT.replace("{user_context_section}", &section)
}

/// 摘要是否退化：清洗后没有正文，或短到不足以承担续接职责。
pub(crate) fn is_degenerate_summary(raw: &str) -> bool {
    clean_summary(raw).chars().count() < MIN_SUMMARY_CHARS
}

/// 把模型输出整理成可以直接回放的摘要正文。
///
/// Grok Build 允许模型先写 `<analysis>` 草稿再写 `<summary>`。草稿不能进入上下文，
/// `<summary>` 标签本身也要换成人读的 `Summary:` 前缀。
pub(crate) fn clean_summary(raw: &str) -> String {
    let mut text = strip_leading_analysis(raw).to_owned();
    if let Some(start) = find_ci(&text, "<summary>", 0) {
        let inner_start = start + "<summary>".len();
        if let Some(end) = find_last_ci(&text, "</summary>", inner_start)
            && end >= inner_start
        {
            let body = strip_leading_scratchpad(&text[inner_start..end]);
            let mut replaced = String::with_capacity(text.len());
            replaced.push_str(&text[..start]);
            replaced.push_str("Summary:\n");
            replaced.push_str(&body);
            replaced.push_str(&text[end + "</summary>".len()..]);
            text = replaced;
        }
    }
    let text = neutralize_control_tokens(&text);
    let mut collapsed = text;
    while collapsed.contains("\n\n\n") {
        collapsed = collapsed.replace("\n\n\n", "\n\n");
    }
    collapsed.trim().to_owned()
}

/// 反复剥掉开头的 `<analysis>` 草稿块；没闭合时删到下一个 `<summary>` 或文末。
fn strip_leading_analysis(text: &str) -> &str {
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        let Some(after_open) = trimmed.strip_prefix("<analysis>") else {
            break trimmed;
        };
        match after_open.find("</analysis>") {
            Some(end) => rest = &after_open[end + "</analysis>".len()..],
            None => {
                break match after_open.find("<summary>") {
                    Some(next) => &after_open[next..],
                    None => "",
                };
            }
        }
    }
}

/// `<summary>` 内部可能还带着草稿：先剥壳，再丢掉不以数字开头的 `</analysis>` 前缀。
fn strip_leading_scratchpad(inner: &str) -> String {
    let mut body = inner.trim();
    let head = body.trim_start_matches(['#', '*', '-', ' ', '>', '\t']);
    let starts_with_number = head
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit());
    if !starts_with_number && let Some(end) = body.rfind("</analysis>") {
        body = body[end + "</analysis>".len()..].trim_start();
    }
    let body = body.strip_prefix("<summary>").unwrap_or(body);
    body.trim().to_owned()
}

/// 在 `<` 后插入零宽空格，回放时就不会再形成活动标签。
fn neutralize_control_tokens(text: &str) -> String {
    let mut out = text.to_owned();
    for token in CONTROL_TOKENS {
        if !out.contains(token) {
            continue;
        }
        let neutralized = format!("<\u{200b}{}", &token[1..]);
        out = out.replace(token, &neutralized);
    }
    out
}

/// 取出最后一个正文超过 `min_chars` 的 `<summary>` 块；匹配忽略大小写。
pub(crate) fn extract_summary_block(text: &str, min_chars: usize) -> Option<String> {
    let mut found = None;
    let mut cursor = 0usize;
    while let Some(start) = find_ci(text, "<summary>", cursor) {
        let inner_start = start + "<summary>".len();
        let Some(end) = find_ci(text, "</summary>", inner_start) else {
            break;
        };
        let inner = text[inner_start..end].trim();
        if inner.chars().count() > min_chars {
            found = Some(inner.to_owned());
        }
        cursor = end + "</summary>".len();
    }
    found
}

/// 第一遍摘要交给第二遍使用的 NOTE₁：优先取 `<summary>` 块，太长时按字符截断。
pub(crate) fn note_for_two_pass_pass2(raw: &str) -> String {
    let note = extract_summary_block(raw, MIN_NOTE1_CHARS).unwrap_or_else(|| raw.trim().to_owned());
    if note.chars().count() <= MAX_NOTE1_CHARS {
        return note;
    }
    let truncated: String = note.chars().take(MAX_NOTE1_CHARS).collect();
    format!("{truncated}\n\n[… NOTE₁ truncated for pass2 input budget …]")
}

/// 忽略 ASCII 大小写的子串查找，返回字节下标。
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let pattern = needle.as_bytes();
    if pattern.is_empty() || from >= hay.len() || hay.len() - from < pattern.len() {
        return None;
    }
    (from..=hay.len() - pattern.len())
        .find(|&index| hay[index..index + pattern.len()].eq_ignore_ascii_case(pattern))
}

/// 从 `from` 开始查找，返回最后一次匹配；用来把第一个 `<summary>` 和末尾的 `</summary>` 配对。
fn find_last_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let mut found = None;
    let mut cursor = from;
    while let Some(index) = find_ci(haystack, needle, cursor) {
        found = Some(index);
        cursor = index + needle.len();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(text: &str) -> String {
        text.repeat(600)
    }

    #[test]
    fn prompt_carries_all_nine_sections_and_user_context() {
        let plain = build_summary_prompt(None);
        for section in [
            "1. Primary Request and Intent",
            "2. Key Technical Concepts",
            "3. Files and Code Sections",
            "4. Errors and Fixes",
            "5. Problem Solving",
            "6. All User Messages",
            "7. Pending Tasks",
            "8. Current Work",
            "9. Optional Next Step",
        ] {
            assert!(plain.contains(section), "缺少 {section}");
        }
        assert!(!plain.contains("{user_context_section}"));
        assert!(!plain.contains("**User-provided context for this compaction:**"));

        let manual = build_summary_prompt(Some("只保留压缩部分"));
        assert!(manual.contains("**User-provided context for this compaction:**\n只保留压缩部分"));
    }

    #[test]
    fn clean_summary_strips_wrappers_and_normalizes_text() {
        let cases = [
            (
                "<analysis>先想想怎么写</analysis>\n\n<summary>\n1. Primary Request and Intent: 做事\n</summary>",
                "Summary:\n1. Primary Request and Intent: 做事",
            ),
            (
                "<summary># 草稿\n<analysis>中间推理</analysis>\n1. Primary: 做事</summary>",
                "Summary:\n1. Primary: 做事",
            ),
            (
                "<summary>1. Primary: 做事</summary>",
                "Summary:\n1. Primary: 做事",
            ),
            (
                "<analysis>没有闭合\n\n<summary>正文</summary>",
                "Summary:\n正文",
            ),
            ("<analysis>只剩草稿", ""),
            ("第一段\n\n\n\n第二段\n", "第一段\n\n第二段"),
            ("没有标签的纯文本", "没有标签的纯文本"),
        ];
        for (raw, expected) in cases {
            assert_eq!(clean_summary(raw), expected, "{raw}");
        }
    }

    #[test]
    fn control_tokens_are_neutralized_but_section_six_echo_survives() {
        let raw = "<summary>6. All User Messages: 用户说过 <summary>假的</summary> 与 </analysis> 以及 <summary_request></summary>";
        let cleaned = clean_summary(raw);
        assert!(!cleaned.contains("<summary>"), "{cleaned}");
        assert!(cleaned.contains("<\u{200b}summary>"), "{cleaned}");
        assert!(cleaned.contains("6. All User Messages"), "{cleaned}");
        assert!(!cleaned.contains("</analysis>"), "{cleaned}");
        assert!(!cleaned.contains("<summary_request>"), "{cleaned}");
    }

    #[test]
    fn degenerate_detection_uses_cleaned_chars() {
        assert!(is_degenerate_summary("<summary>太短</summary>"));
        assert!(is_degenerate_summary(""));
        assert!(!is_degenerate_summary(&format!(
            "<summary>{}</summary>",
            long("内容")
        )));
    }

    #[test]
    fn note_prefers_last_long_summary_block_and_truncates() {
        let short = "<summary>短</summary>";
        let long_block = format!("<summary>{}</summary>", long("摘要"));
        let raw = format!("{short}{long_block}");
        assert_eq!(
            note_for_two_pass_pass2(&raw),
            long("摘要").trim().to_owned()
        );
        assert_eq!(note_for_two_pass_pass2("纯文本笔记"), "纯文本笔记");

        let huge = "字".repeat(MAX_NOTE1_CHARS + 10);
        let truncated = note_for_two_pass_pass2(&huge);
        assert!(truncated.ends_with("[… NOTE₁ truncated for pass2 input budget …]"));
        assert_eq!(
            truncated.chars().filter(|c| *c == '字').count(),
            MAX_NOTE1_CHARS
        );
    }

    #[test]
    fn extract_is_case_insensitive_and_skips_short_blocks() {
        let raw = "<SUMMARY>短</SUMMARY><Summary>这是一段足够长的正文</Summary>";
        assert_eq!(
            extract_summary_block(raw, MIN_NOTE1_CHARS),
            None,
            "两个块都短于阈值"
        );
        assert_eq!(
            extract_summary_block(raw, 5).as_deref(),
            Some("这是一段足够长的正文")
        );
    }
}
