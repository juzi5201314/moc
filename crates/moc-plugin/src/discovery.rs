//! 插件目录发现：多来源扫描、按 rank 排序与真实路径去重。
//!
//! 来源与 rank（小者优先；项目级随向上遍历深度递增，越近越优先）：
//! 1. 项目 `.moc/plugins`（rank 100+depth）
//! 2. 配置 `[plugins] dirs`（rank 300）
//! 3. 用户 `~/.moc/plugins`（rank 400）
//!
//! 布局固定为 `<root>/<name>/plugin.json`（平铺，不做分类子目录）。同名插件取 rank
//! 最小者；目录按真实路径去重，避免 symlink 安装器装两次。

use std::path::{Path, PathBuf};

/// 发现配置；`dirs` 已由调用方展开 `~`。
#[derive(Debug, Clone, Default)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub user: bool,
    pub project: bool,
    pub dirs: Vec<PathBuf>,
}

/// 一个候选插件目录。
#[derive(Debug, Clone)]
pub struct Discovered {
    /// 目录名；最终名以清单为准，这里只用于告警与排序。
    pub dir_name: String,
    pub dir: PathBuf,
    /// 来源描述，用于 `plugin/list` 与诊断。
    pub source: String,
    /// 安装来源所属配置；在解析符号链接之前确定。
    pub config_path: PathBuf,
}

/// 扫描全部来源，返回按优先级排序的候选目录。
pub fn discover(cwd: &Path, home: &Path, config: &DiscoveryConfig) -> Vec<Discovered> {
    if !config.enabled {
        return Vec::new();
    }
    let mut roots: Vec<(u32, PathBuf, String, PathBuf)> = Vec::new();
    if config.project {
        let git_root = git_root(cwd);
        for (depth, dir) in ancestors(cwd, git_root.as_deref()).into_iter().enumerate() {
            roots.push((
                100 + depth as u32,
                dir.join(".moc").join("plugins"),
                format!("项目 .moc/plugins（{}）", dir.display()),
                dir.join(".moc/config.toml"),
            ));
        }
    }
    for dir in &config.dirs {
        roots.push((
            300,
            dir.clone(),
            format!("配置 dirs（{}）", dir.display()),
            home.join("config.toml"),
        ));
    }
    if config.user {
        roots.push((
            400,
            home.join("plugins"),
            "用户 .moc/plugins".into(),
            home.join("config.toml"),
        ));
    }
    roots.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (_rank, root, source, config_path) in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        children.sort();
        for child in children {
            let Some(dir_name) = child
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if dir_name.starts_with('.') || !child.join("plugin.json").is_file() {
                continue;
            }
            let Ok(real) = child.canonicalize() else {
                tracing::warn!("插件目录无法解析真实路径，已跳过：{}", child.display());
                continue;
            };
            if !seen.insert(real.clone()) {
                continue;
            }
            found.push(Discovered {
                dir_name,
                dir: real,
                source: source.clone(),
                config_path: config_path.clone(),
            });
        }
    }
    found
}

/// 从 cwd 向上到 git root（含）；没有 git root 时到文件系统根。
fn ancestors(cwd: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in cwd.ancestors() {
        dirs.push(dir.to_owned());
        if Some(dir) == git_root {
            break;
        }
    }
    dirs
}

/// 最近一个含 `.git` 的祖先目录；没有则 `None`。
fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        std::fs::write(dir.join("index.js"), "export default {};").unwrap();
    }

    #[test]
    fn ranks_project_over_config_over_user() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let home = temp.path().join("home");
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&project).unwrap();
        install(&project.join(".moc/plugins"), "shared");
        install(&extra, "shared");
        install(&home.join("plugins"), "shared");
        install(&home.join("plugins"), "user-only");

        let config = DiscoveryConfig {
            enabled: true,
            user: true,
            project: true,
            dirs: vec![extra],
        };
        let found = discover(&project, &home, &config);
        let names: Vec<_> = found.iter().map(|item| item.dir_name.as_str()).collect();
        // 同名由宿主按顺序取舍；发现阶段保持 rank 顺序，项目级排在最前。
        assert_eq!(names, ["shared", "shared", "shared", "user-only"]);
        assert!(found[0].source.starts_with("项目"));
        assert!(found[3].source.starts_with("用户"));
    }

    #[test]
    fn walks_up_to_the_git_root_and_stops_there() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let nested = repo.join("crates/app");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        install(&repo.join(".moc/plugins"), "root-level");
        // 仓库之外的父目录不参与扫描。
        install(&temp.path().join(".moc/plugins"), "outside");

        let config = DiscoveryConfig {
            enabled: true,
            user: false,
            project: true,
            dirs: Vec::new(),
        };
        let found = discover(&nested, temp.path(), &config);
        let names: Vec<_> = found.iter().map(|item| item.dir_name.as_str()).collect();
        assert_eq!(names, ["root-level"]);
    }

    #[test]
    fn symlinked_installs_are_deduplicated() {
        #[cfg(unix)]
        {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let project = temp.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            install(&home.join("plugins"), "real");
            std::fs::create_dir_all(project.join(".moc/plugins")).unwrap();
            std::os::unix::fs::symlink(
                home.join("plugins/real"),
                project.join(".moc/plugins/link"),
            )
            .unwrap();

            let config = DiscoveryConfig {
                enabled: true,
                user: true,
                project: true,
                dirs: Vec::new(),
            };
            let found = discover(&project, &home, &config);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].dir_name, "link");
        }
    }

    #[test]
    fn disabled_discovery_returns_nothing() {
        let temp = tempfile::tempdir().unwrap();
        install(&temp.path().join("plugins"), "x");
        let found = discover(
            temp.path(),
            temp.path(),
            &DiscoveryConfig {
                enabled: false,
                user: true,
                project: true,
                dirs: Vec::new(),
            },
        );
        assert!(found.is_empty());
    }
}
