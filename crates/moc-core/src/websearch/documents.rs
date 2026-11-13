//! 会话级网页快照：正文独立落盘，URL 索引只保存元数据；刷新不覆盖旧快照。

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use xxhash_rust::xxh3::xxh3_128;

use super::ExtractResult;
use super::http::WebFetchFailure;

// 冷路径字符串键；仅保留正在使用的锁，避免会话结束后积累 URL。
static LOCKS: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(parking_lot::Mutex::default);

pub(super) struct DocumentStore {
    root: PathBuf,
    index: PathBuf,
}

impl DocumentStore {
    pub fn new(root: &Path, url: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            index: root
                .join("web-cache")
                .join(format!("{:032x}.json", xxh3_128(url.as_bytes()))),
        }
    }

    pub async fn lock(
        &self,
        cancel: &CancellationToken,
    ) -> Result<OwnedMutexGuard<()>, WebFetchFailure> {
        let lock = {
            let mut locks = LOCKS.lock();
            locks.retain(|_, lock| lock.strong_count() != 0);
            let slot = locks.entry(self.index.clone()).or_default();
            if let Some(lock) = slot.upgrade() {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                *slot = Arc::downgrade(&lock);
                lock
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(WebFetchFailure::Cancelled),
            guard = lock.lock_owned() => Ok(guard),
        }
    }

    pub async fn load(&self, url: &str) -> Result<Option<ExtractResult>, WebFetchFailure> {
        let bytes = match tokio::fs::read(&self.index).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut page: ExtractResult = serde_json::from_slice(&bytes)?;
        if page.url != url && page.final_url != url {
            return Err(WebFetchFailure::message("web document index URL mismatch"));
        }
        let file = page
            .artifact
            .as_deref()
            .and_then(|uri| uri.strip_prefix("artifact://artifacts/"))
            .filter(|name| Path::new(name).components().count() == 1 && name.starts_with("web-"))
            .ok_or_else(|| WebFetchFailure::message("invalid web document artifact"))?;
        page.raw_content =
            tokio::fs::read_to_string(self.root.join("artifacts").join(file)).await?;
        page.cached = true;
        Ok(Some(page))
    }

    pub async fn save(&self, page: &mut ExtractResult) -> Result<(), WebFetchFailure> {
        let directory = self.root.join("artifacts");
        tokio::fs::create_dir_all(&directory).await?;
        let name = format!("web-{}.md", crate::ids::entry_id());
        tokio::fs::write(directory.join(&name), page.raw_content.as_bytes()).await?;
        page.artifact = Some(format!("artifact://artifacts/{name}"));
        let mut indexes = vec![self.index.clone()];
        if page.final_url != page.url {
            indexes.push(Self::new(&self.root, &page.final_url).index);
        }
        let metadata = serde_json::to_vec(page)?;
        // 索引原子替换；读者要么得到旧快照，要么得到已经完整写入的新快照。
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            for index in indexes {
                let parent = index.parent().expect("URL 索引有父目录");
                std::fs::create_dir_all(parent)?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(&metadata)?;
                file.persist(index).map_err(|error| error.error)?;
            }
            Ok(())
        })
        .await??;
        Ok(())
    }
}
