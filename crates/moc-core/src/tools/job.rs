//! 会话所有的后台进程、日志、等待与进程树取消。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::SessionUpdate;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Finished(Option<i32>),
    Killing,
    Killed,
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub command: String,
    pub path: PathBuf,
    pub status: JobStatus,
    pid: Option<u32>,
    group: Option<super::bash::ProcessGroup>,
}

#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<String, JobInfo>>,
    changed: Notify,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    shutting_down: AtomicBool,
    pub(crate) slots: Arc<crate::concurrency::Gate>,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
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
                            "后台进程已达 {} 个；请等待或结束已有 job 后再试",
                            config.max_running
                        ),
                    )
                })
            }
            crate::config::JobOverflow::Wait => self
                .slots
                .acquire(config.max_running, cancel)
                .await
                .ok_or_else(|| ToolError::new("E_CANCELLED", "等待后台进程空位已取消")),
        }
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        ctx: ToolCtx,
        id: String,
        command: String,
        log_path: PathBuf,
        process: super::bash::Process,
        permit: crate::concurrency::Permit,
    ) -> Result<(), ToolError> {
        if self.shutting_down.load(Ordering::Acquire) {
            process.group.kill()?;
            return Err(ToolError::new(
                "E_CANCELLED",
                "会话正在关闭，不能启动后台任务",
            ));
        }
        let super::bash::Process {
            pid,
            group,
            completion,
        } = process;
        self.jobs.lock().insert(
            id.clone(),
            JobInfo {
                id: id.clone(),
                command: command.clone(),
                path: log_path.clone(),
                status: JobStatus::Running,
                pid,
                group: Some(group),
            },
        );
        let manager = Arc::clone(self);
        let worker = tokio::spawn(async move {
            let permit = permit;
            let waited = completion.await;
            drop(permit);
            let status = {
                let mut jobs = manager.jobs.lock();
                let status = if jobs
                    .get(&id)
                    .is_some_and(|job| job.status == JobStatus::Killing)
                {
                    JobStatus::Killed
                } else {
                    match waited {
                        Ok(result) if result.timed_out => JobStatus::TimedOut,
                        Ok(result) => JobStatus::Finished(result.code),
                        Err(error) => {
                            tracing::warn!(job_id = %id, %error, "后台任务等待失败");
                            JobStatus::Finished(None)
                        }
                    }
                };
                if let Some(job) = jobs.get_mut(&id) {
                    job.status = status;
                    job.group.take();
                }
                status
            };
            manager.changed.notify_waiters();
            if manager.shutting_down.load(Ordering::Acquire) {
                return;
            }
            let (exit_code, exit_label) = match status {
                JobStatus::Finished(code) => (code, format!("exit {}", code.unwrap_or(-1))),
                JobStatus::Killed => (None, "killed".into()),
                JobStatus::TimedOut => (None, "timeout".into()),
                JobStatus::Running | JobStatus::Killing => unreachable!("任务已回收"),
            };
            let output = super::output::prepare(
                ToolOutput::File {
                    path: log_path.clone(),
                    suffix: String::new(),
                },
                &ctx.config.tools.output,
                &ctx.paths,
            )
            .await;
            let output = match output {
                Ok(output) => output,
                Err(error) => format!("[E_JOB_LOG] 无法读取日志：{error}"),
            };
            {
                let mut state = ctx.state.lock().await;
                state.notices.push(format!(
                    "后台任务 {id} 已结束（{exit_label}），输出预览：\n{output}\n完整输出：{}",
                    log_path.display()
                ));
            }
            let update = SessionUpdate::JobFinished {
                job_id: id.clone(),
                command,
                exit_code,
                log_path: log_path.to_string_lossy().into_owned(),
            };
            if let Err(error) = ctx.request.updates().emit(ctx.session_id, update).await {
                tracing::warn!(job_id = %id, %error, "发送后台任务完成通知失败");
            }
        });
        let mut workers = self.workers.lock();
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
        Ok(())
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
            .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("未知 job `{id}`")))?;
        let text = read_tail(&job.path, lines)
            .await
            .map_err(|error| ToolError::new("E_JOB_LOG", error.to_string()))?;
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
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("未知 job `{id}`")))?;
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
            let status = self
                .info(id)
                .await
                .expect("运行中的任务不会从表移除")
                .status;
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
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("未知 job `{id}`")))?
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

pub async fn copy_output<R: AsyncRead + Unpin>(
    mut reader: R,
    path: PathBuf,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::File::create(path).await?;
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).await?;
    }
    file.flush().await?;
    file.sync_all().await
}

async fn read_tail(path: &Path, max_lines: usize) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let length = file.metadata().await?.len();
    let start = length.saturating_sub(1024 * 1024);
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut bytes = Vec::with_capacity((length - start) as usize);
    file.read_to_end(&mut bytes).await?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    Ok(lines[lines.len().saturating_sub(max_lines)..].join("\n"))
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
        "管理后台 shell 作业：list、output、kill、wait。"
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
                    "timeout_secs":{"type":"integer","minimum":1,"maximum":600}
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
                                format!(
                                    "{}\t{:?}\t{}\t{}",
                                    job.id,
                                    job.status,
                                    job.command.lines().next().unwrap_or(""),
                                    job.path.display()
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
                    let status = ctx
                        .jobs
                        .wait(
                            &id,
                            Duration::from_secs(args.timeout_secs.unwrap_or(60).clamp(1, 600)),
                        )
                        .await?;
                    let mut output = ctx
                        .jobs
                        .output(&id, args.tail_lines.unwrap_or(200).clamp(1, 2000))
                        .await?;
                    let ToolOutput::Excerpt { text, .. } = &mut output else {
                        unreachable!("后台日志返回选定尾部片段");
                    };
                    text.insert_str(0, &format!("{status:?}\n"));
                    output
                }
                _ => {
                    return Err(ToolError::new(
                        "E_BAD_PAYLOAD",
                        "job op 必须是 list/output/kill/wait",
                    ));
                }
            };
            Ok(ToolOutcome::success(output))
        })
    }
}

fn require_id(id: Option<String>) -> Result<String, ToolError> {
    id.filter(|id| !id.is_empty())
        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "该 job 操作需要 job_id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_waits_for_job_and_reads_bounded_log_tail() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(JobManager::new());
        let path = dir.path().join("job.log");
        tokio::fs::write(
            &path,
            (0..2500)
                .map(|line| format!("line {line}\n"))
                .collect::<String>(),
        )
        .await
        .unwrap();
        let output = read_tail(&path, 2000).await.unwrap();
        assert!(output.starts_with("line 500"));
        assert!(output.ends_with("line 2499"));
        assert!(manager.list().await.is_empty());
    }
}
