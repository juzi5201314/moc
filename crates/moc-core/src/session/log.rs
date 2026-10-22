//! transcript 的物理编码：普通 JSONL 与 `.jsonl.zst` 的 zstd 帧容器。
//!
//! 压缩文件由首尾相接的独立 zstd 帧组成：header 一帧，每次 append 一帧，rewrite 一帧。
//! 帧边界就是提交边界：崩溃只会撕裂最后一帧，读取时忽略它，写租约获取后截掉即可续写。
//! 这里不保存会话元数据、帧索引或压缩上下文，读取始终是完整解码。

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::error::CoreError;

use super::entries::Entry;
use super::store::SessionPaths;

/// zstd 压缩级别；对会话日志的折中值，调整不影响格式兼容。
const LEVEL: i32 = 3;
const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const READ_BUFFER: usize = 128 * 1024;
const FRAME_BUFFER: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Plain,
    Zstd,
}

/// 读取策略：严格模式遇到损坏帧直接报错；列表只读元信息，跳过坏帧继续。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Fail,
    SkipFrame,
}

#[derive(Debug)]
struct FrameScan {
    frames: Vec<Range<u64>>,
    torn_start: Option<u64>,
}

/// 按扩展名判断格式；格式是会话创建时固定的，不做内容嗅探。
pub(crate) fn format(path: &Path) -> Format {
    if path.extension().is_some_and(|extension| extension == "zst") {
        Format::Zstd
    } else {
        Format::Plain
    }
}

pub(crate) fn for_each_line(
    path: &Path,
    policy: Policy,
    on_line: &mut dyn FnMut(&[u8]) -> Result<(), CoreError>,
) -> Result<(), CoreError> {
    match format(path) {
        Format::Plain => for_each_plain_line(path, on_line),
        Format::Zstd => for_each_zstd_line(path, policy, on_line),
    }
}

fn for_each_plain_line(
    path: &Path,
    on_line: &mut dyn FnMut(&[u8]) -> Result<(), CoreError>,
) -> Result<(), CoreError> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(READ_BUFFER, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        // 最后一段没有换行说明是写者追加到一半，忽略；中间行一定以换行结束。
        if line.last() != Some(&b'\n') {
            return Ok(());
        }
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        on_line(&line)?;
    }
}

fn for_each_zstd_line(
    path: &Path,
    policy: Policy,
    on_line: &mut dyn FnMut(&[u8]) -> Result<(), CoreError>,
) -> Result<(), CoreError> {
    let scan = scan_zstd(path)?;
    if scan.frames.is_empty() {
        return Ok(());
    }
    let mut file = File::open(path)?;
    let mut context = zstd::zstd_safe::DCtx::create();
    let mut line = Vec::new();
    for (index, frame) in scan.frames.iter().enumerate() {
        match decode_frame(&mut file, frame, &mut context, &mut line, path, on_line) {
            Ok(()) => {}
            // 回调自己的错误（比如 header 解析失败）不能被当成坏帧吞掉。
            Err(failure) if failure.handler => return Err(failure.error),
            Err(failure) if policy == Policy::SkipFrame => {
                tracing::warn!(
                    path = %path.display(),
                    frame = index,
                    error = %failure.error,
                    "跳过损坏的 transcript 帧"
                );
                // 解码失败的上下文停在帧中间，下一帧必须从干净状态开始。
                context = zstd::zstd_safe::DCtx::create();
                line.clear();
            }
            Err(failure) => return Err(failure.error),
        }
    }
    Ok(())
}

struct FrameFailure {
    error: CoreError,
    handler: bool,
}

impl FrameFailure {
    fn frame(error: CoreError) -> Self {
        Self {
            error,
            handler: false,
        }
    }
}

fn decode_frame(
    file: &mut File,
    frame: &Range<u64>,
    context: &mut zstd::zstd_safe::DCtx<'static>,
    line: &mut Vec<u8>,
    path: &Path,
    on_line: &mut dyn FnMut(&[u8]) -> Result<(), CoreError>,
) -> Result<(), FrameFailure> {
    file.seek(SeekFrom::Start(frame.start))
        .map_err(|error| FrameFailure::frame(error.into()))?;
    let input = BufReader::with_capacity(FRAME_BUFFER, (&mut *file).take(frame.end - frame.start));
    let decoder = zstd::stream::read::Decoder::with_context(input, context);
    let mut output = BufReader::with_capacity(FRAME_BUFFER, decoder);
    loop {
        line.clear();
        let read = output.read_until(b'\n', line).map_err(|error| {
            FrameFailure::frame(CoreError::Session(format!(
                "解压 {} 的 zstd 帧（字节 {}..{}）失败: {error}",
                path.display(),
                frame.start,
                frame.end
            )))
        })?;
        if read == 0 {
            return Ok(());
        }
        if line.last() != Some(&b'\n') {
            return Err(FrameFailure::frame(CoreError::Session(format!(
                "{} 的 zstd 帧（字节 {}..{}）没有以换行结束",
                path.display(),
                frame.start,
                frame.end
            ))));
        }
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        on_line(line).map_err(|error| FrameFailure {
            error,
            handler: true,
        })?;
    }
}

