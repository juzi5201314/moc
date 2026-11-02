//! `sidekick` 工具的 handoff 逻辑：给 Fusion lead 的持久执行侧投递工作。
//!
//! 与普通 spawn 的差异：同一会话跨 handoff 复用（上下文延续），首次创建走
//! `sidekick` preset 的内部 spawn 路径，后续 handoff 就是一次代理消息投递——
//! 运行中注入、空闲时唤醒。`mode` 改写这条路径：keep 沿用、compact 先压缩、
//! fresh 换新会话、retry 无消息重放。

use std::sync::Arc;

use moc_protocol::RequestContext;
use moc_protocol::methods::{
    AgentMessageMeta, AgentMessageOutcome, AgentSpawnResult, AgentStatus, SessionInfo,
    SessionModelOverflowAction,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{SpawnModel, SpawnSpec, WaitProgress, live, peers, preset, status_word, terminal};
use crate::error::CoreError;
use crate::service::CoreInner;

/// handoff 的两种收口。
pub(crate) enum Handoff {
    /// 正常产出：JSON 作为工具结果正文。
    ///
    /// `agent_id` 必须存在：`referenced_agent_ids` 依赖它做回退/孤儿跟踪。
    Json(Value),
    /// lead 的取消令牌触发：sidekick 已停，工具层回统一的取消结果。
    Cancelled,
}

/// `sidekick` 工具的 mode：keep（默认）沿用手头会话；compact/fresh/retry
/// 都要求 sidekick 空闲。
#[derive(Debug, Clone, Copy)]
pub(crate) enum HandoffMode {
    /// 现有行为：spawn / 空闲则切模型后唤醒 / 运行中注入。
    Keep,
    /// 压缩历史后投递本条 handoff。
    Compact,
    /// 全新 sidekick 会话（旧会话保留原样）。
    Fresh,
    /// 失败/停止态不带新消息重放上一轮。
    Retry,
}

/// `deliver_handoff` 的投递结果。
enum Delivery {
    /// 投递方式对应的 delivery 词。
    Word(&'static str),
    /// 收尾等待期间 lead 被取消：不再投递。
    Cancelled,
}

fn outcome_json(
    agent_id: &str,
    delivery: &str,
    status: AgentStatus,
    model: &str,
    result: Option<Value>,
    report: Option<String>,
    note: Option<String>,
) -> Value {
    let mut body = json!({
        "agent_id": agent_id,
        "delivery": delivery,
        "status": status_word(status),
        "model": model,
    });
    if let Some(result) = result {
        body["result"] = result;
    }
    if let Some(report) = report {
        body["report"] = Value::String(report);
    }
    if let Some(note) = note {
        body["note"] = Value::String(note);
    }
    body
}

/// 注册表里目标代理的当前状态；没有条目按 `Idle`。
async fn status_of(inner: &CoreInner, agent_id: &str) -> AgentStatus {
    inner
        .agents
        .lock()
        .await
        .items
        .get(agent_id)
        .map_or(AgentStatus::Idle, |runtime| runtime.info.status)
}

/// sidekick 会话头里的当前模型（结果 JSON 的 `model` 字段）。
async fn model_of(inner: &CoreInner, agent_id: &str) -> String {
    let Ok(record) = inner.sessions.get(agent_id).await else {
        return String::new();
    };
    record.state.lock().await.session.model.clone()
}

/// compact/fresh/retry 的忙碌错误：sidekick 还在跑时只有 keep 能进。
fn busy_error() -> CoreError {
    CoreError::Session(
        "The sidekick is running; wait for it with wait_agent or interrupt it with mode=\"keep\"."
            .into(),
    )
}

/// 提示锁抢不到（正在收尾）时的错误。
fn finishing_error() -> CoreError {
    CoreError::Session("The sidekick is still finishing its last run; try again shortly.".into())
}

/// 创建 sidekick 会话：sidekick preset + 内置报告 schema + 指定的执行侧模型。
async fn spawn_sidekick(
    inner: &Arc<CoreInner>,
    request: &RequestContext,
    session_id: &str,
    task: &str,
    model: &str,
) -> Result<AgentSpawnResult, CoreError> {
    super::spawn(
        Arc::clone(inner),
        request.clone(),
        session_id.to_owned(),
        SpawnSpec {
            task: task.to_owned(),
            title: Some("Sidekick".into()),
            model: SpawnModel::Exact(model.to_owned()),
            preset: Some(preset::SIDEKICK_PRESET.to_owned()),
            context: None,
            output_schema: Some(preset::sidekick_report_schema()),
            seed_entries: Vec::new(),
        },
    )
    .await
}

/// 提示锁已在手时把 sidekick 会话模型同步到配置 selector。
///
/// 切换本身的错误直接上抛；切换过程里的压缩失败不挡住 handoff，折成
/// note 交给结果 JSON（切换已生效，问题在压缩）。
async fn switch_to_desired(
    inner: &CoreInner,
    request: &RequestContext,
    record: &Arc<crate::session::SessionRecord>,
    child: &SessionInfo,
    sidekick: &crate::ResolvedModel,
) -> Result<(SessionInfo, Option<String>), CoreError> {
    if child.model == sidekick.selector {
        return Ok((child.clone(), None));
    }
    inner
        .sessions
        .ensure_write_lease(&child.id, record, &request.client().frontend)
        .await?;
    let outcome = crate::service::model::switch(
        inner,
        request,
        &child.id,
        sidekick,
        SessionModelOverflowAction::Compact,
    )
    .await?;
    let note = outcome.compaction_error.map(|error| {
        format!(
            "The sidekick switched to {} but compaction failed: {error}",
            sidekick.selector
        )
    });
    Ok((outcome.session, note))
}

/// keep/compact 共用的投递：运行中注入、空闲唤醒；「正在收尾」窗口等落定后
/// 重投一次，等待期间 lead 取消则不再投递。
async fn deliver_handoff(
    inner: &Arc<CoreInner>,
    request: &RequestContext,
    lead: &SessionInfo,
    target: &SessionInfo,
    message: &str,
    progress: &WaitProgress,
    cancel: &CancellationToken,
) -> Result<Delivery, CoreError> {
    let from = AgentMessageMeta {
        from_id: lead.id.clone(),
        from_title: lead.title.clone(),
        delivery: AgentMessageOutcome::Injected,
    };
    let target = super::MessageTarget {
        session: target.clone(),
    };
    let mut receipt = super::deliver(inner, request, &from, &target, message).await;
    if receipt.outcome == AgentMessageOutcome::Failed {
        // 「正在收尾」窗口：注册表里还是运行态。等它落定后再投一次；
        // 等待期间 lead 取消就不再投递。
        if inner
            .agents
            .lock()
            .await
            .items
            .get(&target.session.id)
            .is_some_and(|runtime| live(runtime.info.status))
        {
            let _ = super::wait(
                inner,
                &lead.id,
                Some(vec![target.session.id.clone()]),
                None,
                Some(progress),
                cancel,
            )
            .await;
            if cancel.is_cancelled() {
                return Ok(Delivery::Cancelled);
            }
            receipt = super::deliver(inner, request, &from, &target, message).await;
        }
    }
    if receipt.outcome == AgentMessageOutcome::Failed {
        return Err(CoreError::Session(
            receipt.error.unwrap_or_else(|| "投递失败".into()),
        ));
    }
    Ok(Delivery::Word(match receipt.outcome {
        AgentMessageOutcome::Injected => "injected",
        AgentMessageOutcome::Woken => "woken",
        AgentMessageOutcome::Failed => unreachable!("刚检查过不是 Failed"),
    }))
}

/// 执行一次 handoff。`sidekick` 是已解析的执行侧模型：其 `selector` 即目标
/// selector，模型切换也直接用它。`mode` 各变体的消息前置条件由工具层校验。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handoff(
    inner: &Arc<CoreInner>,
    request: &RequestContext,
    session_id: &str,
    sidekick: &crate::ResolvedModel,
    mode: HandoffMode,
    message: Option<&str>,
    block: bool,
    progress: &WaitProgress,
    cancel: &CancellationToken,
) -> Result<Handoff, CoreError> {
    let peers = peers(inner, session_id).await?;
    let lead = peers.me.clone();
    let desired = sidekick.selector.as_str();
    let message = message.unwrap_or_default();
    // peers 的 children 已剔除回退孤儿；同名 preset 按创建时间取最新的——
    // `fresh` 之后新建的会话必须稳定胜出，而不是取决于更新时间。
    let existing = peers
        .children
        .iter()
        .filter(|entry| entry.session.agent_preset.as_deref() == Some(preset::SIDEKICK_PRESET))
        .max_by_key(|entry| entry.session.created_at_ms)
        .map(|entry| entry.session.clone());

    let mut notes: Vec<String> = Vec::new();
    let (agent_id, delivery) = match mode {
        HandoffMode::Fresh => {
            if let Some(child) = &existing
                && live(status_of(inner, &child.id).await)
            {
                return Err(busy_error());
            }
            let spawned = spawn_sidekick(inner, request, session_id, message, desired).await?;
            (spawned.agent.id, "spawned")
        }
        HandoffMode::Retry => {
            let Some(child) = existing else {
                return Err(CoreError::Session("No sidekick to retry.".into()));
            };
            let status = status_of(inner, &child.id).await;
            if live(status) {
                return Err(busy_error());
            }
            // 上一轮是否正常结束看 transcript 的正向标记：注册表条目会被
            // 进程重启或终态回收（MAX_TERMINAL_AGENTS）弄丢，不能依赖它。
            let entries = inner.sessions.read_entries(&child.id).await?;
            if crate::service::prompt::turn_completed(&entries) {
                return Err(CoreError::Session(
                    "The sidekick's last handoff completed; send a new message instead.".into(),
                ));
            }
            let record = inner.sessions.get(&child.id).await?;
            let Ok(prompt_lock) = record.try_prompt_lock() else {
                return Err(finishing_error());
            };
            // 失败后用户可能已换 sidekick 模型：先同步，再带新模型重跑。
            let (target, note) =
                switch_to_desired(inner, request, &record, &child, sidekick).await?;
            if let Some(note) = note {
                notes.push(note);
            }
            super::retry_wakeup(
                Arc::clone(inner),
                request.clone(),
                session_id.to_owned(),
                &target,
                prompt_lock,
            )
            .await?;
            (child.id, "retried")
        }
        HandoffMode::Compact => match existing {
            // 没有既有 sidekick：等同首次 spawn。
            None => {
                let spawned = spawn_sidekick(inner, request, session_id, message, desired).await?;
                (spawned.agent.id, "spawned")
            }
            Some(child) => {
                if live(status_of(inner, &child.id).await) {
                    return Err(busy_error());
                }
                let record = inner.sessions.get(&child.id).await?;
                let Ok(prompt_lock) = record.try_prompt_lock() else {
                    return Err(finishing_error());
                };
                let (target, note) =
                    switch_to_desired(inner, request, &record, &child, sidekick).await?;
                if let Some(note) = note {
                    notes.push(note);
                }
                let instructions = format!(
                    "The lead's next handoff follows. Keep what is relevant to it; drop the rest.\n\n<next-handoff>\n{message}\n</next-handoff>"
                );
                // 压缩失败（如 compaction 被禁用）直接作为工具错误返回。
                crate::compact::manual(inner, request, &child.id, Some(&instructions), None, None)
                    .await?;
                drop(prompt_lock);
                match deliver_handoff(inner, request, &lead, &target, message, progress, cancel)
                    .await?
                {
                    Delivery::Cancelled => return Ok(Handoff::Cancelled),
                    Delivery::Word(_) => (child.id, "compacted"),
                }
            }
        },
        HandoffMode::Keep => match existing {
            None => {
                let spawned = spawn_sidekick(inner, request, session_id, message, desired).await?;
                (spawned.agent.id, "spawned")
            }
            Some(child) => {
                let mut target = child.clone();
                // 执行侧模型变了：空闲切成新 selector；忙则沿用旧模型跑完本轮，
                // 下次空闲 handoff 再切（结果 note 里写明）。
                if child.model != desired {
                    let locked = inner.sessions.get(&child.id).await.ok().and_then(|record| {
                        record.try_prompt_lock().ok().map(|lock| (record, lock))
                    });
                    match locked {
                        Some((record, _lock)) => {
                            let (switched, note) =
                                switch_to_desired(inner, request, &record, &child, sidekick)
                                    .await?;
                            if let Some(note) = note {
                                notes.push(note);
                            }
                            target = switched;
                        }
                        None => notes.push(format!(
                            "The sidekick is still running on {}; it switches to {desired} at its next idle handoff.",
                            child.model
                        )),
                    }
                }
                match deliver_handoff(inner, request, &lead, &target, message, progress, cancel)
                    .await?
                {
                    Delivery::Cancelled => return Ok(Handoff::Cancelled),
                    Delivery::Word(word) => (child.id, word),
                }
            }
        },
    };

    let agent_model = model_of(inner, &agent_id).await;

    if !block {
        let status = status_of(inner, &agent_id).await;
        notes.push(
            "Handoff delivered. Collect the report with wait_agent before ending your turn."
                .to_owned(),
        );
        return Ok(Handoff::Json(outcome_json(
            &agent_id,
            delivery,
            status,
            &agent_model,
            None,
            None,
            Some(notes.join(" ")),
        )));
    }

    // 阻塞等待期间标记 awaited：报告随工具结果直达 lead，完成通知不再投递。
    {
        let mut registry = inner.agents.lock().await;
        if let Some(runtime) = registry.items.get_mut(&agent_id) {
            runtime.awaited = true;
        }
    }
    let result = loop {
        let result = super::wait(
            inner,
            session_id,
            Some(vec![agent_id.clone()]),
            None,
            Some(progress),
            cancel,
        )
        .await?;
        if !result.timed_out {
            break result;
        }
    };
    if result.settled {
        let entry = result.agents.into_iter().next().expect("等待目标只有一个");
        return Ok(Handoff::Json(outcome_json(
            &agent_id,
            delivery,
            entry.status,
            &agent_model,
            entry.structured_result,
            entry.message.map(|message| message.text()),
            (!notes.is_empty()).then(|| notes.join(" ")),
        )));
    }
    // 提前返回：要么有新消息进 lead 的收件箱，要么目标恰好在间隙落了终态。
    let settled = {
        let mut registry = inner.agents.lock().await;
        registry.items.get_mut(&agent_id).and_then(|runtime| {
            runtime.awaited = false;
            terminal(runtime.info.status).then(|| {
                (
                    runtime.info.status,
                    runtime.message.clone(),
                    runtime.structured.clone(),
                )
            })
        })
    };
    if let Some((status, report, structured)) = settled {
        return Ok(Handoff::Json(outcome_json(
            &agent_id,
            delivery,
            status,
            &agent_model,
            structured,
            report.map(|message| message.text()),
            (!notes.is_empty()).then(|| notes.join(" ")),
        )));
    }
    if cancel.is_cancelled() {
        let _ = super::stop(inner, session_id.to_owned(), agent_id, None).await;
        return Ok(Handoff::Cancelled);
    }
    let status = status_of(inner, &agent_id).await;
    notes.push(
        "New messages arrived for you while the sidekick is still working. Handle them, then collect its report with wait_agent."
            .to_owned(),
    );
    Ok(Handoff::Json(outcome_json(
        &agent_id,
        delivery,
        status,
        &agent_model,
        None,
        None,
        Some(notes.join(" ")),
    )))
}

/// lead 的提示被用户取消后，停掉仍在后台运行的 sidekick。
///
/// 阻塞 handoff 的取消由 `handoff` 自己处理；这条覆盖的是 `block=false`
/// 留下继续跑的会话——lead 提示都没了，它的工作不应继续计费。
/// 只针对 sidekick：普通 `spawn_agent` 子代理行为不变。
pub(crate) async fn stop_running(inner: &CoreInner, lead_id: &str) {
    let registry = inner.agents.lock().await;
    for runtime in registry.items.values() {
        if runtime.info.parent_id == lead_id
            && runtime.info.preset.as_deref() == Some(preset::SIDEKICK_PRESET)
            && live(runtime.info.status)
        {
            runtime.cancel.cancel();
        }
    }
}
