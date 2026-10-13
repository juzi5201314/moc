//! 已服务锚点到当前文件内容的安全校验。

use std::path::Path;

use thiserror::Error;

use crate::hashline::{
    AnchorError, FileSnapshot, LineEnding, ServedFile, SnapshotId, assign_hashes, canon_hash,
    decode_anchor,
};

/// 验证 edit range 的机器可读错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RangeError {
    #[error("E_BAD_ANCHOR: anchor must be a 3-character base-62 string")]
    BadAnchor,
    #[error("E_STALE_ANCHOR: anchor cannot be located in the current served record")]
    StaleAnchor,
    #[error("E_STALE_RANGE: file changed after read; retry with current anchors")]
    StaleRange,
    #[error("E_UNSERVED_RANGE: {kind} was not shown to the model by the earlier read")]
    Unserved { kind: &'static str },
    #[error("E_RANGE_TOO_LARGE: one anchored range may contain at most 100000 lines")]
    TooLarge,
    #[error("E_ANCHOR_SPACE_EXHAUSTED")]
    AnchorSpaceExhausted,
}

impl From<AnchorError> for RangeError {
    fn from(error: AnchorError) -> Self {
        match error {
            AnchorError::BadAnchor => Self::BadAnchor,
            AnchorError::Exhausted => Self::AnchorSpaceExhausted,
            AnchorError::InvalidIndex | AnchorError::LengthMismatch => Self::StaleRange,
        }
    }
}

/// 当前磁盘文件的 UTF-8 视图、行尾、BOM、锚点以及文件 epoch。
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
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_file_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_file_bytes {
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
    let identity = SnapshotId::from_metadata_and_bytes(&metadata, &bytes);
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
    let hashes = assign_hashes(&lines, &[])
        .map_err(|_| std::io::Error::other("E_ANCHOR_SPACE_EXHAUSTED"))?;
    let canons = lines.iter().map(|line| canon_hash(line)).collect();
    Ok(FileSnapshot {
        bytes,
        lines,
        hashes,
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

/// 将未服务、损坏或漂移的锚点转为当前行位置。
pub fn verify_range(
    served: &ServedFile,
    snapshot: &FileSnapshot,
    from: &str,
    to: &str,
) -> Result<(usize, usize, Option<String>), RangeError> {
    let from_hash = decode_anchor(from)?;
    let to_hash = decode_anchor(to)?;
    if served
        .retired
        .iter()
        .any(|&(hash, _)| hash == from_hash || hash == to_hash)
    {
        return Err(RangeError::StaleRange);
    }
    if served.epoch != Some(snapshot.epoch) {
        return Err(RangeError::Unserved {
            kind: "file content changed; current file",
        });
    }
    let Some(from_position) = find_served(served, snapshot, from_hash) else {
        return Err(RangeError::Unserved {
            kind: "start anchor",
        });
    };
    let Some(to_position) = find_served(served, snapshot, to_hash) else {
        return Err(RangeError::Unserved {
            kind: "end anchor",
        });
    };
    let (start, end, warning) = if from_position <= to_position {
        (from_position, to_position, None)
    } else {
        (
            to_position,
            from_position,
            Some("E_REVERSED_ANCHORS: anchors were reversed and have been swapped".to_owned()),
        )
    };
    if end.saturating_sub(start) >= 100_000 {
        return Err(RangeError::TooLarge);
    }
    for position in start..=end {
        if served.hashes.get(position).copied().flatten().is_none() {
            return Err(RangeError::Unserved {
                kind: "unserved line inside range",
            });
        }
        let expected = served.canons.get(position).copied().unwrap_or_default();
        if expected != snapshot.canons[position] {
            return Err(RangeError::StaleRange);
        }
    }
    Ok((start, end, warning))
}

fn find_served(served: &ServedFile, snapshot: &FileSnapshot, hash: u32) -> Option<usize> {
    served
        .hashes
        .iter()
        .enumerate()
        .filter_map(|(position, existing)| (*existing == Some(hash)).then_some(position))
        .find(|position| {
            served.canons.get(*position).copied().unwrap_or_default() == snapshot.canons[*position]
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::{ServedFile, encode_anchor};

    #[test]
    fn preserves_line_endings_and_rejects_unserved_ranges() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"one\r\ntwo\r\nthree\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 1024).expect("读取文件");
        assert_eq!(snapshot.lines, ["one", "two", "three"]);
        assert_eq!(snapshot.line_ending, LineEnding::CrLf);
        let mut served = ServedFile::new();
        served.record_served(&[(0, snapshot.hashes[0])], &snapshot.lines, &snapshot);
        let error = verify_range(
            &served,
            &snapshot,
            &encode_anchor(snapshot.hashes[0]).unwrap(),
            &encode_anchor(snapshot.hashes[2]).unwrap(),
        )
        .expect_err("未服务尾部边界必须拒绝");
        assert!(matches!(
            error,
            RangeError::Unserved {
                kind: "end anchor"
            }
        ));
    }

    #[test]
    fn full_served_range_accepts_reverse_order_and_warns() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"one\ntwo\nthree").expect("写文件");
        let snapshot = read_snapshot(&path, 1024).expect("读取文件");
        let mut served = ServedFile::new();
        served.record_served(
            &(0..snapshot.lines.len())
                .map(|i| (i, snapshot.hashes[i]))
                .collect::<Vec<_>>(),
            &snapshot.lines,
            &snapshot,
        );
        let (start, end, warning) = verify_range(
            &served,
            &snapshot,
            &encode_anchor(snapshot.hashes[2]).unwrap(),
            &encode_anchor(snapshot.hashes[0]).unwrap(),
        )
        .expect("锚点逆序自动交换");
        assert_eq!((start, end), (0, 2));
        assert!(warning.unwrap().contains("E_REVERSED_ANCHORS"));
    }
}
