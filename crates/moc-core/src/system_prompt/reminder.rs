//! 本地时间提醒。
//!
//! 后台任务完成和工作区变更通过会话 notice 告知模型（见 `tools/job.rs`、
//! `service/prompt/workspace.rs`），这里不重复。

use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct ReminderState {
    last_day: Option<String>,
}

impl ReminderState {
    /// 本地日期变化时返回时间提醒，否则返回 `None`。
    pub fn render(&mut self, now_local: &str) -> Option<String> {
        let day = now_local.get(..10).unwrap_or(now_local);
        if self.last_day.as_deref() == Some(day) {
            return None;
        }
        self.last_day = Some(day.to_owned());
        Some(format!(
            "<system-reminder>Current local time: {now_local}.</system-reminder>"
        ))
    }
}

/// 本机本地时区的当前时间；`ReminderState` 只在日期变化时注入。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_time_once_per_local_day() {
        let mut state = ReminderState::default();
        assert!(
            state
                .render("2026-09-24 18:33 (+08:00)")
                .unwrap()
                .contains("Current local time")
        );
        assert!(state.render("2026-09-24 18:45 (+08:00)").is_none());
        assert!(
            state
                .render("2026-09-25 00:01 (+08:00)")
                .unwrap()
                .contains("Current local time")
        );
    }
}
