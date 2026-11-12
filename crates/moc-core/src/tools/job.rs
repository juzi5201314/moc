//! 会话的 shell 任务：模型发起的每条 bash 都从 spawn 起登记，前台命令可转后台；
//! 后台任务结束后自动回报（主会话空闲时唤醒新一轮），无输出过久时提醒一次。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::methods::{JobInfo, JobLifecycle, JobOrigin, JobStatus};
use moc_protocol::{SessionUpdate, UpdateSink};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{Notify, oneshot};

use super::bash::{Process, ProcessExit, ProcessGroup};
use super::job_log::JobLog;
use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};

/// 保留的已结束任务数量；更早的任务被驱逐，释放日志句柄和环形缓冲。
const RETAINED_FINISHED_JOBS: usize = 16;
/// 监视器采样间隔：检测新输出、推送尾部、判断停滞。
const OBSERVE_INTERVAL: Duration = Duration::from_secs(1);
/// 实时尾部推给前端的行数与单行字符上限。
const LIVE_TAIL_LINES: usize = 5;
const LIVE_TAIL_LINE_CHARS: usize = 240;

/// 前台等待者收到的结果。
pub(crate) type ForegroundResult = Result<ProcessExit, ToolError>;

/// 任务结束时间与输出对外的发布目标；kill/background 这类不经工具调用的入口也要能推送。
#[derive(Clone)]
struct Announce {
    updates: UpdateSink,
    session_id: String,
}

impl Announce {
    async fn send(&self, update: SessionUpdate) {
        if let Err(error) = self.updates.emit(self.session_id.clone(), update).await {
            tracing::debug!(%error, "推送任务更新失败");
        }
    }
}

struct Job {
    id: String,
    command: String,
    description: Option<String>,
    tool_call_id: String,
    log_uri: String,
    status: JobStatus,
    exit_code: Option<i32>,
    lifecycle: JobLifecycle,
    origin: JobOrigin,
    stalled: bool,
    started_at: Instant,
    started_at_ms: u64,
    duration_ms: Option<u64>,
    pid: Option<u32>,
    /// 运行中持有；结束后释放。
    group: Option<ProcessGroup>,
    /// 后台任务占用的会话名额；结束时随任务释放。
    permit: Option<crate::concurrency::Permit>,
    /// 前台等待者；转入后台时取走（丢弃发送端即通知等待者返回）。
    attached: Option<oneshot::Sender<ForegroundResult>>,
    announce: Announce,
    log: JobLog,
    /// 模型已看到的逻辑输出位置（`JobLog` 的 `seen` 坐标）；`job output`/`job wait`/通知送达后推进。
    read_cursor: u64,
}

impl Job {
    fn info(&self) -> JobInfo {
        JobInfo {
            job_id: self.id.clone(),
            command: self.command.clone(),
            description: self.description.clone(),
            tool_call_id: self.tool_call_id.clone(),
            status: self.status,
            exit_code: self.exit_code,
            lifecycle: self.lifecycle,
            origin: self.origin,
            stalled: self.stalled,
            output_elided: self.log.output_elided(),
            started_at_ms: self.started_at_ms,
            duration_ms: self.duration_ms,
            log_uri: self.log_uri.clone(),
        }
    }
}

/// bash 交给任务表的一条新命令。
pub(crate) struct JobSpec {
    pub id: String,
    pub command: String,
    pub description: Option<String>,
    pub log: JobLog,
    pub lifecycle: JobLifecycle,
}

