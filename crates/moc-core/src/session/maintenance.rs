//! 存储维护独立于上下文压缩：先预览，用户确认后才归档和清理未引用产物。

use super::{
    SessionTable,
    lock::SessionLockGuard,
    store::{SessionPaths, now_ms},
};
use crate::{CoreError, config::RetentionConfig, hashline::SnapshotId};
use async_compression::tokio::bufread::{GzipDecoder, GzipEncoder};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) fn journal_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".jsonl") {
        Some(path.to_owned())
    } else if name.ends_with(".jsonl.gz") {
        Some(path.with_extension(""))
    } else {
        None
    }
}

pub(crate) async fn reader(path: &Path) -> Result<Pin<Box<dyn AsyncBufRead + Send>>, CoreError> {
    if path.extension().is_some_and(|ext| ext == "gz") {
        return Ok(Box::pin(BufReader::new(GzipDecoder::new(BufReader::new(
            File::open(path).await?,
        )))));
    }
    match File::open(path).await {
        Ok(file) => Ok(Box::pin(BufReader::new(file))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = File::open(path.with_extension("jsonl.gz")).await?;
            Ok(Box::pin(BufReader::new(GzipDecoder::new(BufReader::new(
                file,
            )))))
        }
        Err(error) => Err(error.into()),
    }
}

