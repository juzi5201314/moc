//! 会话所有的后台进程、日志、等待与进程树取消。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::SessionUpdate;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Notify;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Finished(Option<i32>),
    Killing,
    Killed,
    TimedOut,
    OutputLimited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLifecycle {
    Monitored,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOrigin {
    Explicit,
    AutoBackground,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub command: String,
    pub path: PathBuf,
    pub status: JobStatus,
    pub lifecycle: JobLifecycle,
    pub origin: JobOrigin,
    pub started_at: std::time::Instant,
    pub hard_deadline_secs: Option<u64>,
    pub warning_count: u32,
    pub log_truncated: bool,
    pid: Option<u32>,
    group: Option<super::bash::ProcessGroup>,
    log: super::job_log::JobLog,
}

#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<String, JobInfo>>,
    changed: Notify,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    shutting_down: AtomicBool,
    pub(crate) slots: Arc<crate::concurrency::Gate>,
}

/// 已结束任务的保留数量；更早的任务随驱逐释放日志句柄与环形缓冲。
const RETAINED_FINISHED_JOBS: usize = 16;

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 驱逐超出保留上限的已结束任务：释放 JobLog 句柄与 persistent 环形缓冲。
    ///
    /// 运行中与正在终止的任务不受影响；按开始时间淘汰最旧的已结束任务。
    fn evict_finished(&self, jobs: &mut HashMap<String, JobInfo>) {
        let mut finished: Vec<(std::time::Instant, String)> = jobs
            .values()
            .filter(|job| !matches!(job.status, JobStatus::Running | JobStatus::Killing))
            .map(|job| (job.started_at, job.id.clone()))
            .collect();
        if finished.len() <= RETAINED_FINISHED_JOBS {
            return;
        }
        finished.sort_by_key(|(started_at, _)| *started_at);
        let excess = finished.len() - RETAINED_FINISHED_JOBS;
        for (_, id) in finished.into_iter().take(excess) {
            jobs.remove(&id);
        }
    }

    pub(crate) fn with_slots(slots: Arc<crate::concurrency::Gate>) -> Self {
        Self {
            slots,
            ..Self::default()
        }
    }

    pub(crate) async fn reserve(
        &self,
        config: &crate::config::JobConfig,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::concurrency::Permit, ToolError> {
        match config.overflow {
            crate::config::JobOverflow::Error => {
                self.slots.try_acquire(config.max_running).ok_or_else(|| {
                    ToolError::new(
                        "E_JOB_LIMIT",
                        format!(
                            "{} background processes are already running; wait or finish an existing job",
                            config.max_running
                        ),
                    )
                })
            }
            crate::config::JobOverflow::Wait => self
                .slots
                .acquire(config.max_running, cancel)
                .await
                .ok_or_else(|| ToolError::new("E_CANCELLED", "waiting for a background-process slot was cancelled")),
        }
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        ctx: ToolCtx,
        id: String,
        command: String,
        log_path: PathBuf,
        log: super::job_log::JobLog,
        process: super::bash::Process,
        permit: crate::concurrency::Permit,
        lifecycle: JobLifecycle,
        origin: JobOrigin,
        hard_deadline: Option<Duration>,
    ) -> Result<(), ToolError> {
        if self.shutting_down.load(Ordering::Acquire) {
            process.group.kill()?;
            return Err(ToolError::new(
                "E_CANCELLED",
                "the session is closing; cannot start a background task",
            ));
        }
        let super::bash::Process {
            pid,
            group,
            started_at,
            completion,
        } = process;
        self.jobs.lock().insert(
            id.clone(),
            JobInfo {
                id: id.clone(),
                command: command.clone(),
                path: log_path.clone(),
                status: JobStatus::Running,
                lifecycle,
                origin,
                started_at,
                hard_deadline_secs: hard_deadline.map(|value| value.as_secs()),
                warning_count: 0,
                log_truncated: false,
                pid,
                group: Some(group),
                log: log.clone(),
            },
        );
        let manager = Arc::clone(self);
        let worker = tokio::spawn(async move {
            let waited = {
                let waiting = async {
                    if lifecycle == JobLifecycle::Monitored {
                        manager.wait_with_warnings(&ctx, &id, completion).await
                    } else {
                        completion.await
                    }
                };
                tokio::pin!(waiting);
                tokio::select! {
                    result = &mut waiting => result,
                    _ = ctx.guard.stopped.cancelled() => {
                        let killed = {
                            let mut jobs = manager.jobs.lock();
                            let job = jobs.get_mut(&id).expect("后台 worker 持有已注册任务");
                            job.status = JobStatus::Killing;
                            job.group.as_ref().expect("运行中的任务持有进程组").kill()
                        };
                        let result = waiting.await;
                        killed.and(result)
                    }
                }
            };
            drop(permit);
            manager
                .finish_job(ctx, id, command, log_path, log, lifecycle, origin, waited)
                .await;
        });
        let mut workers = self.workers.lock();
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
        Ok(())
    }

    async fn wait_with_warnings(
        &self,
        ctx: &ToolCtx,
        id: &str,
        mut completion: futures_util::future::BoxFuture<
            'static,
            Result<super::bash::ProcessExit, ToolError>,
        >,
    ) -> Result<super::bash::ProcessExit, ToolError> {
        let interval = Duration::from_secs(ctx.config.tools.bash.slow_warning_interval_secs);
        let mut next_warning = Box::pin(tokio::time::sleep(interval));
        loop {
            tokio::select! {
                result = &mut completion => return result,
                _ = &mut next_warning => {
                    let warning_data = {
                        let mut jobs = self.jobs.lock();
                        jobs.get_mut(id).map(|job| {
                            job.warning_count = job.warning_count.saturating_add(1);
                            (job.started_at.elapsed().as_secs(), job.warning_count)
                        })
                    };
                    let Some((elapsed, warning_count)) = warning_data else { return completion.await; };
                    let notice = format!(
                        "Background job {id} has run for {elapsed}s (monitored, warning {warning_count}); it may be stuck or hitting a slow filesystem. Check output or kill it; do not repeat wait without diagnosis."
                    );
                    ctx.state.lock().await.notices.push(notice);
                    next_warning.as_mut().reset(tokio::time::Instant::now() + interval);
                }
            }
        }
    }

    async fn finish_job(
        &self,
        ctx: ToolCtx,
        id: String,
        command: String,
        log_path: PathBuf,
        log: super::job_log::JobLog,
        lifecycle: JobLifecycle,
        _origin: JobOrigin,
        waited: Result<super::bash::ProcessExit, ToolError>,
    ) {
        let status = {
            let mut jobs = self.jobs.lock();
            let status = if jobs
                .get(&id)
                .is_some_and(|job| job.status == JobStatus::Killing)
            {
                JobStatus::Killed
            } else {
                match waited {
                    Ok(result) if result.timed_out => JobStatus::TimedOut,
                    Ok(result) if result.output_truncated => JobStatus::OutputLimited,
                    Ok(result) => JobStatus::Finished(result.code),
                    Err(error) => {
                        tracing::warn!(job_id = %id, %error, "后台任务等待失败");
                        JobStatus::Finished(None)
                    }
                }
            };
            if let Some(job) = jobs.get_mut(&id) {
                job.status = status;
                job.log_truncated = log.log_truncated();
                job.group.take();
            }
            self.evict_finished(&mut jobs);
            status
        };
        self.changed.notify_waiters();
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let (exit_code, exit_label) = match status {
            JobStatus::Finished(code) => (code, format!("exit {}", code.unwrap_or(-1))),
            JobStatus::Killed => (None, "killed".into()),
            JobStatus::TimedOut => (None, "timeout".into()),
            JobStatus::OutputLimited => (None, "output_limit".into()),
            JobStatus::Running | JobStatus::Killing => unreachable!("任务已回收"),
        };
        let output = {
            let (text, truncated) = log.read_tail(200).unwrap_or_else(|error| {
                (format!("[E_JOB_LOG] failed to read log: {error}"), false)
            });
            if truncated {
                format!("[log_truncated=true]\n{text}")
            } else {
                text
            }
        };
        let log_uri = ctx
            .artifact_store()
            .uri_for_path(&log_path)
            .unwrap_or_else(|error| format!("[{}] {}", error.code, error.message));
        ctx.state.lock().await.notices.push(format!("Background {lifecycle:?} job {id} finished ({exit_label}); output preview:\n{output}\nFull output: {log_uri}"));
        let update = SessionUpdate::JobFinished {
            job_id: id.clone(),
            command,
            exit_code,
            log_path: log_uri,
        };
        if let Err(error) = ctx.request.updates().emit(ctx.session_id, update).await {
            tracing::warn!(job_id = %id, %error, "发送后台任务完成通知失败");
        }
    }
    pub async fn kill_all(&self) {
        self.shutting_down.store(true, Ordering::Release);
        for job in self.jobs.lock().values_mut() {
            if job.status == JobStatus::Running {
                job.status = JobStatus::Killing;
                if let Err(error) = job
                    .group
                    .as_ref()
                    .expect("运行中的后台任务必须持有进程组")
                    .kill()
                {
                    tracing::warn!(job_id = %job.id, pid = ?job.pid, %error, "终止后台任务失败");
                }
            }
        }
        let workers = std::mem::take(&mut *self.workers.lock());
        for worker in workers {
            if let Err(error) = worker.await {
                tracing::warn!(%error, "后台任务回收失败");
            }
        }
    }

    pub async fn list(&self) -> Vec<JobInfo> {
        let mut jobs: Vec<_> = self.jobs.lock().values().cloned().collect();
        jobs.sort_by(|left, right| left.id.cmp(&right.id));
        jobs
    }

    pub async fn info(&self, id: &str) -> Option<JobInfo> {
        self.jobs.lock().get(id).cloned()
    }

    pub async fn output(&self, id: &str, lines: usize) -> Result<ToolOutput, ToolError> {
        let job = self
            .info(id)
            .await
            .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?;
        let (mut text, truncated) = job
            .log
            .read_tail(lines)
            .map_err(|error| ToolError::new("E_JOB_LOG", error.to_string()))?;
        if truncated {
            text.insert_str(0, "[log_truncated=true; oldest output was overwritten]\n");
        }
        Ok(ToolOutput::Excerpt {
            text,
            path: job.path,
        })
    }

    pub async fn kill(&self, id: &str) -> Result<JobStatus, ToolError> {
        {
            let mut jobs = self.jobs.lock();
            let job = jobs
                .get_mut(id)
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?;
            if job.status == JobStatus::Running {
                job.group
                    .as_ref()
                    .expect("运行中的后台任务必须持有进程组")
                    .kill()?;
                job.status = JobStatus::Killing;
            }
        }
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let Some(status) = self.info(id).await.map(|job| job.status) else {
                // 任务已结束并被驱逐（超过保留上限）：终止已经生效。
                return Ok(JobStatus::Killed);
            };
            if !matches!(status, JobStatus::Running | JobStatus::Killing) {
                return Ok(status);
            }
            notified.await;
        }
    }

    pub async fn wait(&self, id: &str, timeout: Duration) -> Result<JobStatus, ToolError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let status = self
                .info(id)
                .await
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?
                .status;
            if !matches!(status, JobStatus::Running | JobStatus::Killing) {
                return Ok(status);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Ok(JobStatus::Running);
            }
        }
    }
}