#[derive(Default)]
pub struct JobManager {
    jobs: Mutex<HashMap<String, Job>>,
    changed: Notify,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    shutting_down: AtomicBool,
    /// 本会话的后台名额；每个会话独立计数。
    slots: Arc<crate::concurrency::Gate>,
    /// 已结束、完成通知还没送到模型的后台任务。
    ///
    /// 不变量：只在持有会话状态锁（`SessionState`）时修改，与 `notices` 的增删同步，
    /// 这样「有未送达的完成通知」与「notices 里有这条通知」始终一致。
    unreported: Mutex<HashSet<String>>,
    /// 用户主动取消了上一轮：后台任务结束不再自动唤醒，等用户下一次输入。
    wake_suppressed: AtomicBool,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 驱逐超出保留上限的已结束任务，释放 JobLog 句柄和环形缓冲。
    ///
    /// 运行中和正在终止的任务不受影响；已结束任务按开始时间从旧到新淘汰。
    fn evict_finished(jobs: &mut HashMap<String, Job>) {
        let mut finished: Vec<(Instant, String)> = jobs
            .values()
            .filter(|job| job.status.is_terminal())
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

    /// 显式 `async=true` 预留名额：按 `overflow` 报错或等待。
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
                            "{} background jobs are already running in this session; wait for or kill one first",
                            config.max_running
                        ),
                    )
                })
            }
            crate::config::JobOverflow::Wait => self
                .slots
                .acquire(config.max_running, cancel)
                .await
                .ok_or_else(|| {
                    ToolError::new("E_CANCELLED", "waiting for a background-job slot was cancelled")
                }),
        }
    }

    /// 登记一个后台任务（显式 async）。
    pub(crate) fn start_background(
        self: &Arc<Self>,
        ctx: &ToolCtx,
        spec: JobSpec,
        process: Process,
        permit: crate::concurrency::Permit,
    ) -> Result<(), ToolError> {
        self.register(ctx, spec, process, JobOrigin::Explicit, Some(permit), None)
    }

    /// 登记一个前台任务；返回的句柄在结束或转入后台前一直挂着本次工具调用。
    pub(crate) fn start_foreground(
        self: &Arc<Self>,
        ctx: &ToolCtx,
        spec: JobSpec,
        process: Process,
    ) -> Result<Foreground, ToolError> {
        let (sender, receiver) = oneshot::channel();
        let id = spec.id.clone();
        self.register(
            ctx,
            spec,
            process,
            JobOrigin::Foreground,
            None,
            Some(sender),
        )?;
        Ok(Foreground {
            manager: Arc::clone(self),
            id,
            receiver,
            settled: false,
            slots_full: false,
        })
    }

    fn register(
        self: &Arc<Self>,
        ctx: &ToolCtx,
        spec: JobSpec,
        process: Process,
        origin: JobOrigin,
        permit: Option<crate::concurrency::Permit>,
        attached: Option<oneshot::Sender<ForegroundResult>>,
    ) -> Result<(), ToolError> {
        let Process {
            pid,
            group,
            started_at,
            completion,
        } = process;
        if self.shutting_down.load(Ordering::Acquire) {
            group.kill()?;
            return Err(ToolError::new(
                "E_CANCELLED",
                "the session is closing; cannot start a command",
            ));
        }
        let log_uri = ctx
            .artifact_store()
            .uri_for_path(spec.log.path())
            .unwrap_or_else(|error| format!("[{}] {}", error.code, error.message));
        let job = Job {
            id: spec.id.clone(),
            command: spec.command,
            description: spec.description,
            tool_call_id: ctx.tool_call_id.clone(),
            log_uri,
            status: JobStatus::Running,
            exit_code: None,
            lifecycle: spec.lifecycle,
            origin,
            stalled: false,
            started_at,
            started_at_ms: crate::session::store::now_ms(),
            duration_ms: None,
            pid,
            group: Some(group),
            permit,
            attached,
            announce: Announce {
                updates: ctx.request.updates(),
                session_id: ctx.session_id.clone(),
            },
            log: spec.log.clone(),
            read_cursor: 0,
        };
        let started = SessionUpdate::JobStarted { job: job.info() };
        let announce = job.announce.clone();
        self.jobs.lock().insert(spec.id.clone(), job);
        let manager = Arc::clone(self);
        let ctx = ctx.clone();
        let id = spec.id;
        let log = spec.log;
        let worker = tokio::spawn(async move {
            announce.send(started).await;
            let waited = manager.supervise(&ctx, &id, &log, completion).await;
            manager.settle(&ctx, &id, waited).await;
        });
        let mut workers = self.workers.lock();
        workers.retain(|worker| !worker.is_finished());
        workers.push(worker);
        Ok(())
    }

    /// 等进程结束；期间每秒采样一次输出，循环保护叫停时终止进程组。
    async fn supervise(
        &self,
        ctx: &ToolCtx,
        id: &str,
        log: &JobLog,
        mut completion: BoxFuture<'static, ForegroundResult>,
    ) -> ForegroundResult {
        let stall_after = Duration::from_secs(ctx.config.tools.bash.stall_after_secs);
        let mut ticker = tokio::time::interval(OBSERVE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seen = 0u64;
        let mut last_growth = Instant::now();
        loop {
            tokio::select! {
                result = &mut completion => return result,
                _ = ctx.guard.stopped.cancelled() => {
                    let killed = self.terminate(id);
                    let result = completion.await;
                    if let Err(error) = &killed {
                        tracing::warn!(job_id = %id, %error, "循环保护终止任务失败");
                    }
                    return result;
                }
                _ = ticker.tick() => {
                    let now = log.bytes_seen();
                    if now != seen {
                        seen = now;
                        last_growth = Instant::now();
                        self.on_output(ctx, id, log).await;
                    } else if last_growth.elapsed() >= stall_after {
                        self.on_stall(ctx, id, log, last_growth.elapsed()).await;
                    }
                }
            }
        }
    }

    async fn on_output(&self, ctx: &ToolCtx, id: &str, log: &JobLog) {
        let (output, resumed, announce) = {
            let mut jobs = self.jobs.lock();
            let Some(job) = jobs.get_mut(id) else { return };
            let resumed = std::mem::take(&mut job.stalled).then(|| job.info());
            let output = SessionUpdate::JobOutput {
                job_id: job.id.clone(),
                tool_call_id: job.tool_call_id.clone(),
                tail: log.live_tail(LIVE_TAIL_LINES, LIVE_TAIL_LINE_CHARS),
            };
            (output, resumed, job.announce.clone())
        };
        if let Some(job) = resumed {
            // 恢复输出后撤掉还没送达的停滞提醒：模型只该看到最新事实。
            let prefix = notice_prefix(id);
            ctx.state
                .lock()
                .await
                .notices
                .retain(|notice| !notice.starts_with(&prefix));
            announce.send(SessionUpdate::JobUpdated { job }).await;
        }
        announce.send(output).await;
    }

    async fn on_stall(&self, ctx: &ToolCtx, id: &str, log: &JobLog, silent: Duration) {
        let (job, notify_model, announce) = {
            let mut jobs = self.jobs.lock();
            let Some(job) = jobs.get_mut(id) else { return };
            if job.stalled || job.status != JobStatus::Running {
                return;
            }
            job.stalled = true;
            // persistent 服务安静是常态；前台命令的模型正等着结果，停滞只给界面看。
            let notify_model =
                job.lifecycle == JobLifecycle::Monitored && job.origin.is_background();
            (job.info(), notify_model, job.announce.clone())
        };
        if notify_model {
            let last = log.live_tail(1, LIVE_TAIL_LINE_CHARS);
            let last = if last.is_empty() {
                "(no output yet)".to_owned()
            } else {
                last
            };
            let notice = format!(
                "{}has produced no output for {}s (last line: {last}). It may be blocked on the network, a lock, or stdin input. Check `job output`, or kill it and rerun with a narrower or non-interactive command.",
                notice_prefix(id),
                silent.as_secs()
            );
            ctx.state.lock().await.notices.push(notice);
        }
        announce.send(SessionUpdate::JobUpdated { job }).await;
    }

    /// 进程结束：落定终态，回给前台等待者或作为后台完成通知送达模型。
    async fn settle(&self, ctx: &ToolCtx, id: &str, waited: ForegroundResult) {
        let (info, attached, announce, log) = {
            let mut jobs = self.jobs.lock();
            let job = jobs.get_mut(id).expect("worker 对应的任务一定已登记");
            let killed = job.status == JobStatus::Killing;
            job.status = match &waited {
                _ if killed => JobStatus::Killed,
                Ok(exit) if exit.timed_out => JobStatus::TimedOut,
                Ok(_) | Err(_) => JobStatus::Exited,
            };
            job.exit_code = match &waited {
                Ok(exit) if !killed && !exit.timed_out => exit.code,
                _ => None,
            };
            job.duration_ms =
                Some(u64::try_from(job.started_at.elapsed().as_millis()).unwrap_or(u64::MAX));
            job.group = None;
            job.permit = None;
            job.stalled = false;
            let attached = job.attached.take();
            let parts = (job.info(), attached, job.announce.clone(), job.log.clone());
            Self::evict_finished(&mut jobs);
            parts
        };
        self.changed.notify_waiters();
        if let Err(error) = &waited {
            tracing::warn!(job_id = %id, %error, "等待任务结束失败");
        }
        let closing = self.shutting_down.load(Ordering::Acquire);
        if let Some(attached) = attached {
            let waited = waited.map(|mut exit| {
                exit.killed = info.status == JobStatus::Killed;
                exit
            });
            // 等待者已被丢弃（提示被中止）时结果无人接收，属于正常收尾。
            let _ = attached.send(waited);
            if !closing {
                announce
                    .send(SessionUpdate::JobFinished { job: info })
                    .await;
            }
            return;
        }
        if closing {
            return;
        }
        let notice = finished_notice(&info, &log);
        {
            let mut state = ctx.state.lock().await;
            let prefix = notice_prefix(id);
            state.notices.retain(|notice| !notice.starts_with(&prefix));
            state.notices.push(notice);
            self.unreported.lock().insert(id.to_owned());
        }
        crate::service::plugin_events::job_finished(
            &ctx.plugins,
            &ctx.plugin_call_context(),
            id,
            &info.command,
            info.exit_code,
            &info.log_uri,
        )
        .await;
        announce
            .send(SessionUpdate::JobFinished { job: info })
            .await;
        crate::service::prompt::wake_for_jobs(
            Arc::clone(&ctx.core),
            ctx.request.clone(),
            ctx.session_id.clone(),
        )
        .await;
    }

    /// 给运行中的任务发终止信号；返回状态变化后的快照。已结束的任务原样返回。
    fn terminate(&self, id: &str) -> Result<Option<(JobInfo, Announce)>, ToolError> {
        let mut jobs = self.jobs.lock();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?;
        if job.status != JobStatus::Running {
            return Ok(None);
        }
        job.group
            .as_ref()
            .expect("运行中的任务一定持有进程组")
            .kill()?;
        job.status = JobStatus::Killing;
        Ok(Some((job.info(), job.announce.clone())))
    }

    pub async fn kill_all(&self) {
        self.shutting_down.store(true, Ordering::Release);
        for job in self.jobs.lock().values_mut() {
            if job.status == JobStatus::Running {
                job.status = JobStatus::Killing;
                if let Err(error) = job
                    .group
                    .as_ref()
                    .expect("运行中的任务一定持有进程组")
                    .kill()
                {
                    tracing::warn!(job_id = %job.id, pid = ?job.pid, %error, "终止任务失败");
                }
            }
        }
        let workers = std::mem::take(&mut *self.workers.lock());
        for worker in workers {
            if let Err(error) = worker.await {
                tracing::warn!(%error, "回收任务失败");
            }
        }
    }

    /// 全部任务（含前台），按 ID 升序。
    pub fn list(&self) -> Vec<JobInfo> {
        let mut jobs: Vec<_> = self.jobs.lock().values().map(Job::info).collect();
        jobs.sort_by(|left, right| left.job_id.cmp(&right.job_id));
        jobs
    }

    /// 模型可见的任务：前台命令还挂在它自己的工具调用上，不列出。
    pub fn background(&self) -> Vec<JobInfo> {
        let mut jobs = self.list();
        jobs.retain(|job| job.origin.is_background());
        jobs
    }

    pub fn info(&self, id: &str) -> Option<JobInfo> {
        self.jobs.lock().get(id).map(Job::info)
    }

    fn require(&self, id: &str) -> Result<JobInfo, ToolError> {
        self.info(id)
            .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))
    }

    /// 最后 `lines` 行输出与日志路径。
    pub(crate) fn output(
        &self,
        id: &str,
        lines: usize,
    ) -> Result<(JobInfo, String, PathBuf), ToolError> {
        let (info, log) = {
            let jobs = self.jobs.lock();
            let job = jobs
                .get(id)
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?;
            (job.info(), job.log.clone())
        };
        let (mut text, elided) = log
            .read_tail(lines)
            .map_err(|error| ToolError::new("E_JOB_LOG", error.to_string()))?;
        if elided {
            text.insert_str(
                0,
                "[output_elided=true; the middle of the output was dropped]\n",
            );
        }
        Ok((info, text, log.path().to_owned()))
    }

    /// 模型侧的 `job output`：与 `output` 相同，但把游标推进到展示文本的末尾，
    /// 之后的 `job wait` 只返回真正的新输出。前端 RPC 走 `output`，不动游标。
    pub(crate) fn output_for_model(
        &self,
        id: &str,
        lines: usize,
    ) -> Result<(JobInfo, String, PathBuf), ToolError> {
        let mut jobs = self.jobs.lock();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?;
        let (mut text, elided, seen) = job
            .log
            .read_tail_and_seen(lines)
            .map_err(|error| ToolError::new("E_JOB_LOG", error.to_string()))?;
        job.read_cursor = seen;
        if elided {
            text.insert_str(
                0,
                "[output_elided=true; the middle of the output was dropped]\n",
            );
        }
        Ok((job.info(), text, job.log.path().to_owned()))
    }

    /// 「展示即已读」：把游标推进到 `seen`（展示文本的末尾）；只前进不回退。
    pub(crate) fn mark_output_read(&self, id: &str, seen: u64) {
        if let Some(job) = self.jobs.lock().get_mut(id) {
            job.read_cursor = job.read_cursor.max(seen);
        }
    }

    /// 终止并等进程组退出。
    pub async fn kill(&self, id: &str) -> Result<JobInfo, ToolError> {
        let before = self.require(id)?;
        if let Some((info, announce)) = self.terminate(id)? {
            announce.send(SessionUpdate::JobUpdated { job: info }).await;
        }
        Ok(self.wait_terminal(id, None).await.unwrap_or(JobInfo {
            status: JobStatus::Killed,
            ..before
        }))
    }

    /// 等任务结束、等到新输出，或超时；返回快照与自上次展示以来的增量文本。
    pub(crate) async fn wait(
        &self,
        id: &str,
        timeout: Duration,
        tail_lines: usize,
    ) -> Result<WaitOutcome, ToolError> {
        enum Step {
            Done(JobInfo, String, u64, WaitReason),
            Pending(JobInfo),
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let log = {
            let jobs = self.jobs.lock();
            jobs.get(id)
                .map(|job| job.log.clone())
                .ok_or_else(|| ToolError::new("E_JOB_NOT_FOUND", format!("unknown job `{id}`")))?
        };
        loop {
            // 先挂号再检查，免得把「检查后、入睡前」到达的输出或终态睡过去。
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let output = log.output_notified();
            tokio::pin!(output);
            output.as_mut().enable();
            let step = {
                let mut jobs = self.jobs.lock();
                let Some(job) = jobs.get_mut(id) else {
                    return Err(ToolError::new(
                        "E_JOB_NOT_FOUND",
                        format!("unknown job `{id}`"),
                    ));
                };
                let reason = if job.status.is_terminal() {
                    Some(WaitReason::Finished)
                } else if job.log.bytes_seen() > job.read_cursor {
                    Some(WaitReason::NewOutput)
                } else {
                    None
                };
                match reason {
                    Some(reason) => {
                        let (text, next, skipped) = job
                            .log
                            .read_since(job.read_cursor)
                            .map_err(|error| ToolError::new("E_JOB_LOG", error.to_string()))?;
                        job.read_cursor = next;
                        Step::Done(job.info(), text, skipped, reason)
                    }
                    None => Step::Pending(job.info()),
                }
            };
            let (info, text, skipped, reason) = match step {
                Step::Done(info, text, skipped, reason) => (info, text, skipped, reason),
                Step::Pending(info) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if !remaining.is_zero() {
                        tokio::select! {
                            _ = changed => continue,
                            _ = output => continue,
                            _ = tokio::time::sleep(remaining) => {}
                        }
                    }
                    return Ok(WaitOutcome {
                        info,
                        reason: WaitReason::TimedOut,
                        text: String::new(),
                        log_path: log.path().to_owned(),
                        skipped_bytes: 0,
                    });
                }
            };
            let (text, omitted) = truncate_to_lines(text, tail_lines);
            return Ok(WaitOutcome {
                info,
                reason,
                text,
                log_path: log.path().to_owned(),
                skipped_bytes: skipped + omitted,
            });
        }
    }

    /// 任务已被驱逐时返回 `None`（说明早已结束）。
    async fn wait_terminal(
        &self,
        id: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> Option<JobInfo> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let info = self.info(id)?;
            if info.status.is_terminal() {
                return Some(info);
            }
            match deadline {
                Some(deadline) => {
                    if tokio::time::timeout_at(deadline, notified).await.is_err() {
                        return self.info(id);
                    }
                }
                None => notified.await,
            }
        }
    }

    /// 用户把前台任务转入后台；名额满也放行——进程已经在跑，转后台只是不再阻塞模型。
    pub(crate) async fn background_by_user(&self, id: &str) -> Result<JobInfo, ToolError> {
        let permit = self.slots.try_acquire(0).expect("无上限时总能取得名额");
        match self.detach(id, JobOrigin::UserBackground, permit).await {
            Some(info) => Ok(info),
            None => self.require(id),
        }
    }

    /// 超过前台阈值时尝试转入后台；只有会话名额已满才返回 `false`，命令留在前台。
    pub(crate) async fn background_auto(&self, id: &str, max_running: usize) -> bool {
        let Some(permit) = self.slots.try_acquire(max_running) else {
            return false;
        };
        // 任务恰好已结束时 detach 什么也不做，结果照常经前台返回。
        self.detach(id, JobOrigin::AutoBackground, permit).await;
        true
    }

    /// 摘下前台等待者并改记为后台任务；任务已结束或已在后台时返回 `None`。
    async fn detach(
        &self,
        id: &str,
        origin: JobOrigin,
        permit: crate::concurrency::Permit,
    ) -> Option<JobInfo> {
        let (info, announce) = {
            let mut jobs = self.jobs.lock();
            let job = jobs.get_mut(id)?;
            if job.status.is_terminal() || job.attached.is_none() {
                return None;
            }
            // 丢弃发送端：前台等待者的接收端随即返回，工具调用以「已转后台」结束。
            job.attached = None;
            job.origin = origin;
            job.permit = Some(permit);
            (job.info(), job.announce.clone())
        };
        announce
            .send(SessionUpdate::JobUpdated { job: info.clone() })
            .await;
        Some(info)
    }

    /// 前台句柄被丢弃而任务仍挂着它：调用方已不在，终止进程，避免留下无人认领的命令。
    fn abandon(&self, id: &str) {
        let mut jobs = self.jobs.lock();
        if let Some(job) = jobs.get_mut(id)
            && job.status == JobStatus::Running
            && job.attached.is_some()
        {
            if let Some(group) = &job.group
                && let Err(error) = group.kill()
            {
                tracing::warn!(job_id = %id, %error, "终止无人认领的前台命令失败");
            }
            job.status = JobStatus::Killing;
        }
    }

    /// `job wait` 已经把结束结果交给模型：撤掉还没送达的同一任务通知，不再重复注入。
    pub(crate) async fn mark_reported(
        &self,
        state: &tokio::sync::Mutex<crate::session::state::SessionState>,
        id: &str,
    ) {
        let prefix = notice_prefix(id);
        let mut state = state.lock().await;
        state.notices.retain(|notice| !notice.starts_with(&prefix));
        self.unreported.lock().remove(id);
    }

    /// 通知已整体送入模型上下文；调用方必须持有会话状态锁。
    pub(crate) fn mark_all_reported(&self) {
        let unreported = std::mem::take(&mut *self.unreported.lock());
        if unreported.is_empty() {
            return;
        }
        let mut jobs = self.jobs.lock();
        for id in unreported {
            // 完成通知的 tail 已随 notices 进入上下文；这些任务均已结束，seen 不再增长。
            if let Some(job) = jobs.get_mut(&id) {
                job.read_cursor = job.log.bytes_seen();
            }
        }
    }

    /// 是否有后台任务结束、通知还没送达，且唤醒没有被用户取消抑制。
    pub(crate) fn wants_wake(&self) -> bool {
        !self.wake_suppressed.load(Ordering::Acquire) && !self.unreported.lock().is_empty()
    }

    pub(crate) fn suppress_wake(&self, suppressed: bool) {
        self.wake_suppressed.store(suppressed, Ordering::Release);
    }
}

