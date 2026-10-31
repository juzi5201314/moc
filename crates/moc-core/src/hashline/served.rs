//! 磁盘文件到行视图快照的读取。

use std::path::Path;

use crate::hashline::{FileSnapshot, LineEnding};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_line_endings() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"\xef\xbb\xbfone\r\ntwo\r\nthree\r\n").expect("写文件");
        let snapshot = read_snapshot(&path, 4096).expect("读取文件");
        assert_eq!(snapshot.lines, ["one", "two", "three"]);
        assert_eq!(snapshot.line_ending, LineEnding::CrLf);
        assert!(snapshot.bom);
    }
}
