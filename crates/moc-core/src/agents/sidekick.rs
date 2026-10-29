//! `sidekick` 工具的 handoff 逻辑：给 Fusion lead 的持久执行侧投递工作。
//!
//! 与普通 spawn 的差异：同一会话跨 handoff 复用（上下文延续），首次创建走
//! `sidekick` preset 的内部 spawn 路径，后续 handoff 就是一次代理消息投递——
//! 运行中注入、空闲时唤醒。

use std::sync::Arc;

use moc_protocol::RequestContext;
use moc_protocol::methods::{
    AgentMessageMeta, AgentMessageOutcome, AgentStatus, SessionModelOverflowAction,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{SpawnModel, SpawnSpec, WaitProgress, live, peers, preset, status_word, terminal};
use crate::error::CoreError;
use crate::service::CoreInner;

/// handoff 的两种收口。
pub(crate) enum Handoff {
    /// 正常产出：JSON 同时作为工具结果正文与 `structured_content`。
    ///
    /// `agent_id` 必须存在：`referenced_agent_ids` 依赖它做回退/孤儿跟踪。
    Json(Value),
    /// lead 的取消令牌触发：sidekick 已停，工具层回统一的取消结果。
    Cancelled,
}

fn outcome_json(
    agent_id: &str,
    delivery: &str,
    status: AgentStatus,
    report: Option<String>,
    note: Option<&str>,
) -> Value {
    let mut body = json!({
        "agent_id": agent_id,
        "delivery": delivery,
        "status": status_word(status),
    });
    if let Some(report) = report {
        body["report"] = Value::String(report);
    }
    if let Some(note) = note {
        body["note"] = Value::String(note.to_owned());
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

/// 执行一次 handoff。`sidekick` 是已解析的执行侧模型：其 `selector` 即目标
/// selector，模型切换也直接用它。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handoff(
    inner: &Arc<CoreInner>,
    request: &RequestContext,
    session_id: &str,
    sidekick: &crate::ResolvedModel,
    message: &str,
    block: bool,
    progress: &WaitProgress,
    cancel: &CancellationToken,
) -> Result<Handoff, CoreError> {
    let peers = peers(inner, session_id).await?;
    let lead = peers.me.clone();
    let desired = sidekick.selector.as_str();
    // peers 的 children 已剔除回退孤儿；同名 preset 取最近活动的那个。
    let existing = peers
        .children
        .iter()
        .filter(|entry| entry.session.agent_preset.as_deref() == Some(preset::SIDEKICK_PRESET))
        .max_by_key(|entry| entry.session.updated_at_ms)
        .map(|entry| entry.session.clone());

    let (agent_id, delivery) = match existing {
        None => {
            let spawned = super::spawn(
                Arc::clone(inner),
                request.clone(),
                session_id.to_owned(),
                SpawnSpec {
                    task: message.to_owned(),
                    title: Some("Sidekick".into()),
                    model: SpawnModel::Exact(desired.to_owned()),
                    preset: Some(preset::SIDEKICK_PRESET.to_owned()),
                    context: None,
                    output_schema: None,
                },
            )
            .await?;
            (spawned.agent.id, "spawned")
        }
        Some(child) => {
            let mut target = child.clone();
            // 执行侧模型变了：子代理空闲就切成新 selector，忙则沿用旧模型完成本轮。
            if child.model != desired
                && let Ok(record) = inner.sessions.get(&child.id).await
                && let Ok(_prompt_lock) = record.try_prompt_lock()
            {
                inner
                    .sessions
                    .ensure_write_lease(&child.id, &record, &request.client().frontend)
                    .await?;
                target = crate::service::model::switch(
                    inner,
                    request,
                    &child.id,
                    sidekick,
                    SessionModelOverflowAction::Compact,
                )
                .await?
                .session;
            }
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
                    .get(&child.id)
                    .is_some_and(|runtime| live(runtime.info.status))
                {
                    let _ = super::wait(
                        inner,
                        session_id,
                        Some(vec![child.id.clone()]),
                        None,
                        Some(progress),
                        cancel,
                    )
                    .await;
                    if cancel.is_cancelled() {
                        return Ok(Handoff::Cancelled);
                    }
                    receipt = super::deliver(inner, request, &from, &target, message).await;
                }
            }
            if receipt.outcome == AgentMessageOutcome::Failed {
                return Err(CoreError::Session(
                    receipt.error.unwrap_or_else(|| "投递失败".into()),
                ));
            }
            let delivery = match receipt.outcome {
                AgentMessageOutcome::Injected => "injected",
                AgentMessageOutcome::Woken => "woken",
                AgentMessageOutcome::Failed => unreachable!("刚检查过不是 Failed"),
            };
            (child.id, delivery)
        }
    };

    if !block {
        let status = status_of(inner, &agent_id).await;
        return Ok(Handoff::Json(outcome_json(
            &agent_id,
            delivery,
            status,
            None,
            Some("Handoff delivered. Collect the report with wait_agent before ending your turn."),
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
            entry.message.map(|message| message.text()),
            None,
        )));
    }
    // 提前返回：要么有新消息进 lead 的收件箱，要么目标恰好在间隙落了终态。
    let settled = {
        let mut registry = inner.agents.lock().await;
        registry.items.get_mut(&agent_id).and_then(|runtime| {
            runtime.awaited = false;
            terminal(runtime.info.status).then(|| (runtime.info.status, runtime.message.clone()))
        })
    };
    if let Some((status, report)) = settled {
        return Ok(Handoff::Json(outcome_json(
            &agent_id,
            delivery,
            status,
            report.map(|message| message.text()),
            None,
        )));
    }
    if cancel.is_cancelled() {
        let _ = super::stop(inner, session_id.to_owned(), agent_id, None).await;
        return Ok(Handoff::Cancelled);
    }
    let status = status_of(inner, &agent_id).await;
    Ok(Handoff::Json(outcome_json(
        &agent_id,
        delivery,
        status,
        None,
        Some(
            "New messages arrived for you while the sidekick is still working. Handle them, then collect its report with wait_agent.",
        ),
    )))
}