/// 同一任务的所有提醒共用的前缀；末尾空格避免 `job_1` 匹配到 `job_10`。
fn notice_prefix(id: &str) -> String {
    format!("Background job {id} ")
}

fn finished_notice(info: &JobInfo, log: &JobLog) -> String {
    let outcome = match info.status {
        JobStatus::Exited => format!("exit {}", info.exit_code.unwrap_or(-1)),
        JobStatus::Killed => "killed".into(),
        JobStatus::TimedOut => "hard timeout".into(),
        JobStatus::Running | JobStatus::Killing => unreachable!("任务已经结束"),
    };
    let seconds = info.duration_ms.unwrap_or(0) / 1000;
    let (tail, elided) = log
        .read_tail(200)
        .unwrap_or_else(|error| (format!("[E_JOB_LOG] failed to read log: {error}"), false));
    let elided = if elided {
        "[the middle of the output was dropped]\n"
    } else {
        ""
    };
    format!(
        "{}finished ({outcome}) after {seconds}s: `{}`\nOutput tail:\n{elided}{tail}\nFull output: {}",
        notice_prefix(&info.job_id),
        info.command,
        info.log_uri
    )
}

/// 前台命令在本次工具调用上的句柄。
pub(crate) struct Foreground {
    manager: Arc<JobManager>,
    pub id: String,
    receiver: oneshot::Receiver<ForegroundResult>,
    settled: bool,
    /// 到了转后台阈值但会话名额已满，命令只能留在前台。
    pub slots_full: bool,
}

