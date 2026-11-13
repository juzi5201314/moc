use super::MISSING_TEXT;
use super::NOTES_MAX_CHARS;
use super::TOTAL_LABELS;
use super::numbers::count;
use super::numbers::is_digits;
use super::numbers::split_numeric;
use std::borrow::Cow;

/// 该文本是否像「状态词」（`TIMEOUT`、`失败`）：既不是日期/时间/版本等结构化文本，也不是长说明。
pub(crate) fn is_status_word(text: &str) -> bool {
    !is_structured_text(text) && text.chars().count() <= NOTES_MAX_CHARS
}

/// 整格是否是无值记号。
pub(super) fn is_missing(text: &str) -> bool {
    MISSING_TEXT
        .iter()
        .any(|mark| text.eq_ignore_ascii_case(mark))
}

/// 首格是否是合计词。
pub(super) fn is_total_label(text: &str) -> bool {
    let trimmed = text.trim().trim_end_matches([':', '：']).trim();
    TOTAL_LABELS
        .iter()
        .any(|label| trimmed.eq_ignore_ascii_case(label))
}

// ---------------------------------------------------------------- 去装饰

/// 去装饰：还原 `\|`、保留 `` `code` `` 内容、剥离强调/删除线标记与链接 URL、
/// HTML 标签换成空格、剥离状态记号、折叠空白。
///
/// 没有需要处理的字符时直接借用原串（数值单元格是热路径，避免无谓分配）。
pub(super) fn clean(cell: &str) -> Cow<'_, str> {
    if !needs_cleaning(cell) {
        return Cow::Borrowed(cell.trim());
    }
    let mut out = String::with_capacity(cell.len());
    let mut pending_space = false;
    let mut pos = 0;
    while pos < cell.len() {
        let ch = cell[pos..].chars().next().expect("pos 始终落在字符边界");
        match ch {
            '\\' => {
                if cell[pos + 1..].starts_with('|') {
                    push_clean(&mut out, &mut pending_space, '|');
                    pos += 2;
                } else {
                    push_clean(&mut out, &mut pending_space, '\\');
                    pos += 1;
                }
            }
            '`' => match cell[pos + 1..].find('`') {
                Some(end) => {
                    for inner in cell[pos + 1..pos + 1 + end].chars() {
                        push_clean(&mut out, &mut pending_space, inner);
                    }
                    pos += 1 + end + 1;
                }
                None => pos += 1,
            },
            // 强调/删除线标记；单个 `~` 是近似记号，必须留给扫描器
            '*' | '_' => pos += 1,
            '~' => {
                if cell[pos + 1..].starts_with('~') {
                    pos += 2;
                } else {
                    push_clean(&mut out, &mut pending_space, '~');
                    pos += 1;
                }
            }
            '[' => match link_text(&cell[pos..]) {
                Some((text, len)) => {
                    for inner in text.chars() {
                        push_clean(&mut out, &mut pending_space, inner);
                    }
                    pos += len;
                }
                None => {
                    push_clean(&mut out, &mut pending_space, '[');
                    pos += 1;
                }
            },
            '!' if cell[pos + 1..].starts_with('[') => pos += 1,
            '<' => match html_tag_len(&cell[pos..]) {
                Some(len) => {
                    if !out.is_empty() {
                        pending_space = true;
                    }
                    pos += len;
                }
                None => {
                    push_clean(&mut out, &mut pending_space, '<');
                    pos += 1;
                }
            },
            mark if is_status_mark(mark) => pos += mark.len_utf8(),
            blank if blank.is_whitespace() => {
                if !out.is_empty() {
                    pending_space = true;
                }
                pos += blank.len_utf8();
            }
            other => {
                push_clean(&mut out, &mut pending_space, other);
                pos += other.len_utf8();
            }
        }
    }
    Cow::Owned(out)
}

/// 是否需要走 [`clean`]：含标记、状态记号、HTML 标签前缀或需要折叠的空白。
pub(super) fn needs_cleaning(text: &str) -> bool {
    if text.contains("![") || text.contains("~~") {
        return true;
    }
    let mut prev_space = false;
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '[' | '<' => return true,
            mark if is_status_mark(mark) => return true,
            blank if blank.is_whitespace() => {
                if blank != ' ' || prev_space {
                    return true;
                }
                prev_space = true;
                continue;
            }
            _ => {}
        }
        prev_space = false;
    }
    false
}

