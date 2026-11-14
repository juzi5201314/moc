//! `/session` 的身份和文件大小汇总；只读取元数据，不扫描工作区或解压历史。

use super::{CoreInner, to_rpc};
use crate::session::store::SessionPaths;
use moc_protocol::{RpcError, SessionInfo};
use std::fmt::Write;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub(super) async fn report(inner: &CoreInner, session_id: &str) -> Result<String, RpcError> {
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let info = record.state.lock().await.session.clone();
    let paths = record.paths.clone();
    tokio::task::spawn_blocking(move || render(&info, &paths))
        .await
        .map_err(|error| to_rpc(crate::CoreError::Session(error.to_string())))?
        .map_err(to_rpc)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Space {
    files: u64,
    bytes: u64,
    incomplete: bool,
}

impl Space {
    fn merge(&mut self, other: &Self) {
        self.files += other.files;
        self.bytes += other.bytes;
        self.incomplete |= other.incomplete;
    }

    fn describe(&self) -> String {
        format!(
            "{} 个文件 · {}{}",
            self.files,
            size(self.bytes),
            if self.incomplete {
                "（统计不完整）"
            } else {
                ""
            }
        )
    }
}

struct Group {
    label: String,
    path: PathBuf,
    space: Space,
}

#[derive(Default)]
struct Inventory {
    groups: Vec<Group>,
    own: Space,
    children: Space,
    cache: Space,
}

fn render(info: &SessionInfo, paths: &SessionPaths) -> Result<String, crate::CoreError> {
    let transcript = std::path::absolute(&paths.transcript)?;
    let sidecar = std::path::absolute(&paths.sidecar)?;
    let mut inventory = Inventory::default();
    sidecar_inventory(&sidecar, false, &mut inventory);
    for name in ["plans", "artifacts", "jobs"] {
        let path = sidecar.join(name);
        if !inventory.groups.iter().any(|group| group.path == path) {
            inventory.groups.push(Group {
                label: group_label(name).to_owned(),
                path,
                space: Space {
                    incomplete: inventory.own.incomplete,
                    ..Space::default()
                },
            });
        }
    }
    let (log, log_state) = match fs::symlink_metadata(&transcript) {
        Ok(metadata) if metadata.is_file() => (
            Space {
                files: 1,
                bytes: metadata.len(),
                incomplete: false,
            },
            "",
        ),
        Ok(_) => (
            Space {
                incomplete: true,
                ..Space::default()
            },
            "非普通文件，未计入",
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Space::default(), "尚未落盘"),
        Err(_) => (
            Space {
                incomplete: true,
                ..Space::default()
            },
            "无法读取大小",
        ),
    };
    inventory.own.merge(&log);
    inventory
        .groups
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let mut text = format!(
        "会话信息\nID：{}\n标题：{}\n工作目录：{}\n创建时间：{}\n最近更新：{}\n",
        info.id,
        info.title,
        info.cwd,
        timestamp(info.created_at_ms),
        timestamp(info.updated_at_ms)
    );
    if let Some(parent) = &info.parent_session_id {
        let _ = writeln!(text, "父会话：{parent}");
    }
    if let Some(source) = &info.forked_from {
        let _ = writeln!(text, "分叉来源：{source}");
    }
    let _ = writeln!(text, "\n会话文件：{}", transcript.display());
    let _ = writeln!(
        text,
        "文件大小：{}{}{}",
        size(log.bytes),
        if log_state.is_empty() { "" } else { " · " },
        log_state
    );
    let _ = writeln!(
        text,
        "格式：{}",
        if paths.compressed() {
            "JSONL · zstd 压缩"
        } else {
            "JSONL · 未压缩"
        }
    );
    let _ = writeln!(text, "附属目录：{}", sidecar.display());
    for group in &inventory.groups {
        let _ = writeln!(
            text,
            "\n{}：{}\n  {}",
            group.label,
            group.space.describe(),
            group.path.display()
        );
    }
    let _ = writeln!(text, "\n自身持久数据：{}", inventory.own.describe());
    if inventory.children.files != 0 || inventory.children.incomplete {
        let _ = writeln!(text, "子会话持久数据：{}", inventory.children.describe());
        let _ = writeln!(text, "  {} 下的子会话日志与目录", sidecar.display());
    }
    if inventory.cache.files != 0 || inventory.cache.incomplete {
        let _ = writeln!(text, "临时缓存合计：{}", inventory.cache.describe());
        let _ = writeln!(text, "  会话及子会话附属目录中的 history/、urlfs/");
    }
    text.push_str("文件大小合计不含临时缓存，不跟随符号链接。");
    Ok(text)
}

fn sidecar_inventory(root: &Path, child: bool, inventory: &mut Inventory) {
    if fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_symlink()) {
        if child {
            &mut inventory.children
        } else {
            &mut inventory.own
        }
        .incomplete = true;
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            if child {
                &mut inventory.children
            } else {
                &mut inventory.own
            }
            .incomplete = true;
            return;
        }
    };
    // 子会话只出现在 sidecar 的直接下一层；产物目录里的同名文件夹不是子会话。
    for entry in entries {
        let Ok(entry) = entry else {
            if child {
                &mut inventory.children
            } else {
                &mut inventory.own
            }
            .incomplete = true;
            continue;
        };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(_) => {
                if child {
                    &mut inventory.children
                } else {
                    &mut inventory.own
                }
                .incomplete = true;
                continue;
            }
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() && name.starts_with(crate::ids::SESSION_PREFIX) {
            sidecar_inventory(&path, true, inventory);
            continue;
        }
        let space = measure(&path);
        let cache = kind.is_dir() && matches!(name.as_ref(), "history" | "urlfs");
        let child_log =
            kind.is_file() && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"));
        if cache {
            inventory.cache.merge(&space);
        } else if child || child_log {
            inventory.children.merge(&space);
        } else {
            inventory.own.merge(&space);
        }
        if !child && !child_log {
            inventory.groups.push(Group {
                label: group_label(&name).to_owned(),
                path,
                space,
            });
        }
    }
}