/// 前台等待的结局。
pub(crate) enum ForegroundEnd {
    Finished(ForegroundResult),
    /// 已转入后台（自动或用户），结果之后作为通知送达。
    Backgrounded,
}

impl Foreground {
    /// 等结果或转后台。`cancel` 触发时终止进程组并等它退出。
    pub(crate) async fn wait(
        &mut self,
        cancel: &tokio_util::sync::CancellationToken,
        background_after: Option<(Duration, usize)>,
    ) -> ForegroundEnd {
        let mut threshold =
            background_after.map(|(after, slots)| (Box::pin(tokio::time::sleep(after)), slots));
        let mut cancelled = false;
        let end = loop {
            tokio::select! {
                biased;
                result = &mut self.receiver => break match result {
                    Ok(result) => ForegroundEnd::Finished(result),
                    Err(_) => ForegroundEnd::Backgrounded,
                },
                _ = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    if let Err(error) = self.manager.terminate(&self.id) {
                        tracing::warn!(job_id = %self.id, %error, "取消时终止命令失败");
                    }
                }
                _ = async {
                    match &mut threshold {
                        Some((sleep, _)) => sleep.as_mut().await,
                        None => std::future::pending().await,
                    }
                }, if !cancelled => {
                    let slots = threshold.take().map_or(0, |(_, slots)| slots);
                    // 名额满时留在前台；调用方据此在结果里说明原因。
                    self.slots_full = !self.manager.background_auto(&self.id, slots).await;
                }
            }
        };
        self.settled = true;
        if cancelled && matches!(end, ForegroundEnd::Finished(_)) {
            return ForegroundEnd::Finished(Err(ToolError::new(
                "E_CANCELLED",
                "command cancelled",
            )));
        }
        end
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        if !self.settled {
            self.manager.abandon(&self.id);
        }
    }
}

