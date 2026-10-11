//! 会话模式切换、计划批准与只读工作区约束。

use moc_protocol::methods::{
    PlanAction, PlanStatus, SessionMode, SessionPlanApproveParams, SessionPlanApproveResult,
    SessionUpdate,
};
use moc_protocol::{RequestContext, RpcError, RpcErrorCode};

use super::{CoreInner, protocol_to_rpc, to_rpc};
use crate::compact;
use crate::error::CoreError;
use crate::session::entries::EntryPayload;

/// 计划模式切换时的会话模型同步：进入计划模式应用 `plan` 角色模型，退出时恢复
/// 进入前的会话模型。
///
/// `plan_model` 是进入计划模式时预先解析好的目标模型选择器；`plan = "none"` 表示
/// 不切换，此时传 `None`。返回切换后的选择器；模型未变化时为 `None`，调用方据此
/// 决定是否推送 [`SessionUpdate::ModelChanged`]。
pub async fn sync_plan_model(
    inner: &CoreInner,
    session_id: &str,
    from: SessionMode,
    to: SessionMode,
    plan_model: Option<String>,
) -> Result<Option<String>, CoreError> {
    if from == to {
        return Ok(None);
    }
    let record = inner.sessions.get(session_id).await?;
    let target = {
        let state = record.state.lock().await;
        let target = if to == SessionMode::Plan {
            plan_model
        } else if from == SessionMode::Plan {
            Some(state.exit_plan_model().to_owned())
        } else {
            None
        };
        target.filter(|target| *target != state.model)
    };
    let Some(target) = target else {
        return Ok(None);
    };
    inner.sessions.set_model(session_id, target.clone()).await?;
    Ok(Some(target))
}

/// 推送模式变化；模型随模式切换时按「先模式后模型」的顺序补推模型变化。
pub async fn emit_mode_change(
    ctx: &RequestContext,
    session_id: &str,
    mode: SessionMode,
    model: Option<String>,
) -> Result<(), RpcError> {
    ctx.updates()
        .emit(session_id.to_owned(), SessionUpdate::ModeChanged { mode })
        .await
        .map_err(protocol_to_rpc)?;
    if let Some(model) = model {
        ctx.updates()
            .emit(session_id.to_owned(), SessionUpdate::ModelChanged { model })
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(())
}

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
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
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
            "The plan has already been processed",
        ));
    }

    if params.action == PlanAction::ExecuteCompacted {
        compact::manual(
            inner,
            &ctx,
            &params.session_id,
            Some("Preserve the plan and key decisions"),
        )
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
        emit_mode_change(&ctx, &params.session_id, SessionMode::Plan, None).await?;
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
        // 执行会话用进入计划模式前的会话模型，而不是计划模型。
        let model = config
            .resolve_model(state.exit_plan_model())
            .map_err(to_rpc)?;
        let child = inner
            .sessions
            .create(
                &state.session.cwd,
                &model.selector,
                SessionMode::Normal,
                state.permission,
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
        tokio::fs::create_dir_all(child_record.paths.plans())
            .await
            .map_err(CoreError::Io)
            .map_err(to_rpc)?;
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
            auto_prompt: Some("Execute the approved plan.".into()),
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
        .append_message(
            &params.session_id,
            moc_llm::Message::user_text(format!(
                "<active-plan id=\"{}\">\n{}\n</active-plan>",
                plan.plan_id, plan.markdown
            )),
            None,
            None,
        )
        .await
        .map_err(to_rpc)?;
    inner
        .sessions
        .set_mode(&params.session_id, SessionMode::Normal)
        .await
        .map_err(to_rpc)?;
    let model = sync_plan_model(
        inner,
        &params.session_id,
        SessionMode::Plan,
        SessionMode::Normal,
        None,
    )
    .await
    .map_err(to_rpc)?;
    emit_mode_change(&ctx, &params.session_id, SessionMode::Normal, model).await?;
    Ok(SessionPlanApproveResult {
        session_id: params.session_id,
        mode: SessionMode::Normal,
        auto_prompt: Some("Execute the approved plan.".into()),
        plan_markdown: None,
    })
}

pub fn allowed_tools(mode: SessionMode) -> &'static [&'static str] {
    match mode {
        SessionMode::Normal | SessionMode::ClmExperimental => &[
            "read",
            "grep",
            "find",
            "todo",
            "job",
            "bash",
            "edit",
            "write",
            "ask",
            "spawn_agent",
            "list_agents",
            "send_agent",
            "steer_agent",
            "stop_agent",
            "wait_agent",
            "skill",
            "clm_context",
            "run_code",
        ],
        SessionMode::Ask => &[
            "read", "grep", "find", "todo", "job", "bash", "ask", "skill", "run_code",
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
            "run_code",
        ],
        SessionMode::Ptc => &["run_code"],
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
        SessionMode::Normal | SessionMode::ClmExperimental => true,
        SessionMode::Ask | SessionMode::Plan => tier == crate::tools::Tier::Read,
        SessionMode::Ptc => false,
    }
}

pub fn tool_enabled(
    name: &str,
    ask_supported: bool,
    config: &crate::config::ResolvedConfig,
) -> bool {
    match name {
        "ask" => ask_supported,
        "todo" => config.tools.todo.enabled,
        "job" => config.tools.job.enabled,
        "skill" => config.skills.enabled,
        _ => true,
    }
}

#[cfg(test)]
mod agent_mode_tests {
    use super::allowed_tools;
    use moc_protocol::SessionMode;

    #[test]
    fn child_agent_controls_only_exist_in_normal_modes() {
        let agents = [
            "spawn_agent",
            "list_agents",
            "send_agent",
            "steer_agent",
            "stop_agent",
            "wait_agent",
        ];
        for mode in [SessionMode::Normal, SessionMode::ClmExperimental] {
            let allowed = allowed_tools(mode);
            assert!(agents.iter().all(|tool| allowed.contains(tool)));
        }
        for mode in [SessionMode::Ask, SessionMode::Plan, SessionMode::Ptc] {
            let allowed = allowed_tools(mode);
            assert!(agents.iter().all(|tool| !allowed.contains(tool)));
        }
    }
}
