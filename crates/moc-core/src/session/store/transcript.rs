use super::super::entries::Entry;
use super::super::log;
use super::INLINE_COMPRESS_LIMIT;
use super::SessionFile;
use crate::error::CoreError;
use parking_lot::RwLock;
use std::path::Path;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

impl SessionFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(in super::super) fn revision(&self) -> crate::hashline::SnapshotId {
        *self.revision.read()
    }

    /// 在目标路径创建 transcript 文件，并把 header 写成第一行。
    ///
    /// 供延迟写盘使用：目录按需创建，header 先写临时文件再 rename 发布，崩溃不会留下
    /// 打不开的半条 header。同 id 并发创建由目标已存在的检查拒绝（上层互斥锁会避免）。
    pub async fn create_with_header(path: &Path, header: &Entry) -> Result<Self, CoreError> {
        let parent = path
            .parent()
            .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
        fs::create_dir_all(parent).await?;
        if fs::try_exists(path).await? {
            return Err(CoreError::Session(format!(
                "transcript 已存在：{}",
                path.display()
            )));
        }
        let format = log::format(path);
        let mut line = header.to_json_vec().map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        line.push(b'\n');
        let payload = match format {
            log::Format::Plain => line,
            log::Format::Zstd => log::compress_frame(&line)?,
        };
        let temp = path.with_extension(format!("jsonl.{}.tmp", crate::ids::entry_id()));
        let result = async {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await?;
            file.write_all(&payload).await?;
            file.flush().await?;
            file.sync_data().await?;
            drop(file);
            fs::rename(&temp, path).await?;
            Ok::<(), CoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp).await;
        }
        result?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .await?;
        let revision = crate::hashline::SnapshotId::from_metadata(&file.metadata().await?);
        Ok(Self {
            path: path.to_owned(),
            format,
            file: tokio::sync::Mutex::new(Some(file)),
            revision: RwLock::new(revision),
        })
    }

    pub async fn changed_on_disk(&self) -> Result<bool, CoreError> {
        let metadata = match fs::metadata(&self.path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.into()),
        };
        Ok(*self.revision.read() != crate::hashline::SnapshotId::from_metadata(&metadata))
    }

    /// 重新打开磁盘上的 transcript。
    ///
    /// 外部进程可能已经追加过记录；重新打开后 revision 以磁盘为准。格式在会话创建时
    /// 固定，这里不重新判定。调用者要持有会话文件锁。
    pub async fn reopen(&self) -> Result<(), CoreError> {
        let mut current = self.file.lock().await;
        let reopened = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&reopened.metadata().await?);
        *current = Some(reopened);
        Ok(())
    }

    /// 追加一条完整记录并 flush；磁盘文件不存在时先用 header 重建。
    ///
    /// 写入或刷写失败时把文件截回写入前的长度：调用方会重试同一个批次，留下半条记录
    /// 会让下一次追加粘到残尾，产生无法解析的中间行。压缩会话的每条记录是一个独立帧，
    /// 帧边界就是回滚边界。
    pub async fn append(&self, entry: &Entry, header: Option<&Entry>) -> Result<(), CoreError> {
        let mut line = entry.to_json_vec().map_err(|error| {
            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
        })?;
        line.push(b'\n');
        let payload = match self.format {
            log::Format::Plain => line,
            log::Format::Zstd if line.len() <= INLINE_COMPRESS_LIMIT => log::compress_frame(&line)?,
            log::Format::Zstd => tokio::task::spawn_blocking(move || log::compress_frame(&line))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))??,
        };
        let mut file = self.file.lock().await;
        if file.is_none() {
            let parent = self
                .path
                .parent()
                .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
            fs::create_dir_all(parent).await?;
            let created = OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(&self.path)
                .await?;
            *file = Some(created);
            if let Some(header) = header {
                let mut header_line = header.to_json_vec().map_err(|error| {
                    CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
                })?;
                header_line.push(b'\n');
                let header_payload = match self.format {
                    log::Format::Plain => header_line,
                    log::Format::Zstd => log::compress_frame(&header_line)?,
                };
                file.as_mut()
                    .expect("刚创建的句柄")
                    .write_all(&header_payload)
                    .await?;
            }
        }
        let handle = file.as_mut().expect("延迟创建之后一定已有句柄");
        let before = handle.metadata().await?.len();
        if let Err(error) = async {
            handle.write_all(&payload).await?;
            handle.flush().await?;
            Ok::<(), std::io::Error>(())
        }
        .await
        {
            handle.set_len(before).await?;
            handle.flush().await?;
            return Err(error.into());
        }
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&handle.metadata().await?);
        Ok(())
    }

    /// 截掉最后一个不完整记录，返回是否发生截断。
    ///
    /// 只在调用方持有写租约时使用：崩溃可能留下半条记录，读取时忽略它并不代表文件可以
    /// 继续追加，残留字节会让下一条记录粘成坏行。明文丢掉最后一个换行之后的字节，
    /// 压缩会话丢掉不完整的最后一帧。
    pub async fn truncate_partial_tail(&self) -> Result<bool, CoreError> {
        const SCAN_CHUNK: u64 = 64 * 1024;
        let mut file = self.file.lock().await;
        let Some(handle) = file.as_mut() else {
            return Ok(false);
        };
        let length = handle.metadata().await?.len();
        if length == 0 {
            return Ok(false);
        }
        if self.format == log::Format::Zstd {
            let path = self.path.clone();
            let truncated = tokio::task::spawn_blocking(move || log::truncate_torn_tail(&path))
                .await
                .map_err(|error| CoreError::Session(error.to_string()))??;
            if truncated {
                *self.revision.write() =
                    crate::hashline::SnapshotId::from_metadata(&handle.metadata().await?);
            }
            return Ok(truncated);
        }
        let mut reader = handle.try_clone().await?;
        let mut end = length;
        let newline = loop {
            let start = end.saturating_sub(SCAN_CHUNK);
            let mut chunk =
                vec![0u8; usize::try_from(end - start).expect("块大小可落入 usize")];
            tokio::io::AsyncSeekExt::seek(&mut reader, std::io::SeekFrom::Start(start)).await?;
            tokio::io::AsyncReadExt::read_exact(&mut reader, &mut chunk).await?;
            if let Some(offset) = chunk.iter().rposition(|byte| *byte == b'\n') {
                break Some(start + offset as u64);
            }
            if start == 0 {
                break None;
            }
            end = start;
        };
        let keep = newline.map_or(0, |offset| offset + 1);
        if keep == length {
            return Ok(false);
        }
        handle.set_len(keep).await?;
        handle.flush().await?;
        *self.revision.write() =
            crate::hashline::SnapshotId::from_metadata(&handle.metadata().await?);
        Ok(true)
    }

    /// 原子替换整个 transcript；rename 前先刷写并 fsync 临时文件。
    /// 磁盘文件不存在时同样适用：整份 entries 通过临时文件 rename 发布，格式保持不变。
    pub async fn rewrite(&self, entries: &[Entry]) -> Result<(), CoreError> {
        let mut current = self.file.lock().await;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| CoreError::Session("transcript 路径缺少父目录".into()))?;
        fs::create_dir_all(parent).await?;
        let temp = self
            .path
            .with_extension(format!("jsonl.{}.tmp", crate::ids::entry_id()));
        let result = async {
            let mut replacement = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await?;
            match self.format {
                log::Format::Plain => {
                    for entry in entries {
                        let line = entry.to_json_vec().map_err(|error| {
                            CoreError::Session(format!("序列化 transcript entry 失败: {error}"))
                        })?;
                        replacement.write_all(&line).await?;
                        replacement.write_all(b"\n").await?;
                    }
                }
                log::Format::Zstd => {
                    // rewrite 是少见操作：在异步线程压成一份小缓冲，避免为 spawn_blocking
                    // 再复制整份 entries；普通 append 已经按大小分流。
                    let encoded = log::encode_zstd_entries(entries)?;
                    replacement.write_all(&encoded).await?;
                }
            }
            replacement.flush().await?;
            replacement.sync_all().await?;
            let reopened = OpenOptions::new()
                .read(true)
                .append(true)
                .open(&temp)
                .await?;
            drop(replacement);
            fs::rename(&temp, &self.path).await?;
            *current = Some(reopened);
            *self.revision.write() = crate::hashline::SnapshotId::from_metadata(
                &current
                    .as_ref()
                    .expect("rename 之后一定已有句柄")
                    .metadata()
                    .await?,
            );
            Ok::<(), CoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp).await;
        }
        result
    }
}

/// 逐条解析日志，保留行号与压缩帧的错误语义。
pub(super) fn read_entries_sync(
    path: &Path,
    on_entry: &mut dyn FnMut(Entry) -> Result<(), CoreError>,
) -> Result<(), CoreError> {
    let mut line_number = 0usize;
    log::for_each_line(path, &mut |line| {
        line_number += 1;
        let entry = serde_json::from_slice(line).map_err(|error| {
            CoreError::Session(format!(
                "解析 {} 第 {line_number} 行失败: {error}",
                path.display()
            ))
        })?;
        on_entry(entry)
    })
}
