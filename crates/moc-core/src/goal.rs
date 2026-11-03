//! 会话目标（goal）：持久目标与自动续轮的域逻辑。
//!
//! 三层分工：本模块管状态机与续跑驱动；`session/goal` RPC 是用户面；
//! `get_goal` / `create_goal` / `update_goal` 工具是模型面。持久化走
//! `GoalChange` 条目（快照 + 清除墓碑），续跑轮次回放自 `Message.goal` 标记。
//!
//! 停止面只有四条：模型 `complete`/`blocked`、用户 `pause`/`clear`、
//! armed 被摘掉（取消 / 循环 / 错误 / length）、目标不存在。没有轮数上限，
//! 也没有 token 预算——目标跑到达成为止。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use moc_protocol::methods::{
    GoalAction, GoalBlockReason, GoalPhase, GoalRoundMeta, GoalView, InputPart, PromptParams,
    SessionUpdate,
};
use moc_protocol::{RequestContext, RpcError};

use crate::service::{CoreInner, invalid_params, protocol_to_rpc, to_rpc};
use crate::session::entries::GoalSnapshot;

/// goal 续跑轮里允许 `blocked` 的最小轮号：前三轮必须先推进过工作，
/// 不允许第一轮就以「做不到」收场。
pub(crate) const MIN_ROUND_BEFORE_BLOCK: u32 = 4;

/// 模型自报 blocked 的稳定分类码。
pub(crate) const BLOCK_CODE_MODEL: &str = "model-reported";

fn err(code: &'static str, message: impl Into<String>) -> RpcError {
    invalid_params(format!("{code}: {}", message.into()))
}

/// 快照 + 回放计数 + 进程内 armed → 线上投影。
pub(crate) fn view(
    snapshot: Option<&GoalSnapshot>,
    rounds_started: u32,
    armed: bool,
) -> Option<GoalView> {
    snapshot.map(|goal| GoalView {
        id: goal.id.clone(),
        revision: goal.revision,
        objective: goal.objective.clone(),
        phase: goal.phase,
        blocked_reason: goal.blocked_reason.clone(),
        rounds_started,
        created_at_ms: goal.created_at_ms,
        updated_at_ms: goal.updated_at_ms,
        armed,
    })
}

fn current_view(
    state: &crate::session::state::SessionState,
    runtime: &crate::service::SessionRuntime,
) -> Option<GoalView> {
    view(
        state.goal.as_ref(),
        state.goal_rounds_started,
        runtime.goal_armed.load(Ordering::Relaxed),
    )
}

