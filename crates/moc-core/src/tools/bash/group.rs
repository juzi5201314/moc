use std::sync::Arc;

use crate::tools::ToolError;

#[cfg(windows)]
use crate::limits::BashLimits;

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    #[link_name = "NtResumeProcess"]
    fn nt_resume_process(process_handle: std::os::windows::io::RawHandle) -> i32;
}

/// 跨平台的进程树句柄：Unix 用独立进程组，Windows 用 Job Object。
/// 用 Arc 包装，完成 future 和 JobManager 终止的是同一组进程。
#[derive(Debug, Clone)]
pub(crate) struct ProcessGroup(Arc<ProcessGroupInner>);

#[derive(Debug)]
enum ProcessGroupInner {
    None,
    #[cfg(unix)]
    Unix(u32),
    #[cfg(windows)]
    Windows(std::os::windows::io::OwnedHandle),
}

impl ProcessGroup {
    #[cfg(unix)]
    pub(crate) fn new(pid: Option<u32>) -> Result<Self, ToolError> {
        let inner = create_group(pid)?;
        Ok(Self(Arc::new(inner)))
    }

    #[cfg(windows)]
    pub(crate) fn new_windows(limits: Option<BashLimits>) -> Result<Self, ToolError> {
        Ok(Self(Arc::new(ProcessGroupInner::Windows(
            create_windows_job(limits)?,
        ))))
    }

    #[cfg(windows)]
    pub(crate) fn for_suspended_spawn(
        command: &mut tokio::process::Command,
        limits: Option<BashLimits>,
    ) -> Result<Self, ToolError> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        let group = Self::new_windows(limits)?;
        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
        Ok(group)
    }

    #[cfg(windows)]
    pub(crate) fn assign_running_process(
        &self,
        process_handle: std::os::windows::io::RawHandle,
    ) -> Result<(), ToolError> {
        let ProcessGroupInner::Windows(job) = self.0.as_ref() else {
            return Err(ToolError::new(
                "E_BASH_SPAWN",
                "Windows Job Object is not initialized",
            ));
        };
        assign_windows_job(job, process_handle)
    }

    #[cfg(windows)]
    pub(crate) fn assign_and_resume_suspended_process(
        &self,
        process_handle: std::os::windows::io::RawHandle,
    ) -> Result<(), ToolError> {
        self.assign_running_process(process_handle)?;
        // SAFETY: 子进程以 CREATE_SUSPENDED 启动且已加入 Job，句柄仍由 Tokio 子进程持有。
        let status = unsafe { nt_resume_process(process_handle) };
        if status < 0 {
            if let ProcessGroupInner::Windows(job) = self.0.as_ref() {
                let _ = kill_windows(job);
            }
            let _ = wait_windows_process(process_handle);
            return Err(ToolError::new(
                "E_BASH_SPAWN",
                format!("failed to resume the Windows child process: NTSTATUS {status:#x}"),
            ));
        }
        Ok(())
    }

    pub(crate) fn kill(&self) -> Result<(), ToolError> {
        self.0.kill()
    }
}

