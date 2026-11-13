//! 管道与 PTY 的原生退出状态，以及前台/后台共用的终止诊断。

use std::fmt::Write;

pub(crate) struct ProcessExit {
    status: Status,
    pub timed_out: bool,
    pub killed: bool,
    pub sandbox_denied: bool,
    /// scope 创建失败，载荷没有执行。
    limits_failed: bool,
    memory_limited: bool,
}

enum Status {
    Pipe(std::process::ExitStatus),
    Pty(portable_pty::ExitStatus),
}

impl ProcessExit {
    pub(super) fn pipe(
        status: std::process::ExitStatus,
        timed_out: bool,
        tail: &str,
        memory_limited: bool,
    ) -> Self {
        Self::new(Status::Pipe(status), timed_out, tail, memory_limited)
    }

    pub(super) fn pty(
        status: portable_pty::ExitStatus,
        timed_out: bool,
        tail: &str,
        memory_limited: bool,
    ) -> Self {
        Self::new(Status::Pty(status), timed_out, tail, memory_limited)
    }

    fn new(status: Status, timed_out: bool, tail: &str, memory_limited: bool) -> Self {
        Self {
            status,
            timed_out,
            killed: false,
            sandbox_denied: crate::sandbox::denial_in(tail),
            limits_failed: crate::limits::failure_in(tail),
            memory_limited,
        }
    }

    /// 协议约定：信号终止没有退出码，不把 PTY 的占位码 1 当成真实退出码。
    pub(crate) fn code(&self) -> Option<i32> {
        match &self.status {
            Status::Pipe(status) => status.code(),
            Status::Pty(status) if status.signal().is_some() => None,
            Status::Pty(status) => Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
        }
    }

    pub(crate) fn append_diagnostics(&self, output: &mut String) {
        if !self.timed_out && !self.killed {
            match &self.status {
                #[cfg(unix)]
                Status::Pipe(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(signal) = status.signal() {
                        let _ = write!(output, "\n(terminated by signal {signal})");
                    }
                }
                Status::Pty(status) => {
                    if let Some(signal) = status.signal() {
                        let _ = write!(output, "\n(terminated by signal: {signal})");
                    }
                }
                #[cfg(not(unix))]
                Status::Pipe(_) => {}
            }
        }
        if self.limits_failed {
            output.push_str(
                "\n\n[limits: the resource scope could not be created, so the command did not run; disable tools.bash.limits or check `systemd --user`]",
            );
        } else if self.memory_cap_suspected() {
            output.push_str(
                "\n\n[limits: this termination may be related to the configured memory cap; SIGKILL/exit 137 alone does not establish OOM. Check system/cgroup logs; if the cap was reached, raise tools.bash.limits.memory_max_bytes or lower build parallelism.]",
            );
        }
    }

    fn memory_cap_suspected(&self) -> bool {
        !self.timed_out
            && !self.killed
            && !self.limits_failed
            && self.memory_limited
            && (self.code() == Some(137) || self.sigkill())
    }

    fn sigkill(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            match &self.status {
                Status::Pipe(status) => status.signal() == Some(libc::SIGKILL),
                Status::Pty(status) => {
                    // portable-pty 只保留 strsignal 描述；缓存同库生成的参照，不复制每次的字符串。
                    static KILL: std::sync::LazyLock<portable_pty::ExitStatus> =
                        std::sync::LazyLock::new(|| {
                            std::process::ExitStatus::from_raw(libc::SIGKILL).into()
                        });
                    status.signal().is_some() && status.signal() == KILL.signal()
                }
            }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn signal_status_and_cap_suspicion_preserve_termination_precedence() {
        for pty in [false, true] {
            for (raw, cap, timed_out, killed, suspected, code) in [
                (libc::SIGKILL, true, false, false, true, None),
                (libc::SIGTERM, true, false, false, false, None),
                (libc::SIGKILL, false, false, false, false, None),
                (libc::SIGKILL, true, true, false, false, None),
                (libc::SIGKILL, true, false, true, false, None),
                (137 << 8, true, false, false, true, Some(137)),
                (0, true, false, false, false, Some(0)),
            ] {
                let status = std::process::ExitStatus::from_raw(raw);
                let mut exit = if pty {
                    ProcessExit::pty(status.into(), timed_out, "", cap)
                } else {
                    ProcessExit::pipe(status, timed_out, "", cap)
                };
                exit.killed = killed;
                assert_eq!(exit.code(), code);
                assert_eq!(exit.memory_cap_suspected(), suspected);
            }
        }
    }
}
