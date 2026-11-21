//! 已展示行和版本链按文件持久化；不保存正文或撤销栈。

use super::{AnchorStore, MAX_FILES, MAX_VERSIONS, ServedFile};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

impl AnchorStore {
    /// 每个运行时只加载一次。实际文件的完整 epoch 仍由 read/edit 在使用时校验。
    pub async fn restore(&self, sidecar: &Path) -> io::Result<()> {
        self.restored
            .get_or_try_init(|| async {
                let directory = sidecar.join("anchors");
                let source = directory.clone();
                let (files, removed) = tokio::task::spawn_blocking(move || load(&source))
                    .await
                    .map_err(io::Error::other)??;
                let mut current = self.files.write();
                current.extend(files);
                while current.len() > MAX_FILES {
                    let path = current.keys().next().cloned().expect("文件数超过上限");
                    current.remove(&path);
                    self.removed.lock().insert(path);
                }
                self.removed.lock().extend(removed);
                Ok::<_, io::Error>(directory)
            })
            .await?;
        Ok(())
    }

    /// 同一会话串行落盘，仅序列化被本轮工具改变的记录；锁外执行文件 I/O。
    pub async fn flush(&self) -> io::Result<()> {
        let Some(directory) = self.restored.get() else {
            return Ok(());
        };
        let _serial = self.persistence.lock().await;
        let (records, removed): (Vec<(PathBuf, Vec<u8>)>, Vec<PathBuf>) = {
            // 与 record_served/commit_edit/invalidate 保持相同锁序。
            let files = self.files.read();
            let mut dirty = self.dirty.lock();
            let mut removed = self.removed.lock();
            let records = dirty
                .iter()
                .filter_map(|path| files.get(path).map(|served| (path, served)))
                .map(|(path, served)| {
                    let bytes = serde_json::to_vec(&(path, served)).map_err(io::Error::other)?;
                    Ok((path.clone(), bytes))
                })
                .collect::<io::Result<Vec<_>>>()?;
            let removed_paths = removed.drain().collect();
            dirty.clear();
            (records, removed_paths)
        };
        if records.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let result = async {
            if !records.is_empty() {
                tokio::fs::create_dir_all(directory).await?;
            }
            for (path, bytes) in &records {
                let hash = xxhash_rust::xxh3::xxh3_128(path.as_os_str().as_encoded_bytes());
                let target = directory.join(format!("{hash:032x}.json"));
                let temporary = target.with_extension(format!("tmp-{}", crate::ids::entry_id()));
                let saved = async {
                    tokio::fs::write(&temporary, bytes).await?;
                    tokio::fs::rename(&temporary, &target).await
                }
                .await;
                if saved.is_err() {
                    let _ = tokio::fs::remove_file(&temporary).await;
                }
                saved?;
            }
            for path in &removed {
                let hash = xxhash_rust::xxh3::xxh3_128(path.as_os_str().as_encoded_bytes());
                let target = directory.join(format!("{hash:032x}.json"));
                match tokio::fs::remove_file(target).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok::<_, io::Error>(())
        }
        .await;
        if result.is_err() {
            let files = self.files.read();
            let mut dirty = self.dirty.lock();
            let mut pending_removals = self.removed.lock();
            for path in records.into_iter().map(|(path, _)| path).chain(removed) {
                if files.contains_key(&path) {
                    pending_removals.remove(&path);
                    dirty.insert(path);
                } else {
                    dirty.remove(&path);
                    pending_removals.insert(path);
                }
            }
        }
        result
    }
}

fn load(directory: &Path) -> io::Result<(HashMap<PathBuf, ServedFile>, Vec<PathBuf>)> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((HashMap::new(), Vec::new()));
        }
        Err(error) => return Err(error),
    };
    let mut entries = entries;
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = HashMap::new();
    let mut removed = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let (path, served): (PathBuf, ServedFile) = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // 防止损坏的位图或版本链在编辑时越界；缺失历史状态不能伪装成有效许可。
        let valid = if served.epoch.is_none() {
            served.seen.is_empty() && served.versions.is_empty()
        } else {
            served.seen.len() == served.len.div_ceil(64)
                && !served.versions.is_empty()
                && served.versions.len() <= MAX_VERSIONS
                && served
                    .versions
                    .last()
                    .is_some_and(|version| version.len == served.len && version.moves.is_empty())
                && served
                    .versions
                    .iter()
                    .all(|version| version.moves.windows(2).all(|pair| pair[0].0 <= pair[1].0))
        };
        if !valid || !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid persisted edit snapshot",
            ));
        }
        if files.len() < MAX_FILES {
            files.insert(path, served);
        } else {
            removed.push(path);
        }
    }
    Ok((files, removed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::apply::{EditOp, EditRequest, apply_edit, plan_edit};
    use crate::hashline::{LineEnding, SnapshotId, served::read_snapshot, tag_text};

    #[tokio::test]
    async fn file_limit_evicts_and_removes_persisted_anchor() {
        let directory = tempfile::tempdir().unwrap();
        let store = AnchorStore::new();
        store.restore(directory.path()).await.unwrap();
        let snapshot = crate::hashline::FileSnapshot {
            bytes: b"line\n".to_vec(),
            lines: vec!["line".into()],
            epoch: SnapshotId {
                device: 0,
                inode: 0,
                modified_secs: 0,
                modified_nanos: 0,
                changed_secs: 0,
                changed_nanos: 0,
                size: 5,
                content_hash: 0,
            },
            bom: false,
            line_ending: LineEnding::Lf,
        };
        for index in 0..MAX_FILES {
            store.record_served(
                directory.path().join(format!("file-{index}")),
                &snapshot,
                &[0..1],
            );
        }
        store.flush().await.unwrap();

        store.record_served(directory.path().join("new-file"), &snapshot, &[0..1]);
        assert_eq!(store.files.read().len(), MAX_FILES);
        store.flush().await.unwrap();

        let anchors = directory.path().join("anchors");
        let count = std::fs::read_dir(&anchors).unwrap().count();
        assert_eq!(count, MAX_FILES);
        let restored = AnchorStore::new();
        restored.restore(directory.path()).await.unwrap();
        assert_eq!(restored.files.read().len(), MAX_FILES);
    }

    #[tokio::test]
    #[allow(clippy::single_range_in_vec_init)]
    async fn restored_anchors_preserve_versions_visibility_and_invalidation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n").unwrap();
        // 锚点与 apply 的路径解析都以规范化路径为键（macOS /var→/private/var）。
        let path = path.canonicalize().unwrap();
        let store = AnchorStore::new();
        store.restore(directory.path()).await.unwrap();
        let snapshot = read_snapshot(&path, 4096).unwrap();
        let tag = store.record_served(path.clone(), &snapshot, &[0..2]);
        let mut request = EditRequest {
            path: path.to_string_lossy().into_owned(),
            tag: tag_text(tag),
            edits: vec![EditOp::Insert {
                after: 0,
                text: "header".into(),
            }],
            move_lines: None,
        };
        apply_edit(
            directory.path(),
            &request,
            &store.served(&path).unwrap(),
            None,
            &store,
            32,
            4096,
        )
        .await
        .unwrap();
        store.flush().await.unwrap();
        drop(store);

        let restored = AnchorStore::new();
        restored.restore(directory.path()).await.unwrap();
        request.edits = vec![EditOp::Replace {
            from: 2,
            to: 2,
            text: "changed".into(),
        }];
        let served = restored.served(&path).unwrap();
        let plan = plan_edit(directory.path(), &request, &served, None, 32, 4096).unwrap();
        assert_eq!(&plan.lines[..4], ["header", "one", "changed", "three"]);
        request.edits = vec![EditOp::Delete { from: 8, to: 8 }];
        assert!(
            plan_edit(directory.path(), &request, &served, None, 32, 4096)
                .unwrap_err()
                .to_string()
                .contains("E_UNSEEN")
        );

        std::fs::write(&path, "external\n").unwrap();
        assert!(
            plan_edit(directory.path(), &request, &served, None, 32, 4096)
                .unwrap_err()
                .to_string()
                .contains("E_STALE_TAG")
        );
        restored.invalidate(&path);
        restored.flush().await.unwrap();
        let invalidated = AnchorStore::new();
        invalidated.restore(directory.path()).await.unwrap();
        assert!(invalidated.served(&path).unwrap().epoch.is_none());
    }
}