/// `job wait` 返回的原因。
pub(crate) enum WaitReason {
    /// 任务进入终态。
    Finished,
    /// 等到了自上次展示以来的新输出。
    NewOutput,
    /// 超时；期间没有新输出、任务仍在运行。
    TimedOut,
}

/// `job wait` 的结果：状态快照、返回原因、增量文本与被折叠的字节数。
pub(crate) struct WaitOutcome {
    pub info: JobInfo,
    pub reason: WaitReason,
    /// 自游标以来的增量文本（按 `tail_lines` 截断）；超时为空。
    pub text: String,
    pub log_path: PathBuf,
    /// 增量范围里没有展示的字节数：日志容量丢弃 + 行数截断省略。
    pub skipped_bytes: u64,
}

/// 增量文本按行截断：保留末尾 `max_lines` 行；返回展示文本与被省略的前缀字节数。
fn truncate_to_lines(text: String, max_lines: usize) -> (String, u64) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return (text, 0);
    }
    let tail = lines[lines.len() - max_lines..].join("\n");
    let omitted = u64::try_from(text.len() - tail.len()).unwrap_or(u64::MAX);
    (tail, omitted)
}

pub(crate) async fn copy_output<R: AsyncRead + Unpin>(
    mut reader: R,
    log: JobLog,
) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        log.append(&buffer[..read])?;
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

