//! 会话的 shell 任务：模型发起的每条 bash 都从 spawn 起登记，前台命令可转后台；
//! 后台任务结束后自动回报（主会话空闲时唤醒新一轮），无输出过久时提醒一次。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
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

use super::bash::{ProcessExit, ProcessGroup};
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
    /// 信号与资源限制诊断不属于 stdout；终态通知和日志读取统一附上。
    diagnostics: String,
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

mod observation;
mod process;

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
}

/// 同一任务的所有提醒共用的前缀；末尾空格避免 `job_1` 匹配到 `job_10`。
fn notice_prefix(id: &str) -> String {
    format!("Background job {id} ")
}

fn finished_notice(info: &JobInfo, log: &JobLog) -> String {
    let outcome = match info.status {
        JobStatus::Exited => info
            .exit_code
            .map_or_else(|| "no exit code".into(), |code| format!("exit {code}")),
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
        "Manage background shell jobs: list, output (recent tail), kill, or wait (returns early on finish or unseen output)."
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
                    "timeout_secs":{"type":"integer","minimum":1,"description":"wait only: max seconds to wait (default 60, capped by tools.job.max_wait_secs)"}
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
