//! 解码历史的私有缓存目录：路径克隆共享所有权，不让一个进程删除另一个进程的镜像。

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(super) struct HistoryCache {
    directory: PathBuf,
}

impl HistoryCache {
    pub(super) fn new(sidecar: &Path) -> Self {
        Self {
            directory: sidecar
                .join("history")
                .join(format!("view-{}", uuid::Uuid::now_v7().simple())),
        }
    }

    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(super) fn ensure(&self) -> io::Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.directory)
    }

    pub(super) fn clear(&self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.directory.display(), %error, "清理解码历史缓存失败");
        }
    }
}

impl Drop for HistoryCache {
    fn drop(&mut self) {
        self.clear();
    }
}