/// 只解析 zstd 帧结构，跳过 payload，得到完整帧范围与不完整尾帧的起点。
fn scan_zstd(path: &Path) -> Result<FrameScan, CoreError> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut frames = Vec::new();
    let mut offset = 0u64;
    while offset < length {
        let start = offset;
        if length - offset < 4 {
            return Ok(torn(frames, start));
        }
        let mut magic = [0u8; 4];
        if read_at(&mut file, offset, &mut magic)? < 4 {
            return Ok(torn(frames, start));
        }
        if u32::from_le_bytes(magic) != ZSTD_MAGIC {
            return Err(corrupt(path, offset, "不是 zstd 帧的 magic"));
        }
        offset += 4;
        // 帧头最多再 13 字节（descriptor + window + dict-id + FCS）。
        let mut header_bytes = [0u8; 14];
        let filled = read_at(&mut file, offset, &mut header_bytes)?;
        if filled == 0 {
            return Ok(torn(frames, start));
        }
        let descriptor = header_bytes[0];
        let fcs_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        if descriptor & 0x18 != 0 {
            return Err(corrupt(path, offset, "帧头保留位非零"));
        }
        let checksum = descriptor & 0x04 != 0;
        let dict_flag = descriptor & 0x03;
        let mut header_len = 1usize;
        if !single_segment {
            header_len += 1; // window descriptor
        }
        header_len += match dict_flag {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        header_len += match fcs_flag {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        if filled < header_len {
            return Ok(torn(frames, start));
        }
        offset += header_len as u64;
        loop {
            if length - offset < 3 {
                return Ok(torn(frames, start));
            }
            let mut block = [0u8; 3];
            read_at(&mut file, offset, &mut block)?;
            offset += 3;
            let header = u32::from_le_bytes([block[0], block[1], block[2], 0]);
            let last = header & 1 != 0;
            let block_type = (header >> 1) & 0x03;
            let block_size = u64::from(header >> 3);
            let payload = match block_type {
                0 | 2 => block_size,
                1 => 1,
                _ => return Err(corrupt(path, offset - 3, "保留的 zstd 块类型")),
            };
            if length - offset < payload {
                return Ok(torn(frames, start));
            }
            offset += payload;
            if last {
                break;
            }
        }
        if checksum {
            if length - offset < 4 {
                return Ok(torn(frames, start));
            }
            offset += 4;
        }
        if offset <= start {
            return Err(corrupt(path, start, "zstd 帧长度为零"));
        }
        frames.push(start..offset);
    }
    Ok(FrameScan {
        torn_start: None,
        frames,
    })
}

fn torn(frames: Vec<Range<u64>>, start: u64) -> FrameScan {
    FrameScan {
        torn_start: Some(start),
        frames,
    }
}

fn corrupt(path: &Path, offset: u64, reason: &str) -> CoreError {
    CoreError::Session(format!(
        "transcript {} 在字节 {offset} 处损坏：{reason}",
        path.display()
    ))
}

fn read_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
    file.seek(SeekFrom::Start(offset))?;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

thread_local! {
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
}

/// 把完整的一行（含换行）压成一个独立帧；复用线程局部的压缩上下文。
pub(crate) fn compress_frame(line: &[u8]) -> Result<Vec<u8>, CoreError> {
    COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut compressor = zstd::bulk::Compressor::new(LEVEL)?;
            compressor.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
            *slot = Some(compressor);
        }
        let compressor = slot.as_mut().expect("刚刚初始化过压缩上下文");
        Ok(compressor.compress(line)?)
    })
}

/// 把 entries 压成一个完整的 zstd 帧；rewrite 用它一次性写临时文件。
pub(crate) fn encode_zstd_entries(entries: &[Entry]) -> Result<Vec<u8>, CoreError> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), LEVEL)?;
    encoder.include_checksum(true)?;
    for entry in entries {
        serde_json::to_writer(&mut encoder, entry).map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        encoder.write_all(b"\n")?;
    }
    Ok(encoder.finish()?)
}

