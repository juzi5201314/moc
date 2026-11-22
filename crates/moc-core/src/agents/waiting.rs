use super::AgentRegistry;
use super::WAIT_DEFAULT_WINDOW_SECS;
use super::WAIT_PROGRESS_INTERVAL;
use super::WaitProgress;
use super::WaitRow;
use super::WaitSnapshot;
use super::invalid;
use super::live;
use super::roster::peers;
use super::terminal;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::store::now_ms;
use moc_protocol::methods::AgentStatus;
use moc_protocol::methods::AgentWaitEntry;
use moc_protocol::methods::AgentWaitResult;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionUpdate;
use moc_protocol::methods::ToolProgressRow;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::futures::OwnedNotified;
use tokio_util::sync::CancellationToken;

/// 在注册表锁内建立等待者并读取快照。
///
/// 完成路径先写终态、再 `notify_waiters`；这两步如果和这里的读取交错，等待者会错过
/// 唤醒并一直挂起（`notify_waiters` 不保存许可）。所以锁内先建等待者再读状态，无论怎么
/// 交错，结果都会是「立即返回终态」或「等待者已就绪」。
pub(super) fn wait_snapshot(registry: &AgentRegistry, targets: &[SessionInfo]) -> WaitSnapshot {
    let mut rows = Vec::with_capacity(targets.len());
    let mut watched = Vec::with_capacity(targets.len());
    let mut settled = false;
    let now = now_ms();
    for session in targets {
        let Some(runtime) = registry.items.get(&session.id) else {
            // 注册表里没有条目：进程内没有在跑，按 `Idle` 回报。
            settled = true;
            rows.push(WaitRow {
                entry: AgentWaitEntry {
                    id: session.id.clone(),
                    title: session.title.clone(),
                    status: AgentStatus::Idle,
                    message: None,
                    finish_reason: None,
                    structured_result: None,
                },
                elapsed_ms: 0,
                activity: None,
            });
            continue;
        };
        let info = &runtime.info;
        if terminal(info.status) {
            settled = true;
        } else {
            watched.push(Box::pin(Arc::clone(&runtime.notify).notified_owned()));
        }
        rows.push(WaitRow {
            entry: AgentWaitEntry {
                id: info.id.clone(),
                title: info.title.clone(),
                status: info.status,
                message: runtime.message.clone(),
                finish_reason: runtime.finish_reason.clone(),
                structured_result: runtime.structured.clone(),
            },
            elapsed_ms: if live(info.status) {
                now.saturating_sub(info.updated_at_ms)
            } else {
                0
            },
            activity: live(info.status)
                .then(|| runtime.activity.clone())
                .flatten(),
        });
    }
    WaitSnapshot {
        rows,
        watched,
        settled,
    }
}

/// 逐个轮询一组等待者，任一就绪即返回。
///
/// 等价于「select 全部 `Notified`」，避免为每个目标各起一个任务。
pub(super) async fn await_any(watchers: &mut [std::pin::Pin<Box<OwnedNotified>>]) {
    std::future::poll_fn(|context| {
        for watcher in watchers.iter_mut() {
            if watcher.as_mut().poll(context).is_ready() {
                return std::task::Poll::Ready(());
            }
        }
        std::task::Poll::Pending
    })
    .await
}

pub(super) async fn push_progress(progress: Option<&WaitProgress>, rows: &[WaitRow]) {
    let Some(progress) = progress else {
        return;
    };
    let rows = rows
        .iter()
        .map(|row| ToolProgressRow {
            agent_id: row.entry.id.clone(),
            title: row.entry.title.clone(),
            status: row.entry.status,
            elapsed_ms: row.elapsed_ms,
            activity: row.activity.clone(),
        })
        .collect();
    if let Err(error) = progress
        .updates
        .emit(
            progress.session_id.clone(),
            SessionUpdate::ToolProgress {
                prompt_id: progress.prompt_id.clone(),
                tool_call_id: progress.tool_call_id.clone(),
                rows,
            },
        )
        .await
    {
        tracing::warn!(%error, "子代理等待进度推送失败");
    }
}

