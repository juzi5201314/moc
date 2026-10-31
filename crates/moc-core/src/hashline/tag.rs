//! 文件快照 tag 与带行号的行渲染。
//!
//! 每个文件版本只有一个 4 位十六进制 tag，由整份内容的 xxh3 摘要折叠而来；
//! 行号就是该版本里的 1 基行号，模型不需要抄写逐行令牌。

use xxhash_rust::xxh3::Xxh3;

/// 整份内容的 64 位摘要；行数一并参与，避免末尾空行差异撞到同一摘要。
pub fn content_hash(lines: &[String]) -> u64 {
    let mut hasher = Xxh3::new();
    let count = u64::try_from(lines.len()).expect("line count must fit in u64");
    hasher.update(&count.to_le_bytes());
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hasher.digest()
}

/// 把 64 位摘要折叠成展示用的 16 位 tag。
pub fn tag_of(hash: u64) -> u16 {
    // 有意截断：异或折叠后只取低 16 位。
    (hash ^ (hash >> 16) ^ (hash >> 32) ^ (hash >> 48)) as u16
}

/// tag 的展示文本，固定 4 个大写十六进制字符。
pub fn tag_text(tag: u16) -> String {
    format!("{tag:04X}")
}

/// 解析模型给出的 tag；接受裸值、`#TAG` 以及整段 `[path#TAG]` 头部。
pub fn parse_tag(raw: &str) -> Option<u16> {
    let raw = raw.trim();
    let raw = raw.rsplit_once('#').map_or(raw, |(_, tail)| tail);
    let raw = raw.strip_suffix(']').unwrap_or(raw).trim();
    if raw.len() != 4 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(raw, 16).ok()
}

/// 渲染一行可编辑内容：`行号│原文`。
pub fn numbered_line(line_no: usize, text: &str) -> String {
    format!("{line_no}│{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_accepts_header_forms_and_rejects_malformed_values() {
        for raw in ["3F2A", "3f2a", "#3F2A", "[src/a.rs#3F2A]", " 3F2A "] {
            assert_eq!(parse_tag(raw), Some(0x3F2A), "{raw}");
        }
        for raw in ["3F2", "3F2AB", "ZZZZ", "", "#"] {
            assert_eq!(parse_tag(raw), None, "{raw}");
        }
    }

    #[test]
    fn different_content_gets_different_tags() {
        let one = vec!["a".to_owned(), "b".to_owned()];
        let two = vec!["a".to_owned(), "c".to_owned()];
        assert_ne!(
            tag_of(content_hash(&one)),
            tag_of(content_hash(&two))
        );
    }
}