fn group_label(name: &str) -> &str {
    match name {
        "plans" => "计划文件 · local://",
        "artifacts" => "工具产物 · artifact://artifacts/",
        "jobs" => "任务输出 · artifact://jobs/",
        "compaction" => "压缩数据",
        "workflows" => "工作流数据",
        "plugins" => "插件状态",
        "history" => "历史临时缓存",
        "urlfs" => "URI 映射缓存",
        _ => name,
    }
}

fn measure(root: &Path) -> Space {
    let mut space = Space::default();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => {
                space.incomplete = true;
                continue;
            }
        };
        if metadata.is_file() {
            space.files += 1;
            space.bytes += metadata.len();
        } else if metadata.is_dir() {
            match fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => pending.push(entry.path()),
                            Err(_) => space.incomplete = true,
                        }
                    }
                }
                Err(_) => space.incomplete = true,
            }
        }
    }
    space
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut divisor = 1u128;
    for unit in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        divisor *= 1024;
        if u128::from(bytes) < divisor * 1024 {
            let hundredths = (u128::from(bytes) * 100 + divisor / 2) / divisor;
            return format!("{}.{:02} {unit}", hundredths / 100, hundredths % 100);
        }
    }
    unreachable!("u64 字节数不超过 EiB 单位的表示范围")
}

fn timestamp(at_ms: u64) -> String {
    let seconds = at_ms / 1000;
    let days = i64::try_from(seconds / 86_400).expect("u64 毫秒换算的天数不超过 i64");
    let (year, month, day) = crate::session::store::civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_separates_descendants_caches_and_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let sidecar = root.path().join("ses_parent");
        for (name, bytes) in [
            ("plans/plan.md", 3),
            ("artifacts/output.txt", 5),
            ("workflows/run/artifacts/ses_not_a_child/data", 7),
            ("2026_ses_child.jsonl.zst", 11),
            ("ses_child/jobs/out", 13),
            ("ses_child/2026_ses_grandchild.jsonl", 17),
            ("ses_child/ses_grandchild/artifacts/out", 19),
            ("history/view/data", 23),
            ("ses_child/history/view/data", 29),
        ] {
            let path = sidecar.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![b'x'; bytes]).unwrap();
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&sidecar, sidecar.join("artifacts/loop")).unwrap();
            std::os::unix::fs::symlink(
                sidecar.join("plans/plan.md"),
                sidecar.join("artifacts/copy"),
            )
            .unwrap();
        }
        let mut inventory = Inventory::default();
        sidecar_inventory(&sidecar, false, &mut inventory);
        assert_eq!(
            inventory.own,
            Space {
                files: 3,
                bytes: 15,
                incomplete: false
            }
        );
        assert_eq!(
            inventory.children,
            Space {
                files: 4,
                bytes: 60,
                incomplete: false
            }
        );
        assert_eq!(
            inventory.cache,
            Space {
                files: 2,
                bytes: 52,
                incomplete: false
            }
        );
        assert_eq!(measure(&sidecar).bytes, 127);
        let file = sidecar.join("plans/plan.md");
        let mut invalid = Inventory::default();
        sidecar_inventory(&file, false, &mut invalid);
        assert!(
            invalid.own.incomplete,
            "不可遍历的目录不能伪装成完整的零值统计"
        );
    }
}