pub(super) fn wait_result(
    rows: Vec<WaitRow>,
    settled: bool,
    messages_pending: bool,
    timed_out: bool,
) -> AgentWaitResult {
    AgentWaitResult {
        agents: rows.into_iter().map(|row| row.entry).collect(),
        settled,
        messages_pending,
        timed_out,
    }
}

/// 等待目标集：显式 id 校验为直接子代理，省略则取全部运行中的直接子代理。
pub(super) async fn resolve_wait_targets(
    inner: &CoreInner,
    session_id: &str,
    agent_ids: Option<Vec<String>>,
) -> Result<Vec<SessionInfo>, CoreError> {
    let peers = peers(inner, session_id).await?;
    match agent_ids.filter(|ids| !ids.is_empty()) {
        Some(ids) => ids
            .iter()
            .map(|id| {
                peers
                    .children
                    .iter()
                    .find(|entry| entry.session.id == *id)
                    .map(|entry| entry.session.clone())
                    .ok_or_else(|| invalid(format!("target `{id}` is not a direct child")))
            })
            .collect(),
        None => {
            let live_targets: Vec<SessionInfo> = peers
                .children
                .iter()
                .filter(|entry| live(entry.status))
                .map(|entry| entry.session.clone())
                .collect();
            if !live_targets.is_empty() {
                return Ok(live_targets);
            }
            // 子代理全都结束了：直接回报它们的终态。否则「刚 spawn 就 wait」这种很快
            // 结束的子代理会撞上空目标错误，而这恰恰是最常见的收集结果时机。
            if peers.children.is_empty() {
                return Err(invalid("no child agents to wait for"));
            }
            Ok(peers
                .children
                .iter()
                .map(|entry| entry.session.clone())
                .collect())
        }
    }
}

/// `agent/wait`：等待任一子代理进入终态，或被消息唤醒，或窗口到期。
pub(crate) async fn wait(
    inner: &CoreInner,
    session_id: &str,
    agent_ids: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    progress: Option<&WaitProgress>,
    cancel: &CancellationToken,
) -> Result<AgentWaitResult, CoreError> {
    let targets = resolve_wait_targets(inner, session_id, agent_ids).await?;
    let session_runtime = inner.runtime(session_id).await;
    let window = Duration::from_secs(timeout_secs.unwrap_or(WAIT_DEFAULT_WINDOW_SECS));
    let deadline = tokio::time::Instant::now() + window;
    // 首个 tick 立即就绪：先消费掉，进度节奏由下面的循环控制。
    let mut ticker = tokio::time::interval(WAIT_PROGRESS_INTERVAL);
    ticker.tick().await;
    loop {
        let WaitSnapshot {
            rows,
            mut watched,
            settled,
        } = {
            let registry = inner.agents.lock().await;
            wait_snapshot(&registry, &targets)
        };
        let (inbox_notified, inbox_pending) = {
            let inbox = session_runtime.agent_inbox.lock().await;
            (
                Arc::clone(&inbox.notify).notified_owned(),
                !inbox.messages.is_empty(),
            )
        };
        let (steering_notified, steering_pending) = {
            let steering = session_runtime.steering.lock().await;
            (
                Arc::clone(&steering.notify).notified_owned(),
                !steering.messages.is_empty(),
            )
        };
        let mut inbox_notified = std::pin::pin!(inbox_notified);
        let mut steering_notified = std::pin::pin!(steering_notified);
        let messages_pending = inbox_pending || steering_pending;
        // 每轮都推一次：既是最新进度行，也是返回前的最终快照。
        push_progress(progress, &rows).await;
        if settled || messages_pending || cancel.is_cancelled() {
            return Ok(wait_result(rows, settled, messages_pending, false));
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(wait_result(rows, false, false, false));
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(wait_result(rows, false, false, true));
            }
            _ = ticker.tick() => {}
            _ = await_any(&mut watched) => {}
            _ = inbox_notified.as_mut() => {}
            _ = steering_notified.as_mut() => {}
        }
    }
}
