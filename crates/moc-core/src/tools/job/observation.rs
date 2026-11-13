use super::super::ToolError;
use super::Job;
use super::JobManager;
use super::WaitOutcome;
use super::WaitReason;
use super::notice_prefix;
use super::truncate_to_lines;
use moc_protocol::SessionUpdate;
use moc_protocol::methods::JobInfo;
use moc_protocol::methods::JobOrigin;
use moc_protocol::methods::JobStatus;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

impl JobManager {
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

    pub(super) fn require(&self, id: &str) -> Result<JobInfo, ToolError> {
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
                        if next == job.read_cursor && matches!(reason, WaitReason::NewOutput) {
                            // 只收到 UTF-8 字符的前半段：继续等补齐或终态，不返回空增量。
                            Step::Pending(job.info())
                        } else {
                            job.read_cursor = next;
                            Step::Done(job.info(), text, skipped, reason)
                        }
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
    pub(super) async fn wait_terminal(
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
    pub(super) async fn detach(
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
    pub(super) fn abandon(&self, id: &str) {
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
