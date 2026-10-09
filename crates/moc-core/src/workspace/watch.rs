//! Git 或 ignore 遍历生成的确定性工作区快照与变更。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use moc_protocol::methods::PathChange;

/// 路径相对 cwd，值表示其被修改或创建的基线状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceSnapshot {
    pub entries: BTreeMap<PathBuf, WorkspaceEntry>,
    /// 达到扫描上限时只比较已覆盖路径，不能把未扫描路径判为删除。
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub status: String,
    pub modified_ns: u128,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("读取工作区快照失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("git status 执行失败：{0}")]
    Git(String),
}

/// 优先取 `git status --porcelain -z`，非 Git 目录使用 ignore 规则快照。
pub fn snapshot(cwd: &Path, max_entries: usize) -> Result<WorkspaceSnapshot, WorkspaceError> {
    if crate::tools::is_slow_filesystem_path(cwd) || crate::tools::is_filesystem_mount(cwd) {
        return Ok(WorkspaceSnapshot {
            entries: BTreeMap::new(),
            truncated: true,
        });
    }
    if let Some(root) = git_root(cwd) {
        return git_snapshot(cwd, &root, max_entries);
    }
    walk_snapshot(cwd, max_entries)
}

/// 对两个有序快照求 added/modified/removed。
pub fn diff(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<PathChange> {
    let mut changes = Vec::new();
    for (path, previous) in &before.entries {
        match after.entries.get(path) {
            None if !after.truncated => changes.push(PathChange {
                path: path.display().to_string(),
                kind: "removed".into(),
            }),
            Some(current) if current != previous => changes.push(PathChange {
                path: path.display().to_string(),
                kind: "modified".into(),
            }),
            _ => {}
        }
    }
    for path in after.entries.keys() {
        if !before.truncated && !before.entries.contains_key(path) {
            changes.push(PathChange {
                path: path.display().to_string(),
                kind: "added".into(),
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    changes
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn git_snapshot(
    cwd: &Path,
    root: &Path,
    max_entries: usize,
) -> Result<WorkspaceSnapshot, WorkspaceError> {
    let canonical_cwd = std::fs::canonicalize(cwd)?;
    let prefix = canonical_cwd
        .strip_prefix(root)
        .map_err(|error| WorkspaceError::Git(error.to_string()))?;
    let mut truncated = false;
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "-uall", "--", "."])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let records: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    let mut statuses = BTreeMap::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.len() < 4 {
            continue;
        }
        let status = String::from_utf8_lossy(&record[..2]).trim().to_owned();
        let path = std::str::from_utf8(&record[3..])
            .map(PathBuf::from)
            .map_err(|error| WorkspaceError::Git(format!("git 输出路径不是 UTF-8: {error}")))?;
        let renamed = status.contains('R') || status.contains('C');
        if let Ok(relative) = path.strip_prefix(prefix) {
            if statuses.len() < max_entries {
                statuses.insert(relative.to_owned(), status);
            } else {
                truncated = true;
                break;
            }
        }
        if renamed {
            index = index.saturating_add(1);
        }
    }
    let output = Command::new("git")
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err(WorkspaceError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let mut entries = BTreeMap::new();
    for raw_path in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(raw_path)
            .map(PathBuf::from)
            .map_err(|error| WorkspaceError::Git(format!("git 输出路径不是 UTF-8: {error}")))?;
        if statuses
            .get(&path)
            .is_some_and(|status| status.contains('D'))
        {
            continue;
        }
        if entries.len() >= max_entries && !entries.contains_key(&path) {
            truncated = true;
            break;
        }
        let absolute = cwd.join(&path);
        let Ok(metadata) = std::fs::metadata(&absolute) else {
            continue;
        };
        let modified_ns = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        entries.insert(
            path.clone(),
            WorkspaceEntry {
                status: statuses
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| "tracked".into()),
                modified_ns,
                size: metadata.len(),
            },
        );
    }
    Ok(WorkspaceSnapshot { entries, truncated })
}

fn walk_snapshot(cwd: &Path, max_entries: usize) -> Result<WorkspaceSnapshot, WorkspaceError> {
    let mut entries = BTreeMap::new();
    let mut visited = 0;
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(cwd);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_path(|left, right| left.cmp(right));
    for result in builder.build() {
        let entry =
            result.map_err(|error| WorkspaceError::Io(std::io::Error::other(error.to_string())))?;
        if entry.path() == cwd {
            continue;
        }
        if visited >= max_entries {
            truncated = true;
            break;
        }
        visited += 1;
        let metadata = entry
            .metadata()
            .map_err(|error| WorkspaceError::Io(std::io::Error::other(error.to_string())))?;
        if !metadata.is_file() {
            continue;
        }
        let modified_ns = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        entries.insert(
            entry
                .path()
                .strip_prefix(cwd)
                .unwrap_or(entry.path())
                .to_owned(),
            WorkspaceEntry {
                status: "file".into(),
                modified_ns,
                size: metadata.len(),
            },
        );
    }
    Ok(WorkspaceSnapshot { entries, truncated })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn non_git_snapshot_respects_ignore_files_and_detects_modification() {
        let root = tempfile::tempdir().expect("临时目录");
        fs::write(root.path().join(".ignore"), "ignored.txt\n").expect("写 ignore");
        fs::write(root.path().join("tracked.txt"), "before").expect("写普通文件");
        fs::write(root.path().join("ignored.txt"), "hidden").expect("写忽略文件");
        let before = snapshot(root.path(), 20).expect("首次快照");
        assert!(before.entries.contains_key(Path::new("tracked.txt")));
        assert!(!before.entries.contains_key(Path::new("ignored.txt")));
        fs::write(root.path().join("tracked.txt"), "after and longer").expect("修改文件");
        fs::write(root.path().join("new.txt"), "added").expect("新增文件");
        let after = snapshot(root.path(), 20).expect("后续快照");
        assert_eq!(
            diff(&before, &after),
            [
                PathChange {
                    path: "new.txt".into(),
                    kind: "added".into()
                },
                PathChange {
                    path: "tracked.txt".into(),
                    kind: "modified".into()
                },
            ]
        );
    }

    #[test]
    fn git_snapshot_tracks_untracked_paths_and_removal() {
        let root = tempfile::tempdir().expect("临时目录");
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git 命令存在");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "test"]);
        fs::write(root.path().join("tracked.txt"), "base").expect("写 tracked");
        run(&["add", "tracked.txt"]);
        run(&["commit", "-qm", "initial"]);
        let before = snapshot(root.path(), 20).expect("git 初始快照");
        fs::remove_file(root.path().join("tracked.txt")).expect("删除 tracked");
        fs::write(root.path().join("new file.txt"), "new").expect("创建空格路径");
        let after = snapshot(root.path(), 20).expect("git 后续快照");
        assert_eq!(
            diff(&before, &after),
            [
                PathChange {
                    path: "new file.txt".into(),
                    kind: "added".into()
                },
                PathChange {
                    path: "tracked.txt".into(),
                    kind: "removed".into()
                },
            ]
        );
    }

    #[test]
    fn capped_snapshots_report_partial_coverage_without_false_deletions() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            fs::write(root.path().join(name), name).unwrap();
        }
        let full = snapshot(root.path(), 20).unwrap();
        let partial = snapshot(root.path(), 1).unwrap();
        assert!(partial.truncated);
        assert_eq!(
            partial.entries.keys().collect::<Vec<_>>(),
            [&PathBuf::from("a")]
        );
        assert!(diff(&full, &partial).is_empty());
        assert!(diff(&partial, &full).is_empty());
        fs::write(root.path().join("a"), "changed").unwrap();
        assert_eq!(
            diff(&partial, &snapshot(root.path(), 1).unwrap()),
            [PathChange {
                path: "a".into(),
                kind: "modified".into()
            }]
        );
    }

    #[test]
    fn git_subdirectory_keeps_status_paths_relative_and_caps_entries() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        let cwd = root.path().join("nested");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join("one"), "one").unwrap();
        fs::write(cwd.join("two"), "two").unwrap();
        let initial = snapshot(&cwd, 20).unwrap();
        assert_eq!(initial.entries[Path::new("one")].status, "??");
        assert!(
            Command::new("git")
                .args(["add", "one"])
                .current_dir(&cwd)
                .status()
                .unwrap()
                .success()
        );
        let staged = snapshot(&cwd, 20).unwrap();
        assert_eq!(
            diff(&initial, &staged),
            [PathChange {
                path: "one".into(),
                kind: "modified".into()
            }]
        );
        let limited = snapshot(&cwd, 1).unwrap();
        assert!(limited.truncated);
        assert_eq!(limited.entries.len(), 1);
    }
}