/// 截掉不完整的最后一帧；返回是否发生截断。
pub(crate) fn truncate_torn_tail(path: &Path) -> Result<bool, CoreError> {
    let scan = scan_zstd(path)?;
    let Some(start) = scan.torn_start else {
        return Ok(false);
    };
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(start)?;
    file.sync_data()?;
    Ok(true)
}

/// `history://transcript` 对外的路径：明文直接给出源文件，压缩会话给出解码镜像。
pub(crate) fn history_view_path(paths: &SessionPaths) -> PathBuf {
    if paths.compressed() {
        paths.sidecar.join("history").join("transcript.jsonl")
    } else {
        paths.transcript.clone()
    }
}

/// 刷新压缩会话的解码镜像；返回源文件是否存在。
///
/// 明文会话不需要镜像。镜像按源文件的 dev/ino/size/mtime/ctime 失效；解码期间源发生
/// 变化时只写解码前的 stamp，下次访问会再刷新一次。
pub(crate) fn refresh_history_view(paths: &SessionPaths) -> Result<bool, CoreError> {
    if !paths.compressed() {
        return Ok(paths.transcript.exists());
    }
    let mirror = history_view_path(paths);
    let stamp_path = mirror.with_extension("stamp");
    let source = &paths.transcript;
    if !source.exists() {
        // 源被删除或移走时不要继续暴露旧镜像，让读取方看到不存在的明确结果。
        let _ = fs::remove_file(&mirror);
        let _ = fs::remove_file(&stamp_path);
        return Ok(false);
    }
    let permissions = fs::metadata(source)?.permissions();
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        // 镜像只承载数据，不继承执行位；创建时就不得比源文件更可见。
        fs::Permissions::from_mode(permissions.mode() & 0o666)
    };
    let before = snapshot_stamp(source)?;
    if mirror.exists()
        && let Ok(stamp) = fs::read_to_string(&stamp_path)
        && stamp == before
    {
        fs::set_permissions(&mirror, permissions)?;
        return Ok(true);
    }
    if let Some(parent) = mirror.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = mirror.with_extension(format!("jsonl.{}.tmp", crate::ids::entry_id()));
    let result = (|| -> Result<(), CoreError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(permissions.mode());
        }
        let out = options.open(&temp)?;
        out.set_permissions(permissions)?;
        let mut out = BufWriter::with_capacity(READ_BUFFER, out);
        for_each_line(source, Policy::Fail, &mut |line| {
            out.write_all(line)?;
            out.write_all(b"\n")?;
            Ok(())
        })?;
        out.flush()?;
        out.get_ref().sync_all()?;
        drop(out);
        fs::rename(&temp, &mirror)?;
        let after = snapshot_stamp(source)?;
        let stamp = if after == before { after } else { before };
        fs::write(&stamp_path, stamp)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
        let _ = fs::remove_file(&mirror);
        let _ = fs::remove_file(&stamp_path);
    }
    result?;
    Ok(true)
}

