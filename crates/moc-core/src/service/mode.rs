//! 会话模式切换、计划批准与只读工作区约束。

use moc_protocol::methods::{
    PlanAction, PlanStatus, SessionMode, SessionPlanApproveParams, SessionPlanApproveResult,
    SessionUpdate,
};
use moc_protocol::{RequestContext, RpcError, RpcErrorCode};

use super::{CoreInner, protocol_to_rpc, to_rpc};
use crate::compact;
use crate::session::entries::EntryPayload;

pub async fn approve_plan(
    inner: &CoreInner,
    ctx: RequestContext,
    params: SessionPlanApproveParams,
) -> Result<SessionPlanApproveResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _turn = record.try_turn_lock().map_err(to_rpc)?;
    let _file_lock = inner
        .sessions
        .acquire_lock(&params.session_id, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await.clone();
    let plan = state
        .plans
        .iter()
        .find(|plan| plan.plan_id == params.plan_id)
        .cloned()
        .ok_or_else(|| RpcError::of(RpcErrorCode::InvalidParams))?;
    if plan.status != PlanStatus::Submitted {
        return Err(RpcError::new(
            RpcErrorCode::InvalidParams.code(),
            "计划已处理",
        ));
    }

    if params.action == PlanAction::ExecuteCompacted {
        compact::manual(inner, &ctx, &params.session_id, Some("保留计划与关键决策"))
            .await
            .map_err(to_rpc)?;
    }

    if params.action == PlanAction::Reject {
        inner
            .sessions
            .append(
                &params.session_id,
                EntryPayload::Plan {
                    plan_id: plan.plan_id.clone(),
                    title: plan.title.clone(),
                    path: plan.path.clone(),
                    markdown: plan.markdown.clone(),
                    status: PlanStatus::Rejected,
                },
            )
            .await
            .map_err(to_rpc)?;
        inner
            .sessions
            .set_mode(&params.session_id, SessionMode::Plan)
            .await
            .map_err(to_rpc)?;
        ctx.updates()
            .emit(
                params.session_id.clone(),
                SessionUpdate::ModeChanged {
                    mode: SessionMode::Plan,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
        return Ok(SessionPlanApproveResult {
            session_id: params.session_id,
            mode: SessionMode::Plan,
            auto_prompt: None,
            plan_markdown: None,
        });
    }

    if params.action == PlanAction::ExecuteNewSession {
        let config = inner
            .project_config(std::path::Path::new(&state.session.cwd))
            .map_err(to_rpc)?;
        let model = config.resolve_model(&state.model).map_err(to_rpc)?;
        let child = inner
            .sessions
            .create(
                &state.session.cwd,
                &model.selector,
                SessionMode::Normal,
                Some(&plan.title),
                Some(params.session_id.clone()),
            )
            .await
            .map_err(to_rpc)?;
        let child_record = inner.sessions.get(&child.id).await.map_err(to_rpc)?;
        let child_path = child_record
            .paths
            .plans()
            .join(format!("{}.md", plan.plan_id));
        tokio::fs::write(&child_path, &plan.markdown)
            .await
            .map_err(protocol_to_rpc)?;
        inner
            .sessions
            .append(
                &child.id,
                EntryPayload::Plan {
                    plan_id: plan.plan_id.clone(),
                    title: plan.title.clone(),
                    path: format!("local://{}/plans/{}.md", child.id, plan.plan_id),
                    markdown: plan.markdown.clone(),
                    status: PlanStatus::Approved,
                },
            )
            .await
            .map_err(to_rpc)?;
        inner
            .sessions
            .append_message(
                &child.id,
                moc_llm::Message::user_text(format!(
                    "<active-plan id=\"{}\">\n{}\n</active-plan>",
                    plan.plan_id, plan.markdown
                )),
                None,
                None,
            )
            .await
            .map_err(to_rpc)?;
        return Ok(SessionPlanApproveResult {
            session_id: child.id,
            mode: SessionMode::Normal,
            auto_prompt: Some("开始执行计划。".into()),
            plan_markdown: Some(plan.markdown),
        });
    }

    inner
        .sessions
        .append(
            &params.session_id,
            EntryPayload::Plan {
                plan_id: plan.plan_id.clone(),
                title: plan.title.clone(),
                path: plan.path.clone(),
                markdown: plan.markdown.clone(),
                status: PlanStatus::Approved,
            },
        )
        .await
        .map_err(to_rpc)?;
    inner
        .sessions
        .set_mode(&params.session_id, SessionMode::Normal)
        .await
        .map_err(to_rpc)?;
    ctx.updates()
        .emit(
            params.session_id.clone(),
            SessionUpdate::ModeChanged {
                mode: SessionMode::Normal,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(SessionPlanApproveResult {
        session_id: params.session_id,
        mode: SessionMode::Normal,
        auto_prompt: Some("开始执行计划。".into()),
        plan_markdown: None,
    })
}

pub fn allowed_tools(mode: SessionMode) -> &'static [&'static str] {
    match mode {
        SessionMode::Normal => &[
            "read", "grep", "find", "todo", "job", "bash", "edit", "write", "ask", "skill",
        ],
        SessionMode::Ask => &[
            "read", "grep", "find", "todo", "job", "bash", "ask", "skill",
        ],
        SessionMode::Plan => &[
            "read",
            "grep",
            "find",
            "todo",
            "job",
            "bash",
            "ask",
            "skill",
            "submit_plan",
        ],
    }
}

/// 是否允许在当前模式调用该工具。
///
/// 内置工具沿用名字白名单；动态（MCP）工具按风险类别判定：
/// Normal 全量放行，Ask/Plan 只放行 `Tier::Read` 的只读工具。
pub fn mode_allows(mode: SessionMode, name: &str, tier: crate::tools::Tier) -> bool {
    if allowed_tools(mode).contains(&name) {
        return true;
    }
    if !name.starts_with("mcp__") {
        return false;
    }
    match mode {
        SessionMode::Normal => true,
        SessionMode::Ask | SessionMode::Plan => tier == crate::tools::Tier::Read,
    }
}
