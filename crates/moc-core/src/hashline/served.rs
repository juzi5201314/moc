//! 已服务锚点令牌到当前文件内容的安全校验。

use std::path::Path;

use thiserror::Error;

use crate::hashline::{AnchorParse, FileSnapshot, LineEnding, ServedFile, id_str};

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
    #[error("{field} \"{anchor}\" no longer matches any served line")]
    Stale { field: &'static str, anchor: String },
    #[error("{kind} {line_no} was not shown by read")]
    Unserved { kind: &'static str, line_no: usize },
    #[error("one anchored range may contain at most 100000 lines")]
    TooLarge,
}

/// 当前磁盘文件的 UTF-8 视图、行尾、BOM 以及文件 epoch。
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
    Ok(FileSnapshot {
        bytes,
        lines,
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

/// 将锚点定位到当前文件行。
///
/// 锚点是会话内稳定的完整令牌（展示行号 + id）：按令牌精确查找，行号位移
/// 不影响匹配；同一 token 出现多次（理论上被分配逻辑禁止）时报 Stale，
/// 绝不静默选一行。
pub fn locate(
    served: &ServedFile,
    snapshot: &FileSnapshot,
    parsed: &AnchorParse,
) -> Result<Located, RangeError> {
    let mut found = None;
    for (position, entry) in served.entries.iter().enumerate() {
        if let Some(token) = entry
            && token.line == parsed.line
            && token.id == parsed.id
        {
            if found.is_some() {
                return Err(stale_error(parsed));
            }
            found = Some(position);
        }
    }
    let Some(position) = found else {
        if parsed.line <= snapshot.lines.len() && !served.is_served(parsed.line - 1) {
            return Err(RangeError::Unserved {
                kind: "anchor line",
                line_no: parsed.line,
            });
        }
        return Err(stale_error(parsed));
    };
    let current_line = position + 1;
    let delta = isize::try_from(current_line).expect("line count must fit in isize")
        - isize::try_from(parsed.line).expect("line count must fit in isize");
    Ok(Located {
        position,
        moved: current_line != parsed.line,
        delta,
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
    if let Some(position) =
        (start_position..=end_position).find(|position| !served.is_served(*position))
    {
        return Err(RangeError::Unserved {
            kind: "range line",
            line_no: position + 1,
        });
    }
    Ok((start_position, end_position, warning))
}

fn stale_error(parsed: &AnchorParse) -> RangeError {
    RangeError::Stale {
        field: "anchor",
        anchor: format!("{}{}", parsed.line, id_str(parsed.id)),
    }
}

fn parse_or_error(field: &'static str, raw: &str) -> Result<AnchorParse, RangeError> {
    crate::hashline::parse_anchor(raw).map_err(|received| RangeError::BadAnchor { field, received })
}

fn with_field(error: RangeError, field: &'static str, raw: &str) -> RangeError {
    match error {
        RangeError::Stale { .. } => RangeError::Stale {
            field,
            anchor: raw.to_owned(),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::{ServedFile, anchor_token};

    fn snapshot(path: &Path, text: &[u8]) -> FileSnapshot {
        std::fs::write(path, text).expect("写文件");
        read_snapshot(path, 4096).expect("读取文件")
    }

    #[test]
    fn preserves_line_endings_and_rejects_unserved_ranges() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let snapshot = snapshot(&path, b"one\r\ntwo\r\nthree\r\n");
        assert_eq!(snapshot.lines, ["one", "two", "three"]);
        assert_eq!(snapshot.line_ending, LineEnding::CrLf);
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[0..1, 2..3]);
        let error = verify_range(
            &served,
            &snapshot,
            &anchor_token(1, &snapshot.lines[0]),
            &anchor_token(3, &snapshot.lines[2]),
        )
        .expect_err("未服务中间行必须拒绝");
        assert!(matches!(error, RangeError::Unserved { line_no: 2, .. }));
    }

    #[test]
    fn full_served_range_accepts_reverse_order_and_warns() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let snapshot = snapshot(&path, b"one\ntwo\nthree");
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[0..snapshot.lines.len()]);
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

    #[test]
    fn duplicate_lines_resolve_by_stable_token_after_any_shift() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let snapshot = snapshot(&path, b"end\nend\ntail\n");
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[0..snapshot.lines.len()]);
        let second = served.token(1).expect("第二行令牌");
        assert_eq!(second.line, 2);
        // 在文件顶部插入一行：第二行移到第 3 行。
        served.rebase(&[(0, 0), (0, 1), (3, 1)], 4);
        let shifted = FileSnapshot {
            bytes: b"new\nend\nend\ntail\n".to_vec(),
            lines: ["new", "end", "end", "tail"].map(str::to_owned).to_vec(),
            epoch: snapshot.epoch,
            bom: false,
            line_ending: LineEnding::Lf,
        };
        let (start, end, _) = verify_range(&served, &shifted, &second.text(), &second.text())
            .expect("重复行按令牌定位");
        assert_eq!((start, end), (2, 2), "第二行现在位于 index 2");
    }

    #[test]
    fn token_from_another_epoch_is_stale() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let snapshot = snapshot(&path, b"one\ntwo\n");
        let mut served = ServedFile::new();
        served.serve(snapshot.epoch, &snapshot.lines, &[0..2]);
        let old = served.token(0).expect("旧令牌");
        let mut next_epoch = snapshot.epoch;
        next_epoch.size += 1;
        let changed = ["other", "two"].map(str::to_owned).to_vec();
        served.serve(next_epoch, &changed, &[0..2]);
        let parsed = crate::hashline::parse_anchor(&old.text()).expect("解析旧令牌");
        let error = locate(&served, &snapshot, &parsed).expect_err("旧 epoch 令牌必须失效");
        assert!(matches!(error, RangeError::Stale { .. }));
    }
}
