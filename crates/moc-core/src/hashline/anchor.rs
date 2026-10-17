//! 行号加短哈希锚点。
//!
//! 锚点由「展示行号 + 两字母 id」组成，是一枚会话内稳定的令牌：
//! 行被首次展示时分配，之后随行移动而保留；行被重新展示时令牌行号更新到
//! 当前位置。解析按完整令牌精确匹配，不依赖当前行号重算，因此重复内容行、
//! 纯符号行在位移后仍能定位到原行。

use xxhash_rust::xxh32::Xxh32;

/// 锚点后缀使用的小写字母表。
pub const ID_ALPHABET: [u8; 26] = *b"abcdefghijklmnopqrstuvwxyz";
/// 两字母后缀的哈希空间基数（用质数压缩模偏置）。
pub const ID_SPACE: u32 = 647;
/// 两字母 id 的全部可用数量；令牌探测与行内去重使用完整空间。
pub const ID_TOTAL: u16 = 26 * 26;
/// 纯符号行使用行号作为种子的上限。
pub const ID_SEED_LINE_LIMIT: usize = 40_000;

/// 会话内稳定的展示令牌：行号是最近一次展示时的位置，id 是行的稳定身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnchorToken {
    pub line: usize,
    pub id: [u8; 2],
}

impl AnchorToken {
    /// 令牌的裸文本，例如 `42sr`。
    pub fn text(&self) -> String {
        format!("{}{}", self.line, id_str(self.id))
    }
}

/// 根据 1 基行号和行文本计算首选的两字母后缀。
///
/// 首选值只在首次展示时使用；之后 id 由 [`AnchorToken`] 保存，不再随行号重算。
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
    id_from_index(index)
}

/// 将两字母后缀转为字符串。
pub fn id_str(id: [u8; 2]) -> String {
    String::from_utf8_lossy(&id).into_owned()
}

/// 两字母 id 在完整空间中的下标。
pub fn id_index(id: [u8; 2]) -> u16 {
    u16::from(id[0] - b'a') * 26 + u16::from(id[1] - b'a')
}

/// 从完整空间下标还原两字母 id。
pub fn id_from_index(index: u32) -> [u8; 2] {
    let index = u16::try_from(index).expect("id index must fit in u16");
    assert!(
        index < ID_TOTAL,
        "id index must be within the two-letter space"
    );
    [
        ID_ALPHABET[usize::from(index / 26)],
        ID_ALPHABET[usize::from(index % 26)],
    ]
}

/// 令牌在「行号 + id」空间中的紧凑键，用于会话内去重。
pub fn token_key(token: AnchorToken) -> u64 {
    u64::try_from(token.line).expect("line number must fit in u64") * u64::from(ID_TOTAL)
        + u64::from(id_index(token.id))
}

/// 返回不带行文本的锚点 token（首次展示时的首选值）。
pub fn anchor_token(line_no: usize, text: &str) -> String {
    AnchorToken {
        line: line_no,
        id: id_of(line_no, text),
    }
    .text()
}

/// 使用已有令牌渲染锚点行。
pub fn anchor_line_token(token: AnchorToken, text: &str) -> String {
    format!("{}│{text}", token.text())
}

/// 解析后的锚点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorParse {
    pub line: usize,
    pub id: [u8; 2],
    /// 输入带有整行、diff 前缀或空白装饰时，提供清洗后的裸 token。
    pub trimmed: Option<String>,
}

impl AnchorParse {
    pub fn token(&self) -> AnchorToken {
        AnchorToken {
            line: self.line,
            id: self.id,
        }
    }
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

    #[test]
    fn id_index_round_trips_the_full_two_letter_space() {
        for index in 0..ID_TOTAL {
            let id = id_from_index(u32::from(index));
            assert_eq!(id_index(id), index);
        }
    }

    #[test]
    fn token_keys_are_unique_across_lines_with_the_same_id() {
        let token = AnchorToken {
            line: 7,
            id: *b"zz",
        };
        let other = AnchorToken {
            line: 8,
            id: *b"zz",
        };
        assert_ne!(token_key(token), token_key(other));
        assert_eq!(token.text(), "7zz");
    }
}