pub(crate) async fn copy_output<R: AsyncRead + Unpin>(
    mut reader: R,
    log: super::job_log::JobLog,
    group: super::bash::ProcessGroup,
) -> std::io::Result<bool> {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            log.finish()?;
            return Ok(false);
        }
        if log.append(&buffer[..read])? {
            group.kill().map_err(std::io::Error::other)?;
            log.finish()?;
            return Ok(true);
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JobArgs {
    op: String,
    job_id: Option<String>,
    tail_lines: Option<usize>,
    timeout_secs: Option<u64>,
}

pub struct JobTool;

impl Tool for JobTool {
    fn name(&self) -> &'static str {
        "job"
    }
    fn description(&self) -> &'static str {
        "Manage background shell jobs: list, output, kill, or wait."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        true
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "job",
            schema(
                serde_json::json!({
                    "op":{"type":"string","enum":["list","output","kill","wait"]},
                    "job_id":{"type":"string"},
                    "tail_lines":{"type":"integer","minimum":1,"maximum":2000},
                    "timeout_secs":{"type":"integer","minimum":1,"maximum":60}
                }),
                &["op"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: JobArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let output: ToolOutput = match args.op.as_str() {
                "list" => {
                    let jobs = ctx.jobs.list().await;
                    if jobs.is_empty() {
                        "No background jobs.".into()
                    } else {
                        jobs.iter()
                            .map(|job| {
                                let uri = ctx
                                    .artifact_store()
                                    .uri_for_path(&job.path)
                                    .unwrap_or_else(|error| {
                                        format!("[{}] {}", error.code, error.message)
                                    });
                                format!(
                                    "{}\tlifecycle={:?}\torigin={:?}\tstatus={:?}\telapsed_secs={}\twarnings={}\tlog_truncated={}\t{}",
                                    job.id, job.lifecycle, job.origin, job.status,
                                    job.started_at.elapsed().as_secs(), job.warning_count,
                                    job.log_truncated, uri
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into()
                    }
                }
                "output" => {
                    let id = require_id(args.job_id)?;
                    ctx.jobs
                        .output(&id, args.tail_lines.unwrap_or(200).clamp(1, 2000))
                        .await?
                }
                "kill" => {
                    let id = require_id(args.job_id)?;
                    format!("{:?}", ctx.jobs.kill(&id).await?).into()
                }
                "wait" => {
                    let id = require_id(args.job_id)?;
                    let job = ctx.jobs.info(&id).await.ok_or_else(|| {
                        ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`"))
                    })?;
                    let status = ctx
                        .jobs
                        .wait(
                            &id,
                            Duration::from_secs(args.timeout_secs.unwrap_or(60).min(60)),
                        )
                        .await?;
                    let mut output = ctx
                        .jobs
                        .output(&id, args.tail_lines.unwrap_or(200).clamp(1, 2000))
                        .await?;
                    let elapsed_secs = job.started_at.elapsed().as_secs();
                    let slow = job.lifecycle == JobLifecycle::Monitored
                        && elapsed_secs >= ctx.config.tools.bash.slow_warning_interval_secs;
                    let message = if status == JobStatus::Running {
                        if job.lifecycle == JobLifecycle::Persistent {
                            "persistent job will not auto-complete; use output or kill"
                        } else {
                            "monitored job is still running; inspect output or kill it instead of repeating wait without diagnosis"
                        }
                    } else {
                        "job finished"
                    };
                    let structured = serde_json::json!({
                        "status": format!("{status:?}"), "lifecycle": format!("{:?}", job.lifecycle),
                        "origin": format!("{:?}", job.origin), "elapsed_secs": elapsed_secs,
                        "warning_count": job.warning_count, "slow": slow,
                        "hard_deadline_secs": job.hard_deadline_secs, "log_truncated": job.log_truncated,
                        "message": message,
                    });
                    if let ToolOutput::Excerpt { text, .. } = &mut output {
                        text.insert_str(0, &format!("{status:?}\n{message}\n"));
                    }
                    let mut outcome = ToolOutcome::success(output);
                    outcome.structured_content = Some(structured);
                    return Ok(outcome);
                }
                _ => {
                    return Err(ToolError::new(
                        "E_BAD_PAYLOAD",
                        "job op must be list/output/kill/wait",
                    ));
                }
            };
            Ok(ToolOutcome::success(output))
        })
    }
}

fn require_id(id: Option<String>) -> Result<String, ToolError> {
    id.filter(|id| !id.is_empty())
        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "this job operation requires job_id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(
        directory: &tempfile::TempDir,
        id: &str,
        started_at: std::time::Instant,
        status: JobStatus,
    ) -> JobInfo {
        let path = directory.path().join(format!("{id}.log"));
        let log = super::super::job_log::JobLog::create(
            path.clone(),
            super::super::job_log::JobLogMode::Persistent,
            1024,
        )
        .expect("创建日志");
        JobInfo {
            id: id.into(),
            command: "sleep".into(),
            path,
            status,
            lifecycle: JobLifecycle::Monitored,
            origin: JobOrigin::Explicit,
            started_at,
            hard_deadline_secs: None,
            warning_count: 0,
            log_truncated: false,
            pid: None,
            group: None,
            log,
        }
    }

    #[test]
    fn finished_jobs_beyond_the_retention_cap_are_evicted() {
        let directory = tempfile::tempdir().expect("临时目录");
        let manager = JobManager::new();
        let start = std::time::Instant::now();
        let mut jobs = manager.jobs.lock();
        jobs.insert(
            "running".into(),
            job(&directory, "running", start, JobStatus::Running),
        );
        for index in 0..RETAINED_FINISHED_JOBS + 3 {
            let id = format!("finished_{index:02}");
            jobs.insert(
                id.clone(),
                job(
                    &directory,
                    &id,
                    start + Duration::from_secs(index as u64),
                    JobStatus::Finished(Some(0)),
                ),
            );
        }
        manager.evict_finished(&mut jobs);
        assert_eq!(jobs.len(), RETAINED_FINISHED_JOBS + 1);
        assert!(jobs.contains_key("running"), "运行中的任务不得被驱逐");
        assert!(!jobs.contains_key("finished_00") && !jobs.contains_key("finished_02"));
        assert!(jobs.contains_key("finished_03"));
        assert!(jobs.contains_key(&format!("finished_{RETAINED_FINISHED_JOBS:02}")));
    }

    #[test]
    fn read_tail_returns_last_lines_within_bound() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("job.log");
        let log = super::super::job_log::JobLog::create(
            path,
            super::super::job_log::JobLogMode::Persistent,
            1024 * 1024,
        )
        .expect("创建日志");
        let content = (0..2500)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        log.append(content.as_bytes()).expect("写入日志");
        let (output, truncated) = log.read_tail(2000).expect("读取日志");
        assert!(!truncated);
        assert!(output.starts_with("line 500"));
        assert!(output.ends_with("line 2499"));
    }
}
