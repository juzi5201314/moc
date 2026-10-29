//! 存储维护：先预览，用户确认后清理没有被引用的产物和任务日志。

use super::{SessionTable, lock::SessionLockGuard, log, store::SessionPaths};
use crate::{CoreError, config::RetentionConfig, hashline::SnapshotId};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs;

pub(crate) fn journal_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")).then(|| path.to_owned())
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
        let mut files = Vec::new();
        for session in &sessions {
            if self.records.read().contains_key(&session.id) || session.lock.is_some() {
                report.protected += 1;
                continue;
            }
            let Some(paths) = self.store.find(&session.id).await? else {
                continue;
            };
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
        // 只按完整的规范化 URI 或 transcript 里的完整物理路径保护，不按文件名把不同
        // 会话关联起来。压缩会话由 log 层解码后再扫描。
        let referenced = if files.is_empty() {
            HashSet::new()
        } else {
            let references: Vec<String> = files
                .iter()
                .flat_map(|file| {
                    let mut values = Vec::with_capacity(2);
                    if let Ok(uri) = crate::resources::ArtifactStore::new(file.paths.clone())
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
            let mut scan_paths = Vec::new();
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
                    scan_paths.push(path);
                }
            }
            tokio::task::spawn_blocking(move || {
                let mut referenced = HashSet::new();
                for path in scan_paths {
                    log::for_each_line(&path, log::Policy::Fail, &mut |line| {
                        for found in matcher.find_overlapping_iter(line) {
                            referenced.insert(references[found.pattern().as_usize()].clone());
                        }
                        Ok(())
                    })?;
                }
                Ok::<_, CoreError>(referenced)
            })
            .await
            .map_err(|error| CoreError::Session(error.to_string()))??
        };
        for candidate in files {
            let uri = crate::resources::ArtifactStore::new(candidate.paths.clone())
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
        entries::{Entry, EntryPayload, SessionOrigin},
        store::{SessionFile, SessionStore},
    };
    use moc_protocol::{SessionEngine, SessionPermission};

    #[tokio::test]
    async fn preview_preserves_referenced_files_and_cleanup_applies() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path().join("sessions"));
        let id = "ses_0123456789ab";
        let header = Entry::header(
            id.into(),
            root.path().to_string_lossy().into_owned(),
            "p/model:off".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "维护测试".into(),
            1,
        );
        let paths = store.paths_for_compressed(root.path(), id, 1);
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
                agent: None,
            },
            header.id().into(),
            2,
        );
        file.append(&entry, None).await.unwrap();
        drop(file);
        let table = SessionTable::new(store.root(), true);
        let policy = RetentionConfig {
            artifacts_days: 0,
            logs_days: 0,
            ..Default::default()
        };
        let preview = table.maintenance(&policy, false).await.unwrap();
        assert_eq!(
            preview.removed,
            vec![garbage.to_string_lossy().into_owned()]
        );
        assert!(paths.transcript.exists() && garbage.exists());
        table.maintenance(&policy, true).await.unwrap();
        assert!(paths.transcript.exists());
        assert!(kept.exists());
        assert!(!garbage.exists());
        assert_eq!(table.list().await.unwrap()[0].id, id);
        table.get(id).await.unwrap();
        table
            .append_message(id, moc_llm::Message::user_text("维护后继续"), None, None)
            .await
            .unwrap();
        let entries = store.read_entries(&paths).await.unwrap();
        assert!(entries.iter().any(|entry|matches!(entry,Entry::Message{message,..} if message.text().contains("维护后继续"))));
        assert!(entries.iter().any(|entry|matches!(entry,Entry::Message{message,..} if message.text().contains("kept.txt"))));
    }
}
