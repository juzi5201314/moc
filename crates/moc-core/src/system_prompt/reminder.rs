//! Volatile time, job, workspace, and read-only reminders.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use moc_protocol::methods::PathChange;

#[derive(Debug, Clone, Default)]
pub struct ReminderState {
    last_day: Option<String>,
}

impl ReminderState {
    /// 返回本次提示待注入的提醒；无新事实则返回 `None`。
    pub fn render(
        &mut self,
        now_local: &str,
        background_jobs: &[BackgroundJob],
        changes: &[PathChange],
        violation: bool,
    ) -> Option<String> {
        let mut messages = Vec::new();
        let day = now_local.get(..10).unwrap_or(now_local);
        if self.last_day.as_deref() != Some(day) {
            messages.push(format!("Current local time: {now_local}."));
            self.last_day = Some(day.to_owned());
        }
        for job in background_jobs {
            messages.push(format!(
                "Background job {} finished ({}). Output tail:\n{}\nFull output: {}",
                job.id,
                job.exit,
                bounded_tail(&job.tail, 40),
                job.log_path.display()
            ));
        }
        if !changes.is_empty() {
            let paths = changes
                .iter()
                .map(|change| format!("{} {}", change.kind, change.path))
                .collect::<Vec<_>>()
                .join(", ");
            if violation {
                messages.push(format!(
                    "You modified the workspace in a read-only mode: {paths}. Stop modifying and explain why to the user."
                ));
            } else {
                messages.push(format!(
                    "The workspace changed outside this prompt: {paths}."
                ));
            }
        }
        (!messages.is_empty()).then(|| {
            format!(
                "<system-reminder>{}</system-reminder>",
                messages.join("\n\n")
            )
        })
    }
}

#[derive(Debug, Clone)]
pub struct BackgroundJob {
    pub id: String,
    pub exit: String,
    pub tail: String,
    pub log_path: std::path::PathBuf,
}

/// 当前机器本地时区时间，仅在日期变化时由 ReminderState 注入。
pub fn local_time() -> String {
    #[cfg(unix)]
    let output = Command::new("date")
        .args(["+%Y-%m-%d %H:%M (%:z)"])
        .output();
    #[cfg(windows)]
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Date -Format 'yyyy-MM-dd HH:mm (zzz)'",
        ])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|output| !output.is_empty())
        .unwrap_or_else(|| "time unavailable".into())
}

/// 合并每种来源只需展示一次的最新工作区变更。
pub fn deduplicate_changes(changes: impl IntoIterator<Item = PathChange>) -> Vec<PathChange> {
    let mut by_path = BTreeMap::new();
    for change in changes {
        by_path.insert(change.path.clone(), change);
    }
    by_path.into_values().collect()
}

fn bounded_tail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// 仅用于展示的 cwd 路径；绝不写入稳定 system 前缀。
pub fn cwd_reminder(cwd: &Path) -> String {
    format!("cwd: {}", cwd.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_time_once_per_day_and_new_job_and_workspace_facts() {
        let mut state = ReminderState::default();
        let first = state
            .render("2026-09-24 18:33 (+08:00)", &[], &[], false)
            .unwrap();
        assert!(first.contains("Current local time"));
        assert!(
            state
                .render("2026-09-24 18:45 (+08:00)", &[], &[], false)
                .is_none()
        );
        let facts = state
            .render(
                "2026-09-24 18:45 (+08:00)",
                &[BackgroundJob {
                    id: "job_4".into(),
                    exit: "exit 0".into(),
                    tail: (0..50)
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    log_path: "/tmp/job_4.log".into(),
                }],
                &[PathChange {
                    path: "src/main.rs".into(),
                    kind: "modified".into(),
                }],
                true,
            )
            .unwrap();
        assert!(!facts.contains("Current local time"));
        assert!(facts.contains("Background job job_4"));
        assert!(!facts.contains("\n8\n"));
        assert!(facts.contains("read-only mode"));
        assert!(
            state
                .render("2026-09-25 00:01 (+08:00)", &[], &[], false)
                .unwrap()
                .contains("Current local time")
        );
    }
}
