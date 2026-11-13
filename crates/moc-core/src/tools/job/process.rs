use super::super::ToolCtx;
use super::super::ToolError;
use super::super::bash::Process;
use super::super::job_log::JobLog;
use super::Announce;
use super::Foreground;
use super::ForegroundResult;
use super::Job;
use super::JobManager;
use super::JobSpec;
use super::LIVE_TAIL_LINE_CHARS;
use super::LIVE_TAIL_LINES;
use super::OBSERVE_INTERVAL;
use super::finished_notice;
use super::notice_prefix;
use futures_util::future::BoxFuture;
use moc_protocol::SessionUpdate;
use moc_protocol::methods::JobInfo;
use moc_protocol::methods::JobLifecycle;
use moc_protocol::methods::JobOrigin;
use moc_protocol::methods::JobStatus;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;

impl JobManager {
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

    pub(super) fn register(
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
    pub(super) async fn supervise(
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

    pub(super) async fn on_output(&self, ctx: &ToolCtx, id: &str, log: &JobLog) {
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

    pub(super) async fn on_stall(&self, ctx: &ToolCtx, id: &str, log: &JobLog, silent: Duration) {
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
    pub(super) async fn settle(&self, ctx: &ToolCtx, id: &str, waited: ForegroundResult) {
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
    pub(super) fn terminate(&self, id: &str) -> Result<Option<(JobInfo, Announce)>, ToolError> {
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
}