fn snapshot_stamp(path: &Path) -> Result<String, CoreError> {
    let metadata = fs::metadata(path)?;
    let id = crate::hashline::SnapshotId::from_metadata(&metadata);
    Ok(format!(
        "{}:{}:{}:{}.{}:{}.{}",
        id.device,
        id.inode,
        id.size,
        id.modified_secs,
        id.modified_nanos,
        id.changed_secs,
        id.changed_nanos
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(line: &str) -> Vec<u8> {
        compress_frame(line.as_bytes()).expect("压缩测试帧")
    }

    fn collect(path: &Path, policy: Policy) -> Result<Vec<String>, CoreError> {
        let mut lines = Vec::new();
        for_each_line(path, policy, &mut |line| {
            lines.push(String::from_utf8(line.to_vec()).expect("测试行是 UTF-8"));
            Ok(())
        })?;
        Ok(lines)
    }

    #[test]
    fn frame_container_ignores_a_torn_tail_and_keeps_complete_frames() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("ses_1.jsonl.zst");
        let first = frame("{\"a\":1}\n");
        let second = frame("{\"a\":2}\n");
        let mut committed = first.clone();
        committed.extend(&second);
        fs::write(&path, &committed).expect("写入两帧");
        assert_eq!(
            collect(&path, Policy::Fail).unwrap(),
            ["{\"a\":1}", "{\"a\":2}"]
        );

        let third = frame("{\"a\":3}\n");
        let mut torn = committed.clone();
        torn.extend(&third[..third.len() / 2]);
        fs::write(&path, &torn).expect("写入半帧");
        assert_eq!(
            collect(&path, Policy::Fail).unwrap(),
            ["{\"a\":1}", "{\"a\":2}"],
            "不完整的最后一帧应被忽略"
        );
        assert!(truncate_torn_tail(&path).unwrap());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            committed.len() as u64,
            "截断只丢掉半帧"
        );
        let mut continued = committed.clone();
        continued.extend(&third);
        fs::write(&path, &continued).expect("续写完整帧");
        assert_eq!(
            collect(&path, Policy::Fail).unwrap(),
            ["{\"a\":1}", "{\"a\":2}", "{\"a\":3}"]
        );
    }

    #[test]
    fn a_corrupt_complete_frame_fails_strict_reads_but_is_skipped_for_listing() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("ses_1.jsonl.zst");
        let mut first = frame("{\"a\":1}\n");
        let second = frame("{\"a\":2}\n");
        // 破坏第一帧的 checksum，帧结构仍然完整。
        *first.last_mut().expect("帧非空") ^= 0xff;
        let mut bytes = first;
        bytes.extend(second);
        fs::write(&path, &bytes).expect("写入损坏帧");

        assert!(
            collect(&path, Policy::Fail).is_err(),
            "严格读取必须报告损坏帧"
        );
        assert_eq!(
            collect(&path, Policy::SkipFrame).unwrap(),
            ["{\"a\":2}"],
            "列表读取跳过坏帧并保留后面的元信息"
        );
    }

    #[test]
    fn history_mirror_refreshes_when_the_compressed_source_changes() {
        let directory = tempfile::tempdir().expect("临时目录");
        let paths = SessionPaths {
            transcript: directory.path().join("1_ses_1.jsonl.zst"),
            sidecar: directory.path().join("ses_1"),
        };
        assert!(
            !refresh_history_view(&paths).unwrap(),
            "源不存在时不生成镜像"
        );
        assert!(!history_view_path(&paths).exists(), "源不存在时不生成镜像");

        fs::write(&paths.transcript, frame("{\"a\":1}\n")).expect("写入源");
        assert!(refresh_history_view(&paths).unwrap());
        let mirror = history_view_path(&paths);
        assert_eq!(fs::read_to_string(&mirror).unwrap(), "{\"a\":1}\n");

        let mut two = frame("{\"a\":1}\n");
        two.extend(frame("{\"a\":2}\n"));
        fs::write(&paths.transcript, two).expect("更新源");
        assert!(refresh_history_view(&paths).unwrap());
        assert_eq!(
            fs::read_to_string(&mirror).unwrap(),
            "{\"a\":1}\n{\"a\":2}\n"
        );

        fs::remove_file(&paths.transcript).expect("删除源");
        assert!(!refresh_history_view(&paths).unwrap());
        assert!(!mirror.exists(), "源删除后旧镜像也要移除");
    }

    #[cfg(unix)]
    #[test]
    fn history_mirror_never_exposes_private_source_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("临时目录");
        let paths = SessionPaths {
            transcript: directory.path().join("1_ses_1.jsonl.zst"),
            sidecar: directory.path().join("ses_1"),
        };
        fs::write(&paths.transcript, frame("{\"secret\":1}\n")).expect("写入源");
        fs::set_permissions(&paths.transcript, fs::Permissions::from_mode(0o600))
            .expect("限制源权限");
        assert!(refresh_history_view(&paths).expect("生成镜像"));
        let mirror = history_view_path(&paths);
        assert_eq!(
            fs::metadata(&mirror).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // 已发布镜像也不能通过缓存命中保留被外部放宽的权限。
        fs::set_permissions(&mirror, fs::Permissions::from_mode(0o644)).expect("模拟旧版本镜像");
        assert!(refresh_history_view(&paths).expect("访问缓存镜像"));
        assert_eq!(
            fs::metadata(&mirror).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn plaintext_sessions_need_no_mirror() {
        let directory = tempfile::tempdir().expect("临时目录");
        let paths = SessionPaths {
            transcript: directory.path().join("1_ses_1.jsonl"),
            sidecar: directory.path().join("ses_1"),
        };
        assert_eq!(history_view_path(&paths), paths.transcript);
        assert!(!refresh_history_view(&paths).unwrap());
        fs::write(&paths.transcript, "{\"a\":1}\n").expect("写入明文");
        assert!(refresh_history_view(&paths).unwrap());
        assert_eq!(
            fs::read_to_string(history_view_path(&paths)).unwrap(),
            "{\"a\":1}\n"
        );
    }
}