/// 协议枚举的 snake_case 名，供模型可见文本使用。
pub(crate) fn wire<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn elapsed_secs(info: &JobInfo) -> u64 {
    info.duration_ms
        .unwrap_or_else(|| crate::session::store::now_ms().saturating_sub(info.started_at_ms))
        / 1000
}

impl Tool for JobTool {
    fn name(&self) -> &'static str {
        "job"
    }
    fn description(&self) -> &'static str {
        "Manage background shell jobs: list, output (recent tail), kill, or wait (returns early when the job finishes or produces output you have not seen, up to timeout_secs). Completion of a background job is reported to you automatically."
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
                    "job_id":{"type":"string","description":"Job id from list or a notice"},
                    "tail_lines":{"type":"integer","minimum":1,"maximum":2000,"description":"Log lines to return; default 200"},
                    "timeout_secs":{"type":"integer","minimum":1,"description":"wait only: max seconds to block before returning; returns earlier on finish or unseen output; default 60, capped by tools.job.max_wait_secs"}
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
            let tail_lines = args.tail_lines.unwrap_or(200).clamp(1, 2000);
            let output: ToolOutput = match args.op.as_str() {
                "list" => {
                    let jobs = ctx.jobs.background();
                    if jobs.is_empty() {
                        "No background jobs.".into()
                    } else {
                        jobs.iter()
                            .map(|job| {
                                format!(
                                    "{}\tlifecycle={}\torigin={}\tstatus={}{}\telapsed_secs={}\tstalled={}\toutput_elided={}\t{}",
                                    job.job_id,
                                    wire(job.lifecycle),
                                    wire(job.origin),
                                    wire(job.status),
                                    job.exit_code.map_or_else(String::new, |code| format!("({code})")),
                                    elapsed_secs(job),
                                    job.stalled,
                                    job.output_elided,
                                    job.log_uri
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into()
                    }
                }
                "output" => {
                    let id = require_id(args.job_id)?;
                    let (_, text, path) = ctx.jobs.output_for_model(&id, tail_lines)?;
                    ToolOutput::Excerpt { text, path }
                }
                "kill" => {
                    let id = require_id(args.job_id)?;
                    let info = ctx.jobs.kill(&id).await?;
                    ctx.jobs.mark_reported(&ctx.state, &id).await;
                    format!("{:?}", info.status).into()
                }
                "wait" => {
                    let id = require_id(args.job_id)?;
                    let timeout = args
                        .timeout_secs
                        .unwrap_or(60)
                        .clamp(1, ctx.config.tools.job.max_wait_secs);
                    let waited = ctx
                        .jobs
                        .wait(&id, Duration::from_secs(timeout), tail_lines)
                        .await?;
                    let reason = match waited.reason {
                        WaitReason::Finished => "finished",
                        WaitReason::NewOutput => "new_output",
                        WaitReason::TimedOut => "timeout",
                    };
                    let message = match waited.reason {
                        WaitReason::Finished => {
                            ctx.jobs.mark_reported(&ctx.state, &id).await;
                            "job finished".to_owned()
                        }
                        WaitReason::NewOutput => {
                            "still running; it produced new output since you last saw it (below); wait again to keep watching".to_owned()
                        }
                        WaitReason::TimedOut => {
                            if waited.info.lifecycle == JobLifecycle::Persistent {
                                "persistent job will not auto-complete; use output or kill"
                                    .to_owned()
                            } else {
                                "monitored job is still running and produced no new output; its completion will be reported automatically, so continue with other work instead of waiting again".to_owned()
                            }
                        }
                    };
                    let structured = serde_json::json!({
                        "status": format!("{:?}", waited.info.status),
                        "reason": reason,
                        "exit_code": waited.info.exit_code,
                        "lifecycle": wire(waited.info.lifecycle),
                        "origin": wire(waited.info.origin),
                        "elapsed_secs": elapsed_secs(&waited.info),
                        "stalled": waited.info.stalled,
                        "output_elided": waited.info.output_elided,
                        "message": message,
                    });
                    let mut header = format!("{:?}\n{message}\n", waited.info.status);
                    if waited.skipped_bytes > 0 {
                        header.push_str(&format!(
                            "[... {} earlier bytes of new output are not shown; the log keeps full output ...]\n",
                            waited.skipped_bytes
                        ));
                    }
                    let text = format!("{header}{}", waited.text);
                    let mut outcome = ToolOutcome::success(ToolOutput::Excerpt {
                        text,
                        path: waited.log_path,
                    });
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
