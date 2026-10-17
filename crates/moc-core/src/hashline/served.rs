//! 已服务窗口到当前文件内容的安全校验。

use std::path::Path;

use thiserror::Error;
use xxhash_rust::xxh32::xxh32;

use crate::hashline::{AnchorParse, FileSnapshot, LineEnding, REBASE_WINDOW, ServedFile, id_of};

/// 验证 edit range 的机器可读错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeError {
    #[error(
        "{field} must be an anchor: the line number plus the two-letter id as shown by read, for example \"42sr\" (bare, or an entire copied line \"42sr│text\"). Received \"{received}\"."
    )]
    BadAnchor {
        field: &'static str,
        received: String,
    },
    #[error("{field} \"{anchor}\" no longer matches any served line within the rebase window")]
    Stale { field: &'static str, anchor: String },
    #[error("{kind} was not shown by read")]
    Unserved {
        kind: &'static str,
        line_no: usize,
        unserved: Vec<usize>,
    },
    #[error("one anchored range may contain at most 100000 lines")]
    TooLarge,
}

/// 当前磁盘文件的 UTF-8 视图、行尾、BOM、行摘要以及文件 epoch。
pub fn read_snapshot(path: &Path, max_file_bytes: u64) -> Result<FileSnapshot, std::io::Error> {
    use std::io::ErrorKind;
    use std::io::Read;

    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_file_bytes {
        return Err(std::io::Error::new(
            ErrorKind::FileTooLarge,
            "E_TOO_LARGE: file exceeds the read limit",
        ));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| {
        std::io::Error::new(ErrorKind::FileTooLarge, "E_TOO_LARGE: file is too large")
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let byte_count = u64::try_from(bytes.len()).expect("Vec length must fit in u64");
    if byte_count > max_file_bytes {
        return Err(std::io::Error::new(
            ErrorKind::FileTooLarge,
            format!(
                "E_TOO_LARGE: {} bytes exceeds {max_file_bytes}",
                bytes.len()
            ),
        ));
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(std::io::Error::new(ErrorKind::InvalidData, "E_BINARY"));
    }
    let identity = crate::hashline::SnapshotId::from_metadata_and_bytes(&metadata, &bytes);
    let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
    let content = if bom { &bytes[3..] } else { &bytes[..] };
    let line_ending = if content.windows(2).any(|window| window == b"\r\n") {
        LineEnding::CrLf
    } else if content.contains(&b'\r') {
        LineEnding::Cr
    } else {
        LineEnding::Lf
    };
    let utf8 = String::from_utf8_lossy(content);
    let normalized = utf8.replace("\r\n", "\n").replace('\r', "\n");
    let lines = split_file_lines(&normalized);
    let canons = lines.iter().map(|line| xxh32(line.as_bytes(), 0)).collect();
    Ok(FileSnapshot {
        bytes,
        lines,
        canons,
        epoch: identity,
        bom,
        line_ending,
    })
}

pub fn split_file_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// 已解析锚点在当前快照中的位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub position: usize,
    pub moved: bool,
    pub delta: isize,
}

/// 将锚点定位到当前文件行，并允许会话内编辑造成的小范围位移。
pub fn locate(
    served: &ServedFile,
    snapshot: &FileSnapshot,
    parsed: &AnchorParse,
) -> Result<Located, RangeError> {
    let position = parsed.line - 1;
    if position >= snapshot.lines.len() {
        return Err(RangeError::Stale {
            field: "anchor",
            anchor: format!("{}{}", parsed.line, String::from_utf8_lossy(&parsed.id)),
        });
    }
    if served.served.get(position).copied().unwrap_or(false)
        && id_of(parsed.line, &snapshot.lines[position]) == parsed.id
    {
        return Ok(Located {
            position,
            moved: false,
            delta: 0,
        });
    }

    let first = parsed.line.saturating_sub(REBASE_WINDOW).max(1);
    let last = parsed
        .line
        .saturating_add(REBASE_WINDOW)
        .min(snapshot.lines.len());
    let mut candidates = Vec::new();
    for candidate in first..=last {
        let index = candidate - 1;
        if id_of(candidate, &snapshot.lines[index]) == parsed.id {
            candidates.push((
                candidate,
                served.served.get(index).copied().unwrap_or(false),
            ));
        }
    }
    if let [(candidate, true)] = candidates.as_slice() {
        let candidate = *candidate;
        let delta = isize::try_from(candidate).expect("line count must fit in isize")
            - isize::try_from(parsed.line).expect("line count must fit in isize");
        return Ok(Located {
            position: candidate - 1,
            moved: true,
            delta,
        });
    }
    let unserved: Vec<usize> = candidates
        .iter()
        .filter_map(|(candidate, is_served)| (!*is_served).then_some(*candidate))
        .collect();
    if candidates.iter().all(|(_, is_served)| !*is_served) && !unserved.is_empty() {
        return Err(RangeError::Unserved {
            kind: "anchor line",
            line_no: parsed.line,
            unserved,
        });
    }
    Err(RangeError::Stale {
        field: "anchor",
        anchor: format!("{}{}", parsed.line, String::from_utf8_lossy(&parsed.id)),
    })
}

/// 校验包含两端锚点的编辑范围。
pub fn verify_range(
    served: &ServedFile,
    snapshot: &FileSnapshot,
    from: &str,
    to: &str,
) -> Result<(usize, usize, Option<String>), RangeError> {
    let parsed_from = parse_or_error("from", from)?;
    let parsed_to = parse_or_error("to", to)?;
    let start =
        locate(served, snapshot, &parsed_from).map_err(|error| with_field(error, "from", from))?;
    let end = locate(served, snapshot, &parsed_to).map_err(|error| with_field(error, "to", to))?;
    if start.moved && end.moved && start.delta != end.delta {
        return Err(RangeError::Stale {
            field: "to",
            anchor: to.to_owned(),
        });
    }
    let (start_position, end_position, warning) = if start.position <= end.position {
        (start.position, end.position, None)
    } else {
        (
            end.position,
            start.position,
            Some("E_REVERSED_ANCHORS: anchors were reversed and have been swapped".to_owned()),
        )
    };
    if end_position.saturating_sub(start_position) >= 100_000 {
        return Err(RangeError::TooLarge);
    }
    let unserved: Vec<usize> = (start_position..=end_position)
        .filter_map(|position| {
            (!served.served.get(position).copied().unwrap_or(false)).then_some(position + 1)
        })
        .collect();
    if !unserved.is_empty() {
        return Err(RangeError::Unserved {
            kind: "range line",
            line_no: unserved[0],
            unserved,
        });
    }
    Ok((start_position, end_position, warning))
}

fn parse_or_error(field: &'static str, raw: &str) -> Result<AnchorParse, RangeError> {
    parse_anchor(raw).map_err(|received| RangeError::BadAnchor { field, received })
}

fn with_field(error: RangeError, field: &'static str, raw: &str) -> RangeError {
    match error {
        RangeError::Stale { .. } => RangeError::Stale {
            field,
            anchor: raw.to_owned(),
        },
        RangeError::Unserved {
            kind,
            line_no,
            unserved,
        } => RangeError::Unserved {
            kind,
            line_no,
            unserved,
        },
        other => other,
    }
}

use crate::hashline::parse_anchor;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::{ServedFile, anchor_token};

    #[test]
    fn preserves_line_endings_and_rejects_unserved_ranges() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"one\r\ntwo\r\nthree\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 1024).expect("读取文件");
        assert_eq!(snapshot.lines, ["one", "two", "three"]);
        assert_eq!(snapshot.line_ending, LineEnding::CrLf);
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, snapshot.lines.len(), &[0..1]);
        let error = verify_range(
            &served,
            &snapshot,
            &anchor_token(1, &snapshot.lines[0]),
            &anchor_token(3, &snapshot.lines[2]),
        )
        .expect_err("未服务尾部边界必须拒绝");
        assert!(matches!(error, RangeError::Unserved { .. }));
    }

    #[test]
    fn full_served_range_accepts_reverse_order_and_warns() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"one\ntwo\nthree").expect("写文件");
        let snapshot = read_snapshot(&path, 1024).expect("读取文件");
        let mut served = ServedFile::new();
        served.serve(
            snapshot.epoch,
            snapshot.lines.len(),
            &[0..snapshot.lines.len()],
        );
        let (start, end, warning) = verify_range(
            &served,
            &snapshot,
            &anchor_token(3, &snapshot.lines[2]),
            &anchor_token(1, &snapshot.lines[0]),
        )
        .expect("锚点逆序自动交换");
        assert_eq!((start, end), (0, 2));
        assert!(
            warning
                .expect("逆序应有警告")
                .contains("E_REVERSED_ANCHORS")
        );
    }
}