/// 追加一个字符，必要时先补一个空格（同时抑制行首与结尾空格）。
pub(super) fn push_clean(out: &mut String, pending_space: &mut bool, ch: char) {
    if *pending_space && !out.is_empty() {
        out.push(' ');
    }
    *pending_space = false;
    out.push(ch);
}

/// 状态记号：含义由相邻数值承载，单独出现时只是噪声。
pub(super) fn is_status_mark(ch: char) -> bool {
    matches!(
        ch,
        '✅' | '❌' | '⚠' | '✓' | '✗' | '✔' | '★' | '⭐' | '↑' | '↓' | '▲' | '▼'
    )
}

/// `[text](url)` / `[text]` 的文本部分；返回 `(文本, 整段字节长度)`。
pub(super) fn link_text(section: &str) -> Option<(&str, usize)> {
    let rest = &section[1..];
    let end = rest.find(']')?;
    let text = &rest[..end];
    let after = &rest[end + 1..];
    if after.starts_with('(') {
        let close = after.find(')')?;
        Some((text, 1 + end + 1 + close + 1))
    } else {
        Some((text, 1 + end + 1))
    }
}

/// HTML 标签的字节长度（含 `<>`）：`<` 后必须是字母或 `/`/`!`/`?`，且 64 字节内闭合。
///
/// `< 5`、`<=10` 不是标签，必须原样留给近似记号扫描器。
pub(super) fn html_tag_len(section: &str) -> Option<usize> {
    let rest = &section[1..];
    let first = rest.chars().next()?;
    if !(first.is_ascii_alphabetic() || matches!(first, '/' | '!' | '?')) {
        return None;
    }
    let mut limit = rest.len().min(64);
    while !rest.is_char_boundary(limit) {
        limit -= 1;
    }
    let end = rest[..limit].find('>')?;
    Some(1 + end + 1)
}

// ---------------------------------------------------------------- 结构化文本

/// 日期、时间、版本号、`#123`、十六进制串：都是「看起来像数字但不能当数值画」的文本。
pub(super) fn is_structured_text(text: &str) -> bool {
    is_date_like(text)
        || is_time_like(text)
        || is_version_like(text)
        || is_hex_like(text)
        || is_issue_ref(text)
}

/// 日期形态：`2024-01-05`、`2024/01/05`、`1/5/24`、`2024-01`（年-月）、含 `年` 的中文写法。
pub(super) fn is_date_like(text: &str) -> bool {
    if text.contains('年') {
        return true;
    }
    let Some((first, second, third)) =
        split_numeric(text, '-').or_else(|| split_numeric(text, '/'))
    else {
        return false;
    };
    match third {
        // `YYYY-MM-DD`：首段 4 位年份；`M/D/YY`：末段 2 位或 4 位年份。
        Some(third) => {
            (first.len() == 4
                && (1..=12).contains(&count(second))
                && (1..=31).contains(&count(third)))
                || ((1..=12).contains(&count(first))
                    && (1..=31).contains(&count(second))
                    && matches!(third.len(), 2 | 4))
        }
        // `YYYY-MM`：年-月，不按区间解析，否则「月」会被当成区间上界。
        None => first.len() == 4 && (1..=12).contains(&count(second)),
    }
}

/// 时间形态：`MM:SS`、`HH:MM:SS`（秒/分不超过 59）。
pub(super) fn is_time_like(text: &str) -> bool {
    let Some((first, second, third)) = split_numeric(text, ':') else {
        return false;
    };
    let in_range = |part: &str| count(part) <= 59;
    match third {
        Some(third) => first.len() <= 3 && in_range(second) && in_range(third),
        None => first.len() <= 3 && in_range(second),
    }
}

/// 版本形态：`v1.2.3`、`1.2.3`（至少两个点，或带 `v` 前缀的 `1.2`）。
pub(super) fn is_version_like(text: &str) -> bool {
    let (body, tagged) = match text.strip_prefix(['v', 'V']) {
        Some(body) => (body, true),
        None => (text, false),
    };
    let mut parts = 0;
    for part in body.split('.') {
        if part.is_empty() || !is_digits(part) {
            return false;
        }
        parts += 1;
    }
    parts >= 3 || (tagged && parts >= 2)
}

/// 十六进制串：`0x` 前缀，或 7–40 位且至少含一个字母的十六进制（纯数字是计数，不是哈希）。
pub(super) fn is_hex_like(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return (1..=40).contains(&rest.len()) && rest.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    (7..=40).contains(&text.len())
        && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        && text.bytes().any(|byte| byte.is_ascii_alphabetic())
}

/// `#123` 形态的引用编号。
pub(super) fn is_issue_ref(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(is_digits)
}
