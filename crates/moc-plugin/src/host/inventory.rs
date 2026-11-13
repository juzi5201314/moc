use super::Settings;
use crate::discovery;
use crate::discovery::Discovered;
use crate::discovery::DiscoveryConfig;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

/// 相对路径按工作目录解析：配置里的 dirs 是相对项目说的，不是相对进程 cwd。
pub(super) fn discovery_config(cwd: &Path, settings: &Settings) -> DiscoveryConfig {
    DiscoveryConfig {
        dirs: settings
            .discovery
            .dirs
            .iter()
            .map(|dir| {
                if dir.is_absolute() {
                    dir.clone()
                } else {
                    cwd.join(dir)
                }
            })
            .collect(),
        ..settings.discovery.clone()
    }
}

/// 当前插件目录的指纹：目录集合、清单与模块文件的大小和修改时间。
///
/// core 在每次取宿主前算一遍，和宿主加载时的 [`PluginHost::stamp`] 比较，不同就重建。
pub fn fingerprint(cwd: &Path, home: &Path, settings: &Settings) -> u64 {
    fingerprint_of(&discovery::discover(
        cwd,
        home,
        &discovery_config(cwd, settings),
    ))
}

pub(super) fn fingerprint_of(discovered: &[Discovered]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for candidate in discovered {
        candidate.dir.hash(&mut hasher);
        candidate.config_path.hash(&mut hasher);
        for file in plugin_files(&candidate.dir) {
            file.hash(&mut hasher);
            let metadata = std::fs::metadata(&file).ok();
            metadata.as_ref().map(|meta| meta.len()).hash(&mut hasher);
            metadata
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_nanos())
                .hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// 参与指纹的文件：清单与插件目录里（含子目录）的 `.js`/`.mjs` 模块。
///
/// 深度上限只用于兜底符号链接成环；正常插件目录远用不到。
pub(super) fn plugin_files(dir: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 8;
    let mut files = Vec::new();
    collect_files(dir, 0, MAX_DEPTH, &mut files);
    files.sort();
    files
}

pub(super) fn collect_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str());
        if name.is_some_and(|name| name.starts_with('.')) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            collect_files(&path, depth + 1, max_depth, out);
            continue;
        }
        let extension = path.extension().and_then(|extension| extension.to_str());
        if name == Some("plugin.json") || matches!(extension, Some("js" | "mjs")) {
            out.push(path);
        }
    }
}
