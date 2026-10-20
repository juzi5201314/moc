//! 会话级沙盒 scratch：临时目录、合成 HOME 与 HOME overlay 的 upper/work。
//!
//! 目录放在系统临时目录下，权限由 `tempfile` 创建为 0700；`TempDir` 随
//! `SessionRuntime` 或后台任务释放，避免在宿主 HOME 里留下会话状态。

use std::path::PathBuf;

use tempfile::{Builder, TempDir};

#[derive(Debug)]
pub(crate) struct SandboxScratch {
    _root: TempDir,
    pub tmp: PathBuf,
    pub var_tmp: PathBuf,
    pub home: PathBuf,
    pub overlay_upper: PathBuf,
    overlay_work: PathBuf,
    /// 被拒绝路径的只读空文件占位；bwrap 用它遮蔽单个文件。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub deny_file: PathBuf,
}

impl SandboxScratch {
    pub(crate) fn new() -> std::io::Result<Self> {
        let root = Builder::new().prefix("moc-sandbox-").tempdir()?;
        let tmp = root.path().join("tmp");
        let var_tmp = root.path().join("var-tmp");
        let home = root.path().join("home");
        let overlay_upper = root.path().join("overlay/upper");
        let overlay_work = root.path().join("overlay/work");
        let deny_file = root.path().join("deny-file");
        std::fs::File::create(&deny_file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&deny_file, std::fs::Permissions::from_mode(0o444))?;
        }
        for directory in [
            &tmp,
            &var_tmp,
            &home,
            &tmp.join("cache"),
            &overlay_upper,
            &overlay_work,
        ] {
            std::fs::create_dir_all(directory)?;
        }
        Ok(Self {
            _root: root,
            tmp,
            var_tmp,
            home,
            overlay_upper,
            overlay_work,
            deny_file,
        })
    }

    /// 为一次 bwrap 启动创建独立的 overlay workdir；overlayfs 要求它为空。
    pub(crate) fn new_overlay_work(&self) -> std::io::Result<TempDir> {
        Builder::new()
            .prefix("work-")
            .tempdir_in(&self.overlay_work)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_all_scratch_directories() {
        let scratch = SandboxScratch::new().expect("创建 scratch");
        for path in [
            &scratch.tmp,
            &scratch.var_tmp,
            &scratch.home,
            &scratch.tmp.join("cache"),
            &scratch.overlay_upper,
        ] {
            assert!(path.is_dir(), "{} 应该是目录", path.display());
        }
        let work = scratch.new_overlay_work().expect("创建 workdir");
        assert!(work.path().is_dir());
        assert!(scratch.deny_file.is_file());
    }
}
