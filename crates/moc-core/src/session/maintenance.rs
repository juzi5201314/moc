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

fn escaped_path_pattern(path: &str) -> String {
    // 使用 JSON 字符串转义并去掉首尾引号，匹配 transcript 中的完整路径。
    let encoded = serde_json::to_string(path).expect("路径字符串可序列化为 JSON");
    encoded[1..encoded.len() - 1].to_owned()
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
                    let mut values = Vec::with_capacity(3);
                    if let Ok(uri) = crate::resources::ArtifactStore::new(file.paths.clone())
                        .uri_for_path(&file.path)
                    {
                        values.push(uri);
                    }
                    let path = file.path.to_string_lossy().into_owned();
                    values.push(escaped_path_pattern(&path));
                    values.push(path);
                    values
                })
                .collect();
            let matcher = aho_corasick::AhoCorasick::new(&references)
                .map_err(|error| CoreError::Session(error.to_string()))?;
            let scan_paths = self.store.journals(true).await?;
            tokio::task::spawn_blocking(move || {
                let mut referenced = HashSet::new();
                for path in scan_paths {
                    log::for_each_line(&path, &mut |line| {
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
                || referenced.contains(&escaped_path_pattern(&candidate.path.to_string_lossy()))
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
        entries::{AgentSessionMeta, Entry, EntryPayload, SessionOrigin},
        store::{SessionFile, SessionStore},
    };
    use moc_protocol::{SessionEngine, SessionPermission};

    #[tokio::test]
    async fn 维护保留子代理及归档中独有的产物引用() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::new(root.path().join("sessions"));
        let parent = "ses_parent000001";
        let child = "ses_child0000001";
        let paths = store.paths_for(root.path(), parent, 1);
        fs::create_dir_all(paths.artifacts()).await.unwrap();
        let mut kept = Vec::new();
        let mut transcripts = Vec::new();
        for (id, parent_id, depth, compressed, agent) in [
            (parent, None, 0, false, None),
            (child, Some(parent), 1, true, None),
            ("ses_grandchild01", Some(child), 2, false, None),
            (
                "ses_actor000001",
                Some(parent),
                1,
                true,
                Some(AgentSessionMeta {
                    workflow_run: Some("run_maintenance".into()),
                    ..Default::default()
                }),
            ),
        ] {
            let (session_paths, origin, text) = if let Some(parent_id) = parent_id {
                let session_paths = store
                    .paths_for_child(root.path(), parent_id, id, 1, compressed)
                    .await
                    .unwrap();
                let artifact = if compressed {
                    session_paths.artifacts()
                } else {
                    paths.artifacts()
                }
                .join(format!("{id}.txt"));
                fs::create_dir_all(artifact.parent().unwrap())
                    .await
                    .unwrap();
                fs::write(&artifact, "仍被引用").await.unwrap();
                // 分别覆盖子代理的 URI 引用和孙代理的完整物理路径引用。
                let reference = if compressed {
                    crate::resources::ArtifactStore::new(session_paths.clone())
                        .uri_for_path(&artifact)
                        .unwrap()
                } else {
                    artifact.to_string_lossy().into_owned()
                };
                kept.push(artifact);
                (
                    session_paths,
                    SessionOrigin::spawned(parent_id, depth, agent),
                    reference,
                )
            } else {
                (
                    paths.clone(),
                    SessionOrigin::root(),
                    "父会话无产物引用".into(),
                )
            };
            let header = Entry::header(
                id.into(),
                root.path().to_string_lossy().into_owned(),
                "p/model:off".into(),
                SessionEngine::Standard,
                SessionPermission::Full,
                origin,
                "维护测试".into(),
                1,
            );
            let file = SessionFile::create_with_header(&session_paths.transcript, &header)
                .await
                .unwrap();
            let entry = Entry::from_payload(
                EntryPayload::Message {
                    message: moc_llm::Message::user_text(text),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                    goal: None,
                },
                header.id().into(),
                2,
            );
            file.append(&entry, None).await.unwrap();
            transcripts.push(session_paths);
        }
        // 引用只留在归档或恢复备份中，当前 transcript 不再含有它们。
        for transcript in &transcripts {
            for reference_path in [
                transcript.compaction().join("segment-0000.md"),
                transcript.transcript.with_extension("jsonl.recovery.bak"),
            ] {
                let artifact = transcript
                    .artifacts()
                    .join(format!("archive-{}.txt", kept.len()));
                fs::create_dir_all(artifact.parent().unwrap())
                    .await
                    .unwrap();
                fs::write(&artifact, "归档仍引用").await.unwrap();
                fs::create_dir_all(reference_path.parent().unwrap())
                    .await
                    .unwrap();
                fs::write(
                    &reference_path,
                    format!(
                        "{}\n",
                        crate::resources::ArtifactStore::new(transcript.clone())
                            .uri_for_path(&artifact)
                            .unwrap()
                    ),
                )
                .await
                .unwrap();
                kept.push(artifact);
            }
        }
        // 子会话位于嵌套的 ses_ 目录中；用真实 zstd 帧覆盖仅在压缩恢复备份中的引用。
        let child_paths = &transcripts[1];
        let artifact = child_paths.artifacts().join("compressed-backup.txt");
        fs::write(&artifact, "压缩备份仍引用").await.unwrap();
        let reference = crate::resources::ArtifactStore::new(child_paths.clone())
            .uri_for_path(&artifact)
            .unwrap();
        let line = format!("{}\n", serde_json::json!({ "text": reference }));
        fs::write(
            child_paths.transcript.with_extension("zst.1234567890.bak"),
            log::compress_frame(line.as_bytes()).unwrap(),
        )
        .await
        .unwrap();
        kept.push(artifact);
        let garbage = paths.artifacts().join("unused.txt");
        fs::write(&garbage, "未被引用").await.unwrap();
        let table = SessionTable::new(store.root(), true);
        let policy = RetentionConfig {
            artifacts_days: 0,
            logs_days: 0,
            ..Default::default()
        };
        for apply in [false, true] {
            let report = table.maintenance(&policy, apply).await.unwrap();
            for artifact in &kept {
                assert!(
                    !report
                        .removed
                        .contains(&artifact.to_string_lossy().into_owned()),
                    "仍被引用的产物不应被清理：{}",
                    artifact.display()
                );
                assert!(artifact.exists());
            }
            assert_eq!(report.removed, vec![garbage.to_string_lossy().into_owned()]);
            assert_eq!(garbage.exists(), !apply);
            assert_eq!(store.list().await.unwrap().len(), transcripts.len());
        }
    }

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
        // Unix 文件名允许反斜杠和双引号，可在当前平台覆盖 JSON 转义路径的匹配。
        let kept = paths.artifacts().join(if cfg!(windows) {
            "kept.txt"
        } else {
            r#"kept\"kept.txt"#
        });
        let garbage = paths.artifacts().join("unused.txt");
        fs::write(&kept, "仍被引用").await.unwrap();
        fs::write(&garbage, "未被引用").await.unwrap();
        let entry = Entry::from_payload(
            EntryPayload::Message {
                message: moc_llm::Message::user_text(format!("保留 {}", kept.display())),
                tool: None,
                finish_reason: None,
                agent: None,
                goal: None,
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
