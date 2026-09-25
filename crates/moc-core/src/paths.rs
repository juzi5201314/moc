#[cfg(windows)]
use std::ffi::OsString;
use std::path::PathBuf;

/// 返回当前用户的主目录，兼容 Unix、原生 Windows 和 Git Bash 环境。
pub(crate) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(path) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(path));
        }
        if let (Some(drive), Some(path)) =
            (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
        {
            let mut home = OsString::from(drive);
            home.push(path);
            return Some(PathBuf::from(home));
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}
