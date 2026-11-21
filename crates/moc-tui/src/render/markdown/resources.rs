use super::super::image::ImageKey;

/// 代码块 info 串是否声明了 SVG：首个空白分隔 token，大小写不敏感。
///
/// 转写层的廉价预筛也走这个判定，两处必须完全一致——否则懒块的估高会用
/// 折行近似顶替真实的占位行。
pub(crate) fn is_svg_fence_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("svg"))
}

/// 代码块 info 串是否声明了 mermaid。判定口径与 [`is_svg_fence_info`] 相同。
pub(crate) fn is_mermaid_fence_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("mermaid"))
}

/// 代码块 info 串是否声明了 WebUI 交互组件（`widget`）。
pub(crate) fn is_widget_fence_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("widget"))
}

/// 从 `widget` 围栏 info 串中提取 `title="..."` 或 `title='...'`。
pub(crate) fn widget_fence_title(info: &str) -> Option<String> {
    let bytes = info.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let key = &info[key_start..i];
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let mut out = String::new();
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 1;
                        let ch = info[i..].chars().next()?;
                        out.push(ch);
                        i += ch.len_utf8();
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    let ch = info[i..].chars().next()?;
                    out.push(ch);
                    i += ch.len_utf8();
                }
                out
            } else {
                let start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                info[start..i].to_owned()
            };
            if key.eq_ignore_ascii_case("title") {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
    }
    None
}

/// 围栏代码块的源码区间是否以闭合围栏结尾。
///
/// pulldown 把未闭合的围栏收到文件末尾，并把空余量当成一次长度为 0 的闭合，
/// 所以区间终点必须落在与开围栏同字符且不更短的真围栏上才算闭合——内容行
/// 末尾的 `` ``` `` 在 ```` ```` ```` 块里并不算闭合。对不齐时当成已闭合：
/// 画不出来会退回代码块，比把已写完的图留在源码里好。
pub(super) fn fence_is_closed(source: &str, range: std::ops::Range<usize>) -> bool {
    let bytes = source.as_bytes();
    if range.end == 0 || range.end > bytes.len() || range.start > bytes.len() {
        return true;
    }
    let Some((fence, needed)) = fence_marker(&bytes[range.start..]) else {
        return true;
    };
    let mut index = range.end;
    while index > range.start && matches!(bytes[index - 1], b' ' | b'\t' | b'\r' | b'\n') {
        index -= 1;
    }
    let mut count = 0;
    while index > 0 && bytes[index - 1] == fence {
        index -= 1;
        count += 1;
    }
    if count < needed {
        return false;
    }
    let mut spaces = 0;
    while index > 0 && bytes[index - 1] == b' ' {
        index -= 1;
        spaces += 1;
        if spaces > 3 {
            return false;
        }
    }
    index == 0 || bytes[index - 1] == b'\n'
}

/// 行首的围栏标记：至多 3 格缩进后跟 `` ``` `` / `~~~` 连续段。
/// 返回 `(字符, 长度)`；够不上围栏时返回 `None`。
pub(super) fn fence_marker(bytes: &[u8]) -> Option<(u8, usize)> {
    let mut index = 0;
    let mut spaces = 0;
    while bytes.get(index) == Some(&b' ') {
        index += 1;
        spaces += 1;
        if spaces > 3 {
            return None;
        }
    }
    let fence = *bytes.get(index)?;
    if fence != b'`' && fence != b'~' {
        return None;
    }
    let start = index;
    while bytes.get(index) == Some(&fence) {
        index += 1;
    }
    let len = index - start;
    (len >= 3).then_some((fence, len))
}

/// `file://` 之后的部分 → 本地路径。
///
/// 按 RFC 8089 处理两种写法：`file:///abs/path`（空 host）与
/// `file://localhost/abs/path`。带其它 host 的是远程文件，本地读不到，直接拒绝。
/// 百分号编码在这里解开——路径里的空格在 markdown 里就是 `%20`。
pub(super) fn file_url_path(rest: &str) -> Option<std::path::PathBuf> {
    let raw = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (host, path) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        format!("/{path}")
    };
    Some(std::path::PathBuf::from(percent_decode(&raw)))
}

/// 解码 `%XX`；非法转义原样保留（宁可找不到文件，也不要静默改路径）。
pub(super) fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high << 4 | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(super) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 本地图片的缓存键：路径 + mtime + 长度。文件被改写就必须重新解码。
///
/// 文件不存在 / 不是普通文件 / 超过解码上限时返回 `None`（回落 alt 文本）。
pub(super) fn file_key(path: &std::path::Path) -> Option<ImageKey> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > super::super::image::decode::MAX_FILE_BYTES {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|elapsed| i128::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0);
    Some(ImageKey::File {
        path: std::sync::Arc::new(path.to_path_buf()),
        mtime_ns,
        len: meta.len(),
    })
}
