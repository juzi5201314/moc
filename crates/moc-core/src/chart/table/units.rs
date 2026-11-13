use super::Dimension;
use super::Scanner;
use super::UnitEntry;
use super::UnitHit;

/// 时间单位（基本单位：秒）。
pub(super) const TIME_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "ns",
        dim: Dimension::Duration,
        factor: 1e-9,
        mult: 1e-9,
    },
    UnitEntry {
        text: "µs",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "μs",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "us",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "ms",
        dim: Dimension::Duration,
        factor: 1e-3,
        mult: 1e-3,
    },
    UnitEntry {
        text: "s",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "sec",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "secs",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "second",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "seconds",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "m",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "min",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "mins",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "minute",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "minutes",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "h",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hr",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hrs",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hour",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hours",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "d",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
    UnitEntry {
        text: "day",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
    UnitEntry {
        text: "days",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
];

/// 字节单位（基本单位：字节）；`iB` 系是 1024 进制，`B` 系是 1000 进制。
pub(super) const BYTE_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "KiB",
        dim: Dimension::Bytes,
        factor: 1024.0,
        mult: 1024.0,
    },
    UnitEntry {
        text: "MiB",
        dim: Dimension::Bytes,
        factor: 1_048_576.0,
        mult: 1_048_576.0,
    },
    UnitEntry {
        text: "GiB",
        dim: Dimension::Bytes,
        factor: 1_073_741_824.0,
        mult: 1_073_741_824.0,
    },
    UnitEntry {
        text: "TiB",
        dim: Dimension::Bytes,
        factor: 1_099_511_627_776.0,
        mult: 1_099_511_627_776.0,
    },
    UnitEntry {
        text: "KB",
        dim: Dimension::Bytes,
        factor: 1e3,
        mult: 1e3,
    },
    UnitEntry {
        text: "MB",
        dim: Dimension::Bytes,
        factor: 1e6,
        mult: 1e6,
    },
    UnitEntry {
        text: "GB",
        dim: Dimension::Bytes,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "TB",
        dim: Dimension::Bytes,
        factor: 1e12,
        mult: 1e12,
    },
    UnitEntry {
        text: "bytes",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "byte",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "B",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
];

/// 量级单位（基本单位：计数）。大小写敏感：`k` 千、`M` 百万、`G` 十亿、`T` 万亿、`b`/`bn` 十亿。
pub(super) const MAGNITUDE_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "bn",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "k",
        dim: Dimension::Count,
        factor: 1e3,
        mult: 1e3,
    },
    UnitEntry {
        text: "M",
        dim: Dimension::Count,
        factor: 1e6,
        mult: 1e6,
    },
    UnitEntry {
        text: "G",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "T",
        dim: Dimension::Count,
        factor: 1e12,
        mult: 1e12,
    },
    UnitEntry {
        text: "b",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "pp",
        dim: Dimension::Percent,
        factor: 1.0,
        mult: 1.0,
    },
];

/// 表头里的单位：`耗时 (ms)`、`体积（KiB）`、`吞吐 tok/s`、`覆盖率 %`。
///
/// 单元格自己写了单位时以单元格为准；表头单位只补齐裸数字，
/// 否则不同量纲的列会全被当成无量纲计数并进同一组，散点与堆叠都会判错。
pub(super) fn header_unit(header: &str) -> Option<UnitHit<'_>> {
    let body = trailing_group(header).unwrap_or_else(|| last_word(header));
    let mut scanner = Scanner { text: body, pos: 0 };
    let hit = scanner.take_unit()?;
    scanner.skip_spaces();
    (scanner.pos == body.len()).then_some(hit)
}

/// 末尾括号组的内容（`耗时 (ms)` → `ms`）。
pub(super) fn trailing_group(header: &str) -> Option<&str> {
    let start = header.rfind(['(', '（'])?;
    let open = header[start..].chars().next()?.len_utf8();
    let close = header[start + open..].find([')', '）'])?;
    Some(header[start + open..start + open + close].trim())
}

/// 最后一个空白分隔的词；没有空白时就是整个表头。
pub(super) fn last_word(header: &str) -> &str {
    header.rsplit(char::is_whitespace).next().unwrap_or(header)
}

/// 速率单位（`tok/s`、`MB/s`、`it/s`）：字母开头的一段 + `/` + 字母开头的一段。
pub(super) fn rate_unit(rest: &str) -> Option<(&str, usize)> {
    let head_end = unit_chars(rest);
    if !rest[..head_end].chars().any(is_unit_letter) {
        return None;
    }
    if !rest[head_end..].starts_with('/') {
        return None;
    }
    let tail = &rest[head_end + 1..];
    let tail_end = unit_chars(tail);
    if !tail[..tail_end].chars().any(is_unit_letter) {
        return None;
    }
    let len = head_end + 1 + tail_end;
    Some((&rest[..len], len))
}

/// 从头部吃下单位字符（ASCII 字母数字与微符号），返回字节长度。
pub(super) fn unit_chars(text: &str) -> usize {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, 'µ' | 'μ') {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// 是否算速率的字母段（ASCII 字母或微符号）。
pub(super) fn is_unit_letter(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, 'µ' | 'μ')
}

/// 词表单位：按最长匹配取，并要求后续不是 ASCII 字母数字（`msx` 不是 `ms`）。
pub(super) fn unit_token(rest: &str) -> Option<UnitHit<'static>> {
    let mut best: Option<UnitHit<'static>> = None;
    for entry in TIME_UNITS.iter().chain(BYTE_UNITS).chain(MAGNITUDE_UNITS) {
        if rest.starts_with(entry.text)
            && !is_alnum(rest[entry.text.len()..].chars().next())
            && best.is_none_or(|current| entry.text.len() > current.text.len())
        {
            best = Some(entry.hit());
        }
    }
    best
}

/// 字符是否是 ASCII 字母数字（用于单位边界判断）。
pub(super) fn is_alnum(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_alphanumeric())
}