impl ProcessGroupInner {
    fn kill(&self) -> Result<(), ToolError> {
        match self {
            Self::None => Ok(()),
            #[cfg(unix)]
            Self::Unix(pid) => kill_unix(*pid),
            #[cfg(windows)]
            Self::Windows(job) => kill_windows(job),
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupInner {
    fn drop(&mut self) {
        if let Err(error) = self.kill() {
            tracing::warn!(%error, "回收进程组失败");
        }
    }
}

#[cfg(unix)]
fn create_group(pid: Option<u32>) -> Result<ProcessGroupInner, ToolError> {
    Ok(pid.map_or(ProcessGroupInner::None, ProcessGroupInner::Unix))
}

#[cfg(unix)]
fn kill_unix(pid: u32) -> Result<(), ToolError> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| ToolError::new("E_BASH_KILL", "child-process PID exceeds the system range"))?;
    // SAFETY: 每条命令独占进程组；负 PID 只指向该命令及其子孙进程。
    let result = unsafe { libc::kill(-pid, libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(ToolError::new("E_BASH_KILL", error.to_string()));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn create_windows_job(
    limits: Option<BashLimits>,
) -> Result<std::os::windows::io::OwnedHandle, ToolError> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    // SAFETY: 传入空属性和空名称，创建匿名 Job Object。
    let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw_job.is_null() {
        return Err(ToolError::new(
            "E_BASH_SPAWN",
            format!(
                "failed to create Windows Job Object: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: CreateJobObjectW 返回的非空句柄由当前线程独占，交给 OwnedHandle 接管。
    let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: info 在本地存活，指针与长度覆盖完整结构体。
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&info).cast::<std::ffi::c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(ToolError::new(
            "E_BASH_SPAWN",
            format!(
                "failed to configure Windows Job Object: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    if let Some(limits) = limits
        && let Err(error) = apply_cpu_limit(&job, limits)
    {
        tracing::warn!(%error, "Windows CPU 限速设置失败；本次命令将在不限速下执行");
    }
    Ok(job)
}

/// CPU 硬限速：`CpuRate` 是占全部逻辑处理器时间的 1/100 百分比。
#[cfg(windows)]
fn apply_cpu_limit(
    job: &std::os::windows::io::OwnedHandle,
    limits: BashLimits,
) -> Result<(), ToolError> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JobObjectCpuRateControlInformation,
        SetInformationJobObject,
    };

    let total = std::thread::available_parallelism().map_or(1, |value| value.get());
    let Some(rate) = crate::limits::windows_cpu_rate(limits, total) else {
        return Ok(());
    };
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    info.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
    // 写 union 字段不需要 unsafe；CpuRate 是 Copy 值，不涉及析构。
    info.Anonymous.CpuRate = rate;
    // SAFETY: info 在本地存活，指针与长度覆盖完整结构体。
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectCpuRateControlInformation,
            std::ptr::from_ref(&info).cast::<std::ffi::c_void>(),
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(ToolError::new(
            "E_BASH_LIMITS",
            format!(
                "failed to apply the Windows CPU rate limit: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn assign_windows_job(
    job: &std::os::windows::io::OwnedHandle,
    process_handle: std::os::windows::io::RawHandle,
) -> Result<(), ToolError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    // SAFETY: 两个句柄在调用期间均有效，调用只修改 Job Object 归属。
    let assigned = unsafe { AssignProcessToJobObject(job.as_raw_handle(), process_handle) };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        terminate_unassigned_process(process_handle);
        return Err(ToolError::new(
            "E_BASH_SPAWN",
            format!("failed to add the child process to the Windows Job Object: {error}"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn wait_windows_process(process_handle: std::os::windows::io::RawHandle) -> Result<(), ToolError> {
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: CreateProcess 返回的句柄可等待，5 秒上限用于失败清理。
    let wait = unsafe { WaitForSingleObject(process_handle, 5_000) };
    if wait != 0 {
        return Err(ToolError::new(
            "E_BASH_SPAWN",
            format!("failed waiting for the Windows child process to exit: {wait}"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_unassigned_process(process_handle: std::os::windows::io::RawHandle) {
    use windows_sys::Win32::System::Threading::TerminateProcess;

    // SAFETY: process_handle 是仍由调用者持有的子进程句柄。
    if unsafe { TerminateProcess(process_handle, 1) } == 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "创建 Windows Job Object 失败，且无法终止子进程"
        );
        return;
    }
    if let Err(error) = wait_windows_process(process_handle) {
        tracing::warn!(%error, "创建 Windows Job Object 失败，且没能及时回收子进程");
    }
}

#[cfg(windows)]
fn kill_windows(job: &std::os::windows::io::OwnedHandle) -> Result<(), ToolError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    // SAFETY: job 由当前进程持有，终止操作只作用于该 Job Object。
    let terminated = unsafe { TerminateJobObject(job.as_raw_handle(), 1) };
    if terminated == 0 {
        return Err(ToolError::new(
            "E_BASH_KILL",
            format!(
                "failed to terminate the Windows Job Object: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ProcessGroup;

    #[cfg(unix)]
    #[test]
    fn empty_unix_group_is_safe_to_kill() {
        ProcessGroup::new(None).unwrap().kill().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn empty_windows_job_can_be_created_and_dropped() {
        drop(ProcessGroup::new_windows(None).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_accepts_a_cpu_rate_limit() {
        let limits = crate::limits::BashLimits {
            cpu_cores: 1,
            memory_high_bytes: 0,
            memory_max_bytes: 0,
            memory_swap_bytes: 0,
        };
        drop(ProcessGroup::new_windows(Some(limits)).unwrap());
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn suspended_windows_process_waits_before_running() {
        let directory = tempfile::tempdir().expect("临时目录");
        let marker = directory.path().join("started.txt");
        let mut command = tokio::process::Command::new(
            std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()),
        );
        command
            .args(["/D", "/S", "/C"])
            .arg(format!("echo ready > \"{}\"", marker.display()));
        let _group = ProcessGroup::for_suspended_spawn(&mut command, None).expect("创建暂停子进程");
        let mut child = command.spawn().expect("启动暂停子进程");

        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        assert!(!marker.exists());
        let handle = child.raw_handle().expect("子进程句柄");
        // SAFETY: 测试子进程仍由 Tokio 子进程持有且保持暂停状态。
        let status = unsafe { super::nt_resume_process(handle) };
        assert!(status >= 0, "恢复子进程失败：{status:#x}");
        assert!(child.wait().await.expect("等待子进程").success());
        assert!(marker.exists());
    }
}