/// 调用者持会话文件锁；解压完整且 fsync 后才发布可写日志。
pub(crate) async fn restore(path: &Path) -> Result<(), CoreError> {
    if fs::try_exists(path).await? {
        return Ok(());
    }
    let archive = path.with_extension("jsonl.gz");
    let mut input = reader(&archive).await?;
    let temp = path.with_extension(format!("{}.tmp", crate::ids::entry_id()));
    let result = async {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        tokio::io::copy(&mut input, &mut out).await?;
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        fs::rename(&temp, path).await?;
        fs::remove_file(&archive).await?;
        Ok::<_, CoreError>(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

async fn archive(path: &Path) -> Result<(), CoreError> {
    let destination = path.with_extension("jsonl.gz");
    let temp = path.with_extension(format!("{}.tmp", crate::ids::entry_id()));
    let result = async {
        let file = File::open(path).await?;
        let mut input =
            GzipEncoder::with_quality(BufReader::new(file), async_compression::Level::Fastest);
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        tokio::io::copy(&mut input, &mut out).await?;
        out.flush().await?;
        out.sync_all().await?;
        drop(out);
        fs::rename(&temp, &destination).await?;
        fs::remove_file(path).await?;
        Ok::<_, CoreError>(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

struct Candidate {
    session: String,
    paths: SessionPaths,
    path: PathBuf,
    revision: SnapshotId,
    bytes: u64,
}

impl SessionTable {
    pub async fn maintenance(
        &self,
        policy: &RetentionConfig,
        apply: bool,
    ) -> Result<moc_protocol::MaintenanceReport, CoreError> {
        let mut report = moc_protocol::MaintenanceReport {
            applied: apply,
            ..Default::default()
        };
        if !policy.enabled {
            return Ok(report);
        }
        let sessions = self.store.list().await?;
        let now = now_ms();
        let mut per_workspace = HashMap::<&str, usize>::new();
        let mut archives = Vec::new();
        let mut files = Vec::new();
        for (index, session) in sessions.iter().enumerate() {
            let count = per_workspace.entry(&session.cwd).or_default();
            let protected =
                index < policy.retain_newest_global || *count < policy.retain_newest_per_workspace;
            *count += 1;
            if protected || self.records.read().contains_key(&session.id) || session.lock.is_some()
            {
                report.protected += 1;
                continue;
            }
            let Some(paths) = self.store.find(&session.id).await? else {
                continue;
            };
            if now.saturating_sub(session.updated_at_ms)
                >= policy.archive_after_days.saturating_mul(86_400_000)
                && let Ok(metadata) = fs::metadata(&paths.transcript).await
            {
                archives.push(Candidate {
                    session: session.id.clone(),
                    paths: paths.clone(),
                    path: paths.transcript.clone(),
                    revision: SnapshotId::from_metadata(&metadata),
                    bytes: metadata.len(),
                });
            }
            for (directory, days) in [
                (paths.artifacts(), policy.artifacts_days),
                (paths.jobs(), policy.logs_days),
            ] {
                let mut entries = match fs::read_dir(directory).await {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                while let Some(entry) = entries.next_entry().await? {
                    if !entry.file_type().await?.is_file() {
                        continue;
                    }
                    let metadata = entry.metadata().await?;
                    let age = metadata.modified()?.elapsed().unwrap_or_default().as_secs();
                    if age < days.saturating_mul(86_400) {
                        continue;
                    }
                    files.push(Candidate {
                        session: session.id.clone(),
                        paths: paths.clone(),
                        path: entry.path(),
                        revision: SnapshotId::from_metadata(&metadata),
                        bytes: metadata.len(),
                    });
                }
            }
        }
        // 只按完整 canonical URI 或旧 transcript 中的完整物理路径保护，不按 basename 串联不同会话。
        let mut referenced = HashSet::new();
        if !files.is_empty() {
            let references: Vec<String> = files
                .iter()
                .flat_map(|file| {
                    let mut values = Vec::with_capacity(2);
                    if let Ok(uri) = crate::resources::ArtifactStore::new(
                        file.session.clone(),
                        file.paths.clone(),
                    )
                    .uri_for_path(&file.path)
                    {
                        values.push(uri);
                    }
                    values.push(file.path.to_string_lossy().into_owned());
                    values
                })
                .collect();
            let matcher = aho_corasick::AhoCorasick::new(&references)
                .map_err(|error| CoreError::Session(error.to_string()))?;
            let mut directories = fs::read_dir(self.store.root()).await?;
            while let Some(directory) = directories.next_entry().await? {
                if !directory.file_type().await?.is_dir() {
                    continue;
                }
                let mut entries = fs::read_dir(directory.path()).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let path = entry.path();
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if journal_path(&path).is_none()
                        && !(name.contains(".jsonl.") && name.ends_with(".bak"))
                    {
                        continue;
                    }
                    let mut source = reader(&path).await?;
                    let mut line = Vec::new();
                    loop {
                        line.clear();
                        if source.read_until(b'\n', &mut line).await? == 0 {
                            break;
                        }
                        for found in matcher.find_overlapping_iter(&line) {
                            referenced.insert(references[found.pattern().as_usize()].clone());
                        }
                    }
                }
            }
        }
        for candidate in archives {
            if self.records.read().contains_key(&candidate.session) {
                report.protected += 1;
                continue;
            }
            let _guard = match SessionLockGuard::acquire(
                candidate.paths.lock_file(&candidate.session),
                "maintenance",
            ) {
                Ok(guard) => guard,
                Err(_) => {
                    report.protected += 1;
                    continue;
                }
            };
            let metadata = fs::metadata(&candidate.path).await?;
            if SnapshotId::from_metadata(&metadata) != candidate.revision {
                report.protected += 1;
                continue;
            }
            if apply {
                archive(&candidate.path).await?;
            }
            report
                .archived
                .push(candidate.path.to_string_lossy().into_owned());
        }
        for candidate in files {
            let uri = crate::resources::ArtifactStore::new(
                candidate.session.clone(),
                candidate.paths.clone(),
            )
            .uri_for_path(&candidate.path)
            .ok();
            if uri.as_deref().is_some_and(|uri| referenced.contains(uri))
                || referenced.contains(&candidate.path.to_string_lossy().into_owned())
                || self.records.read().contains_key(&candidate.session)
            {
                report.protected += 1;
                continue;
            }
            let _guard = match SessionLockGuard::acquire(
                candidate.paths.lock_file(&candidate.session),
                "maintenance",
            ) {
                Ok(guard) => guard,
                Err(_) => {
                    report.protected += 1;
                    continue;
                }
            };
            let metadata = fs::metadata(&candidate.path).await?;
            if SnapshotId::from_metadata(&metadata) != candidate.revision {
                report.protected += 1;
                continue;
            }
            if apply {
                fs::remove_file(&candidate.path).await?;
            }
            report.bytes += candidate.bytes;
            report
                .removed
                .push(candidate.path.to_string_lossy().into_owned());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        entries::{Entry, EntryPayload},
        store::{SessionFile, SessionStore},
    };
    use moc_protocol::{SessionMode, SessionPermission};

    #[tokio::test]
    async fn preview_preserves_files_and_archive_can_resume_without_losing_references() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path().join("sessions"));
        let id = "ses_0123456789ab";
        let header = Entry::header(
            id.into(),
            root.path().to_string_lossy().into_owned(),
            "p/model:off".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            None,
            "归档测试".into(),
            1,
        );
        let paths = store.paths_for(root.path(), id, 1);
        let file = SessionFile::create_with_header(&paths.transcript, &header)
            .await
            .unwrap();
        std::fs::create_dir_all(paths.artifacts()).unwrap();
        let kept = paths.artifacts().join("kept.txt");
        let garbage = paths.artifacts().join("unused.txt");
        fs::write(&kept, "仍被引用").await.unwrap();
        fs::write(&garbage, "未被引用").await.unwrap();
        let entry = Entry::from_payload(
            EntryPayload::Message {
                message: moc_llm::Message::user_text(format!("保留 {}", kept.display())),
                tool: None,
                finish_reason: None,
            },
            header.id().into(),
            2,
        );
        file.append(&entry, None).await.unwrap();
        drop(file);
        let table = SessionTable::new(store.root(), true);
        let policy = RetentionConfig {
            archive_after_days: 0,
            retain_newest_global: 0,
            retain_newest_per_workspace: 0,
            artifacts_days: 0,
            logs_days: 0,
            ..Default::default()
        };
        let preview = table.maintenance(&policy, false).await.unwrap();
        assert_eq!(
            preview.archived,
            vec![paths.transcript.to_string_lossy().into_owned()]
        );
        assert_eq!(
            preview.removed,
            vec![garbage.to_string_lossy().into_owned()]
        );
        assert!(paths.transcript.exists() && garbage.exists());
        table.maintenance(&policy, true).await.unwrap();
        assert!(!paths.transcript.exists());
        assert!(paths.transcript.with_extension("jsonl.gz").exists());
        assert!(kept.exists());
        assert!(!garbage.exists());
        assert_eq!(table.list().await.unwrap()[0].id, id);
        table.get(id).await.unwrap();
        table
            .append_message(id, moc_llm::Message::user_text("恢复后继续"), None, None)
            .await
            .unwrap();
        let entries = store.read_entries(&paths).await.unwrap();
        assert!(entries.iter().any(|entry|matches!(entry,Entry::Message{message,..} if message.text().contains("恢复后继续"))));
        assert!(entries.iter().any(|entry|matches!(entry,Entry::Message{message,..} if message.text().contains("kept.txt"))));
    }
}
