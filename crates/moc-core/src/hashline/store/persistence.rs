//! 已展示行和版本链按文件持久化；不保存正文或撤销栈。

use super::{AnchorStore, MAX_VERSIONS, ServedFile};
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
                let files = tokio::task::spawn_blocking(move || load(&source))
                    .await
                    .map_err(io::Error::other)??;
                self.files.write().extend(files);
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
        let records: Vec<(PathBuf, Vec<u8>)> = {
            // 与 record_served/commit_edit/invalidate 保持相同锁序。
            let files = self.files.read();
            let mut dirty = self.dirty.lock();
            let records = dirty
                .iter()
                .map(|path| {
                    let served = files.get(path).expect("脏记录一定存在");
                    let bytes = serde_json::to_vec(&(path, served)).map_err(io::Error::other)?;
                    Ok((path.clone(), bytes))
                })
                .collect::<io::Result<Vec<_>>>()?;
            dirty.clear();
            records
        };
        if records.is_empty() {
            return Ok(());
        }
        let result = async {
            tokio::fs::create_dir_all(directory).await?;
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
            Ok::<_, io::Error>(())
        }
        .await;
        if result.is_err() {
            self.dirty
                .lock()
                .extend(records.into_iter().map(|(path, _)| path));
        }
        result
    }
}

fn load(directory: &Path) -> io::Result<HashMap<PathBuf, ServedFile>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error),
    };
    let mut files = HashMap::new();
    for entry in entries {
        let path = entry?.path();
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
        files.insert(path, served);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::apply::{EditOp, EditRequest, apply_edit, plan_edit};
    use crate::hashline::{served::read_snapshot, tag_text};

    #[tokio::test]
    #[allow(clippy::single_range_in_vec_init)]
    async fn restored_anchors_preserve_versions_visibility_and_invalidation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n").unwrap();
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