/// `session/goal` 的动作语义；模型工具的 `update_goal` 走 [`apply_tool_update`]。
pub(crate) async fn apply_action(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
    action: GoalAction,
) -> Result<Option<GoalView>, RpcError> {
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let runtime = inner.runtime(session_id).await;
    // 目标操作串行化：读-改-写之间不能有第二个目标变更插队（含工具侧）。
    let _op = runtime.goal_op.lock().await;
    if record.state.lock().await.session.is_subagent() {
        return Err(err("GOAL_NOT_SUPPORTED", "子代理会话不支持目标"));
    }
    // `show` 是只读：不取写租约，别的进程写会话时也能查。
    if !matches!(action, GoalAction::Show) {
        inner
            .sessions
            .ensure_write_lease(session_id, &record, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
    }
    let now = crate::session::store::now_ms();
    let next: Option<GoalSnapshot> = {
        let state = record.state.lock().await;
        match &action {
            GoalAction::Show | GoalAction::Unset => None,
            GoalAction::Create { objective } => {
                let objective = objective.trim();
                if objective.is_empty() {
                    return Err(err("GOAL_INVALID_OBJECTIVE", "objective 不能为空"));
                }
                if let Some(goal) = &state.goal
                    && goal.phase != GoalPhase::Complete
                {
                    return Err(err(
                        "GOAL_ALREADY_EXISTS",
                        "已有进行中的目标；/goal edit 修改或 /goal clear 清除后再建",
                    ));
                }
                Some(GoalSnapshot {
                    id: crate::ids::goal_id(),
                    revision: 1,
                    objective: objective.to_owned(),
                    phase: GoalPhase::Active,
                    blocked_reason: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                })
            }
            GoalAction::Edit { objective } => {
                let objective = objective.trim();
                if objective.is_empty() {
                    return Err(err("GOAL_INVALID_OBJECTIVE", "objective 不能为空"));
                }
                let Some(goal) = &state.goal else {
                    return Err(err("GOAL_NOT_FOUND", "当前没有目标可编辑"));
                };
                if goal.phase == GoalPhase::Complete {
                    return Err(err(
                        "GOAL_INVALID_TRANSITION",
                        "目标已完成；/goal <objective> 可直接建立新目标",
                    ));
                }
                Some(GoalSnapshot {
                    revision: goal.revision.saturating_add(1),
                    objective: objective.to_owned(),
                    updated_at_ms: now,
                    ..goal.clone()
                })
            }
            GoalAction::Pause => {
                let Some(goal) = &state.goal else {
                    return Err(err("GOAL_NOT_FOUND", "当前没有目标可暂停"));
                };
                match goal.phase {
                    GoalPhase::Active => Some(GoalSnapshot {
                        phase: GoalPhase::Paused,
                        updated_at_ms: now,
                        ..goal.clone()
                    }),
                    // paused/blocked 下 pause 幂等返回原状；complete 拒绝。
                    GoalPhase::Paused | GoalPhase::Blocked => None,
                    GoalPhase::Complete => {
                        return Err(err("GOAL_INVALID_TRANSITION", "目标已完成，无法暂停"));
                    }
                }
            }
            GoalAction::Resume => {
                let Some(goal) = &state.goal else {
                    return Err(err("GOAL_NOT_FOUND", "当前没有目标可恢复"));
                };
                match goal.phase {
                    GoalPhase::Paused | GoalPhase::Blocked => Some(GoalSnapshot {
                        phase: GoalPhase::Active,
                        blocked_reason: None,
                        updated_at_ms: now,
                        ..goal.clone()
                    }),
                    GoalPhase::Active => None,
                    GoalPhase::Complete => {
                        return Err(err("GOAL_INVALID_TRANSITION", "目标已完成，无法恢复"));
                    }
                }
            }
        }
    };
    let mut changed = false;
    match action {
        GoalAction::Show => {}
        GoalAction::Unset => {
            // 无目标时 clear 幂等成功；有目标才写墓碑。
            if record.state.lock().await.goal.is_some() {
                inner
                    .sessions
                    .append_goal_change(session_id, None)
                    .await
                    .map_err(to_rpc)?;
                changed = true;
            }
        }
        _ => {
            if let Some(snapshot) = next {
                inner
                    .sessions
                    .append_goal_change(session_id, Some(snapshot))
                    .await
                    .map_err(to_rpc)?;
                changed = true;
            }
        }
    }
    // armed 语义：create/resume 激活续跑；pause/clear 摘掉；edit 保持原值；
    // 落库失败的路径到不了这里。
    match action {
        GoalAction::Create { .. } | GoalAction::Resume => {
            runtime.goal_armed.store(true, Ordering::Relaxed)
        }
        GoalAction::Pause | GoalAction::Unset => runtime.goal_armed.store(false, Ordering::Relaxed),
        GoalAction::Show | GoalAction::Edit { .. } => {}
    }
    // 用户暂停：正在跑的、属于该目标的 goal round 一并中止（对齐 DSH：宿主
    // pause 中止在跑的轮；别的提示不受影响）。
    if matches!(action, GoalAction::Pause) {
        let abort = record.state.lock().await.goal.as_ref().is_some_and(|goal| {
            runtime
                .goal_active_round
                .lock()
                .as_ref()
                .is_some_and(|meta| meta.goal_id == goal.id)
        });
        if abort && let Some(guard) = runtime.guard.lock().as_ref() {
            guard.cancel.cancel();
        }
    }
    let current = current_view(&*record.state.lock().await, &runtime);
    // 无变更的幂等动作（对已 paused 的 pause、对已 active+armed 的 resume）不发事件。
    if changed {
        ctx.updates()
            .emit(
                session_id.to_owned(),
                SessionUpdate::GoalChanged {
                    goal: current.clone(),
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    // create/edit/resume 之后立即评估续跑；提示在跑时抢不到锁，由那一轮的收尾接力。
    if matches!(
        action,
        GoalAction::Create { .. } | GoalAction::Edit { .. } | GoalAction::Resume
    ) {
        maybe_continue(inner, ctx, session_id).await;
    }
    Ok(current)
}

/// 发起 `update_goal` 的轮次身份；权限规则按它区分。
pub(crate) enum TurnKind<'a> {
    /// 人类直发轮。
    Human,
    /// goal 续跑轮；携带本轮来源标记。
    GoalRound(&'a GoalRoundMeta),
    /// 代理唤醒轮。
    Agent,
}

/// 模型 `update_goal` 的动作。
pub(crate) enum ToolUpdate {
    Edit { objective: String },
    Pause,
    Resume,
    Complete,
    Blocked { reason: GoalBlockReason },
}

/// 模型工具 `update_goal`：CAS 校验 `{goal_id, revision}` 后按动作迁移。
///
/// 权限：`edit`/`pause`/`resume` 只在人类直发轮允许；`complete`/`blocked`
/// 人类轮与 goal round 都允许，但 goal round 里 `blocked` 要求
/// [`MIN_ROUND_BEFORE_BLOCK`]——续跑先真跑过三轮，才接受「做不到」的结论。
/// durable paused 的 `resume` 一律拒绝（那是用户语义，走 `/goal resume`）。
pub(crate) async fn apply_tool_update(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
    goal_id: &str,
    revision: u32,
    update: ToolUpdate,
    turn: TurnKind<'_>,
) -> Result<Option<GoalView>, RpcError> {
    match (&update, &turn) {
        (
            ToolUpdate::Edit { .. } | ToolUpdate::Pause | ToolUpdate::Resume,
            TurnKind::GoalRound(_) | TurnKind::Agent,
        ) => {
            return Err(err(
                "GOAL_FORBIDDEN_IN_ROUND",
                "非人类轮次里只能 get / complete / blocked",
            ));
        }
        (ToolUpdate::Blocked { .. }, TurnKind::GoalRound(meta))
            if meta.round < MIN_ROUND_BEFORE_BLOCK =>
        {
            return Err(err(
                "GOAL_BLOCK_TOO_EARLY",
                format!(
                    "第 {} 轮不能报 blocked：续跑未满 {} 轮前必须推进工作或换打法",
                    meta.round,
                    MIN_ROUND_BEFORE_BLOCK - 1
                ),
            ));
        }
        (ToolUpdate::Blocked { .. }, TurnKind::Agent) => {
            return Err(err("GOAL_FORBIDDEN_IN_ROUND", "代理唤醒轮不能报 blocked"));
        }
        _ => {}
    }
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let runtime = inner.runtime(session_id).await;
    let _op = runtime.goal_op.lock().await;
    let now = crate::session::store::now_ms();
    let next = {
        let state = record.state.lock().await;
        let Some(goal) = &state.goal else {
            return Err(err("GOAL_NOT_FOUND", "当前没有目标"));
        };
        if goal.id != goal_id || goal.revision != revision {
            return Err(err(
                "GOAL_STALE_REVISION",
                format!(
                    "目标已变更（当前 revision {}）；先 get_goal 拿最新值再提交",
                    goal.revision
                ),
            ));
        }
        match update {
            ToolUpdate::Edit { ref objective } => {
                let objective = objective.trim();
                if objective.is_empty() {
                    return Err(err("GOAL_INVALID_OBJECTIVE", "objective 不能为空"));
                }
                if goal.phase == GoalPhase::Complete {
                    return Err(err("GOAL_INVALID_TRANSITION", "目标已完成，无法编辑"));
                }
                Some(GoalSnapshot {
                    revision: goal.revision.saturating_add(1),
                    objective: objective.to_owned(),
                    updated_at_ms: now,
                    ..goal.clone()
                })
            }
            ToolUpdate::Pause => match goal.phase {
                GoalPhase::Active => Some(GoalSnapshot {
                    phase: GoalPhase::Paused,
                    updated_at_ms: now,
                    ..goal.clone()
                }),
                _ => {
                    return Err(err("GOAL_INVALID_TRANSITION", "只有 active 目标能 pause"));
                }
            },
            ToolUpdate::Resume => {
                match goal.phase {
                    // durable paused 的恢复是用户语义；模型侧只允许唤醒 disarmed。
                    GoalPhase::Paused => {
                        return Err(err(
                            "GOAL_RESUME_PAUSED",
                            "目标已被用户暂停，恢复请走 /goal resume",
                        ));
                    }
                    GoalPhase::Blocked => Some(GoalSnapshot {
                        phase: GoalPhase::Active,
                        blocked_reason: None,
                        updated_at_ms: now,
                        ..goal.clone()
                    }),
                    GoalPhase::Active => None,
                    GoalPhase::Complete => {
                        return Err(err("GOAL_INVALID_TRANSITION", "目标已完成，无法恢复"));
                    }
                }
            }
            ToolUpdate::Complete => match goal.phase {
                GoalPhase::Complete => {
                    return Err(err("GOAL_INVALID_TRANSITION", "目标已完成"));
                }
                _ => Some(GoalSnapshot {
                    phase: GoalPhase::Complete,
                    blocked_reason: None,
                    updated_at_ms: now,
                    ..goal.clone()
                }),
            },
            ToolUpdate::Blocked { ref reason } => match goal.phase {
                GoalPhase::Complete => {
                    return Err(err("GOAL_INVALID_TRANSITION", "目标已完成"));
                }
                _ => Some(GoalSnapshot {
                    phase: GoalPhase::Blocked,
                    blocked_reason: Some(reason.clone()),
                    updated_at_ms: now,
                    ..goal.clone()
                }),
            },
        }
    };
    if let Some(snapshot) = next {
        inner
            .sessions
            .append_goal_change(session_id, Some(snapshot))
            .await
            .map_err(to_rpc)?;
    }
    match update {
        ToolUpdate::Complete | ToolUpdate::Blocked { .. } | ToolUpdate::Pause => {
            runtime.goal_armed.store(false, Ordering::Relaxed)
        }
        ToolUpdate::Resume | ToolUpdate::Edit { .. } => {
            runtime.goal_armed.store(true, Ordering::Relaxed)
        }
    }
    let current = current_view(&*record.state.lock().await, &runtime);
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::GoalChanged {
                goal: current.clone(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    // edit/resume 之后评估续跑；complete/blocked/pause 不再续。
    if matches!(update, ToolUpdate::Edit { .. } | ToolUpdate::Resume) {
        maybe_continue(inner, ctx, session_id).await;
    }
    Ok(current)
}

/// `create_goal` 工具：模型在人类直发轮里从请求推断出目标。
pub(crate) async fn apply_tool_create(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
    objective: &str,
) -> Result<Option<GoalView>, RpcError> {
    apply_action(
        inner,
        ctx,
        session_id,
        GoalAction::Create {
            objective: objective.to_owned(),
        },
    )
    .await
}

/// 提示结束的统一钩子：目标仍活跃且 armed 时，抢提示锁续跑下一轮。
///
/// 所有提示路径（用户提示、agent 唤醒、retry、上一轮 goal round、detached 投递）
/// 在 `run_prompt_with_lock` 收尾共用本函数，续跑因此自动成链，直到相位离开
/// active 或 armed 被摘掉。抢不到锁说明有提示在跑，它的收尾会重新评估。
pub(crate) async fn maybe_continue(inner: &Arc<CoreInner>, ctx: &RequestContext, session_id: &str) {
    let Ok(record) = inner.sessions.get(session_id).await else {
        return;
    };
    let runtime = inner.runtime(session_id).await;
    if !runtime.goal_armed.load(Ordering::Relaxed) {
        return;
    }
    // 预约本轮的身份快照：抢到提示锁后要逐一复核，任何一个变了就放弃本轮。
    let reservation = {
        let state = record.state.lock().await;
        state
            .goal
            .as_ref()
            .filter(|goal| goal.phase == GoalPhase::Active)
            .map(|goal| (goal.id.clone(), goal.revision, goal.objective.clone()))
    };
    let Some((goal_id, revision, objective)) = reservation else {
        return;
    };
    let Ok(prompt_lock) = record.try_prompt_lock() else {
        // 有提示在跑：它的收尾会重新评估本条件，本轮由它接力。
        return;
    };
    // 编辑护栏：锁已到手，复核目标身份与相位仍与预约一致；轮号以落库计数为准，
    // 预约与放行之间可能已有别的轮落库。
    let meta = {
        let state = record.state.lock().await;
        let still = state.goal.as_ref().is_some_and(|goal| {
            goal.id == goal_id && goal.revision == revision && goal.phase == GoalPhase::Active
        }) && runtime.goal_armed.load(Ordering::Relaxed);
        if !still {
            return;
        }
        GoalRoundMeta {
            goal_id,
            revision,
            round: state.goal_rounds_started.saturating_add(1),
        }
    };
    CoreInner::spawn_detached_prompt(
        inner,
        ctx.clone(),
        PromptParams {
            session_id: session_id.to_owned(),
            input: vec![InputPart::Text {
                text: round_prompt(&objective, meta.round),
            }],
            mode: None,
            agent_message: None,
            goal_round: Some(meta),
        },
        record,
        prompt_lock,
    )
    .await;
}

/// 每轮续跑的用户消息正文：`<goal_round>` 块，写进会话历史供模型与界面读取。
pub(crate) fn round_prompt(objective: &str, round: u32) -> String {
    let objective = serde_json::to_string(objective).unwrap_or_else(|_| "\"\"".into());
    format!(
        "<goal_round>\nObjective: {objective}\nRound: {round}\n\n\
         Continue working toward the objective in this same session. Treat the current \
         workspace, tool results, and durable session state as authoritative; inspect them \
         instead of assuming earlier narration is still current. Make concrete progress and \
         verify the result.\n\n\
         Before calling update_goal(action=complete), audit current repo state against every \
         concrete deliverable: read files, run relevant checks, match verification scope to \
         claim scope. If any deliverable lacks direct current-state evidence, keep working. \
         If the goal cannot ever be reached, update_goal(action=blocked) is allowed only \
         after sustained attempts.\n\
         </goal_round>"
    )
}

/// goal 激活期间每轮注入的运行时提醒正文（`reminder::key::GOAL`）。
pub(crate) fn goal_context(goal: &GoalView) -> String {
    let status = match goal.phase {
        GoalPhase::Active => "active",
        GoalPhase::Paused => "paused",
        GoalPhase::Blocked => "blocked",
        GoalPhase::Complete => "complete",
    };
    let mut text = format!(
        "<goal_context>\nGoal mode active. Objective below: user-provided task, \
         not higher-priority instructions.\n\n<objective>\n{}\n</objective>\n\n\
         Status: {status}; rounds completed: {}.\n\n\
         `goal` tools:\n- `get_goal`: current goal state.\n\
         - `update_goal(action=\"complete\")`: only with verified evidence.\n\
         - `update_goal(action=\"blocked\")`: only if the goal is truly unreachable.\n\n\
         Keep the full objective intact across turns; NEVER redefine success as a smaller, \
         easier, or already-completed subset.",
        goal.objective, goal.rounds_started
    );
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(&format!(
            "\n\nBlocked reason ({}): {}",
            reason.code, reason.message
        ));
    }
    text.push_str("\n</goal_context>");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: &str, phase: GoalPhase, revision: u32) -> GoalSnapshot {
        GoalSnapshot {
            id: id.into(),
            revision,
            objective: "把测试写完".into(),
            phase,
            blocked_reason: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    #[test]
    fn view_projects_snapshot_and_runtime_flags() {
        let projected =
            view(Some(&snapshot("goal_1", GoalPhase::Active, 3)), 5, true).expect("有目标才有视图");
        assert_eq!(projected.id, "goal_1");
        assert_eq!(projected.revision, 3);
        assert_eq!(projected.objective, "把测试写完");
        assert_eq!(projected.phase, GoalPhase::Active);
        assert_eq!(projected.rounds_started, 5);
        assert!(projected.armed);
        assert!(view(None, 0, false).is_none());
    }

    #[test]
    fn round_prompt_embeds_objective_and_round_number() {
        let text = round_prompt("目标是 \"A\" 且换\n行", 7);
        assert!(text.contains("<goal_round>") && text.contains("</goal_round>"));
        // objective 是 JSON 转义后的值：引号与换行不会撑破块边界。
        assert!(text.contains(r#"Objective: "目标是 \"A\" 且换\n行""#));
        assert!(text.contains("Round: 7"));
        assert!(text.contains("update_goal(action=complete)"));
        assert!(text.contains("update_goal(action=blocked)"));
    }

    #[test]
    fn goal_context_reports_phase_rounds_and_tools() {
        let mut goal =
            view(Some(&snapshot("goal_9", GoalPhase::Blocked, 2)), 11, false).expect("有目标");
        goal.blocked_reason = Some(GoalBlockReason {
            code: "model-reported".into(),
            message: "依赖缺失".into(),
        });
        let text = goal_context(&goal);
        assert!(text.contains("<goal_context>"));
        assert!(text.contains("把测试写完"));
        assert!(text.contains("Status: blocked"));
        assert!(text.contains("rounds completed: 11"));
        assert!(text.contains("Blocked reason (model-reported): 依赖缺失"));
        assert!(text.contains("get_goal") && text.contains("update_goal"));
    }
}
