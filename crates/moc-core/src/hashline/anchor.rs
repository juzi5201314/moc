//! 行号加短哈希锚点。

use xxhash_rust::xxh32::Xxh32;

/// 重定位搜索允许的最大行号距离。
pub const REBASE_WINDOW: usize = 5;
/// 锚点后缀使用的小写字母表。
pub const ID_ALPHABET: [u8; 26] = *b"abcdefghijklmnopqrstuvwxyz";
/// 两字母后缀实际使用的哈希空间。
pub const ID_SPACE: u32 = 647;
/// 纯符号行使用行号作为种子的上限。
pub const ID_SEED_LINE_LIMIT: usize = 40_000;

/// 根据 1 基行号和行文本计算两字母锚点后缀。
pub fn id_of(line_no: usize, text: &str) -> [u8; 2] {
    let significant = text.chars().any(char::is_alphanumeric);
    let seed = if significant {
        0
    } else {
        u32::try_from(line_no.min(ID_SEED_LINE_LIMIT)).expect("ID_SEED_LINE_LIMIT must fit in u32")
    };
    let trimmed = text.trim_end_matches(['\r', '\n', ' ', '\t']);
    let mut hasher = Xxh32::new(seed);
    hasher.update(trimmed.as_bytes());
    let index = hasher.digest() % ID_SPACE;
    let high = usize::try_from(index / 26).expect("ID index must fit in usize");
    let low = usize::try_from(index % 26).expect("ID index must fit in usize");
    [ID_ALPHABET[high], ID_ALPHABET[low]]
}

/// 将两字母后缀转为字符串。
pub fn id_str(id: [u8; 2]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

/// 返回不带行文本的锚点 token。
pub fn anchor_token(line_no: usize, text: &str) -> String {
    format!("{line_no}{}", id_str(id_of(line_no, text)))
}

/// 返回 read/diff 使用的完整锚点行。
pub fn anchor_line(line_no: usize, text: &str) -> String {
    format!("{}│{text}", anchor_token(line_no, text))
}

/// 解析后的锚点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorParse {
    pub line: usize,
    pub id: [u8; 2],
    /// 输入带有整行、diff 前缀或空白装饰时，提供清洗后的裸 token。
    pub trimmed: Option<String>,
}

/// 解析模型可能从 read/diff 复制的锚点。
///
/// 失败时返回收到的原文，供调用方在错误载荷中回显。
pub fn parse_anchor(raw: &str) -> Result<AnchorParse, String> {
    let mut value = raw;
    if let Some((prefix, _)) = value.split_once('│') {
        value = prefix;
    }
    value = value.trim_start_matches(|character: char| {
        character.is_whitespace() || matches!(character, '+' | '-' | '>')
    });
    value = value.trim();
    if let Some((token, _)) = value.split_once(char::is_whitespace) {
        value = token;
    }

    let changed = value != raw;
    let bytes = value.as_bytes();
    if bytes.len() < 3 {
        return Err(raw.to_owned());
    }
    let split = bytes.len() - 2;
    if bytes.first() == Some(&b'0')
        || !bytes[..split].iter().all(u8::is_ascii_digit)
        || !bytes[split..].iter().all(u8::is_ascii_lowercase)
    {
        return Err(raw.to_owned());
    }
    let line = value[..split]
        .parse::<usize>()
        .map_err(|_| raw.to_owned())?;
    if line == 0 {
        return Err(raw.to_owned());
    }
    Ok(AnchorParse {
        line,
        id: [bytes[split], bytes[split + 1]],
        trimmed: changed.then(|| value.to_owned()),
    })
}

/// 统一生成锚点展示文本；路径参数保留给调用方的统一入口语义。
pub fn anchor_of(_path: &std::path::Path, line_no: usize, text: &str) -> String {
    anchor_line(line_no, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ids_are_independent_of_line_number() {
        assert_eq!(id_of(1, "fn main() {}"), id_of(99, "fn main() {}"));
    }

    #[test]
    fn punctuation_ids_use_line_number_but_cap_the_seed() {
        assert_ne!(id_of(1, "}"), id_of(2, "}"));
        assert_eq!(id_of(40_000, "}"), id_of(40_001, "}"));
        assert_eq!(id_of(40_001, "}"), id_of(80_000, "}"));
    }

    #[test]
    fn parses_bare_and_decorated_anchors() {
        let bare = parse_anchor("42sr").expect("bare anchor should parse");
        assert_eq!(bare.line, 42);
        assert_eq!(bare.id, *b"sr");
        assert_eq!(bare.trimmed, None);
        assert_eq!(
            parse_anchor("42sr│pub fn main() {}")
                .expect("full anchor line should parse")
                .trimmed,
            Some("42sr".into())
        );
        assert_eq!(
            parse_anchor("+ 13cd│text")
                .expect("diff anchor line should parse")
                .trimmed,
            Some("13cd".into())
        );
        assert_eq!(
            parse_anchor("- 9zs│text")
                .expect("deleted anchor line should parse")
                .trimmed,
            Some("9zs".into())
        );
        assert_eq!(
            parse_anchor(" 42sr extra")
                .expect("whitespace-decorated anchor should parse")
                .trimmed,
            Some("42sr".into())
        );
    }

    #[test]
    fn rejects_missing_or_invalid_anchor_parts() {
        for raw in [
            "sr",
            "pub fn main() {}",
            "0sr",
            "42s",
            "42SR",
            "42srx",
            "42",
        ] {
            assert!(parse_anchor(raw).is_err(), "{raw}");
        }
    }
}
