//! goal 工具组：模型面的持久目标读写。
//!
//! `get_goal` / `create_goal` / `update_goal` 三件套；生命周期、CAS 与续跑
//! 驱动在 `crate::goal` 域模块，本文件只做参数解析与结果渲染。
//! 权限概览：
//! - `get_goal`：任何轮次可读。
//! - `create_goal`：仅人类直发轮（子代理会话不暴露本组工具）。
//! - `update_goal`：`edit`/`pause`/`resume` 仅人类直发轮；`complete`/`blocked`
//!   人类轮与 goal round 都允许，但 goal round 里 `blocked` 需满
//!   [`crate::goal::MIN_ROUND_BEFORE_BLOCK`]。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::RpcError;
use moc_protocol::methods::{GoalBlockReason, GoalView};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

/// 把域错误转成工具错误：域层把稳定码放在消息前缀（`GOAL_X: msg`），
/// 识别已知码转成结构化 `error_code`，其余的按普通 GOAL 失败报。
fn goal_error(error: RpcError) -> ToolError {
    let known = [
        "GOAL_NOT_FOUND",
        "GOAL_STALE_REVISION",
        "GOAL_ALREADY_EXISTS",
        "GOAL_INVALID_OBJECTIVE",
        "GOAL_INVALID_TRANSITION",
        "GOAL_FORBIDDEN_IN_ROUND",
        "GOAL_BLOCK_TOO_EARLY",
        "GOAL_RESUME_PAUSED",
        "GOAL_NOT_SUPPORTED",
    ];
    let code = known
        .iter()
        .copied()
        .find(|code| error.message.starts_with(&format!("{code}: ")))
        .unwrap_or("GOAL");
    ToolError::new(code, error.message)
}

/// 本轮提示的轮次身份：`user_initiated` 与 `goal_round` 在 `ToolCtx` 上同源，
/// 三个取值互斥。
fn turn_kind<'a>(ctx: &'a ToolCtx) -> crate::goal::TurnKind<'a> {
    if let Some(meta) = &ctx.goal_round {
        crate::goal::TurnKind::GoalRound(meta)
    } else if ctx.user_initiated {
        crate::goal::TurnKind::Human
    } else {
        crate::goal::TurnKind::Agent
    }
}

fn render(goal: Option<&GoalView>) -> String {
    let Some(goal) = goal else {
        return "No goal is currently set.".to_owned();
    };
    let mut text = format!(
        "Goal: {}\nStatus: {}",
        goal.objective,
        match goal.phase {
            moc_protocol::methods::GoalPhase::Active => "active",
            moc_protocol::methods::GoalPhase::Paused => "paused",
            moc_protocol::methods::GoalPhase::Blocked => "blocked",
            moc_protocol::methods::GoalPhase::Complete => "complete",
        }
    );
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(&format!("\nBlocker: {}: {}", reason.code, reason.message));
    }
    text.push_str(&format!(
        "\nRounds: {}\nRevision: {}\nActivation: {}",
        goal.rounds_started,
        goal.revision,
        if goal.armed { "armed" } else { "disarmed" }
    ));
    text
}

/// `get_goal`：读当前目标。
pub struct GetGoalTool;

impl Tool for GetGoalTool {
    fn name(&self) -> &'static str {
        "get_goal"
    }

    fn description(&self) -> &'static str {
        "Read the current session goal: objective, phase, admitted rounds, and CAS revision for update_goal."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef::new("get_goal", schema(serde_json::json!({}), &[]))
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        _args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let goal = crate::goal::apply_action(
                &ctx.core,
                &ctx.request,
                &ctx.session_id,
                moc_protocol::methods::GoalAction::Show,
            )
            .await
            .map_err(goal_error)?;
            Ok(ToolOutcome::success(render(goal.as_ref())))
        })
    }
}

/// `create_goal`：从人类直发轮的请求推断并创建目标。
pub struct CreateGoalTool;

impl Tool for CreateGoalTool {
    fn name(&self) -> &'static str {
        "create_goal"
    }

    fn description(&self) -> &'static str {
        "Create a persisted goal for a long-running objective: the session automatically continues new rounds until the goal is completed, blocked, paused, or cleared. Only valid in a direct user turn."
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "create_goal",
            schema(
                serde_json::json!({
                    "objective": {
                        "type": "string",
                        "description": "The concrete completion objective the session should keep working toward."
                    }
                }),
                &["objective"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                objective: String,
            }
            let args: Args = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if !ctx.user_initiated {
                return Err(ToolError::new(
                    "GOAL_FORBIDDEN_IN_ROUND",
                    "create_goal is only allowed in a direct user turn",
                ));
            }
            let goal = crate::goal::apply_tool_create(
                &ctx.core,
                &ctx.request,
                &ctx.session_id,
                &args.objective,
            )
            .await
            .map_err(goal_error)?;
            Ok(ToolOutcome::success(render(goal.as_ref())))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    goal_id: String,
    revision: u32,
    action: String,
    objective: Option<String>,
    blocked_reason: Option<String>,
}

/// `update_goal`：按 CAS 迁移目标相位。
pub struct UpdateGoalTool;

impl Tool for UpdateGoalTool {
    fn name(&self) -> &'static str {
        "update_goal"
    }

    fn description(&self) -> &'static str {
        "Update the current goal by exact id+revision from get_goal. action=complete only with verified evidence for every deliverable; action=blocked only when the goal is truly unreachable (in a goal round it requires several sustained rounds first); action=edit/pause/resume only in direct user turns."
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "update_goal",
            schema(
                serde_json::json!({
                    "goal_id": {
                        "type": "string",
                        "description": "Exact goal id returned by get_goal."
                    },
                    "revision": {
                        "type": "integer",
                        "description": "Exact revision returned by get_goal; rejected when the goal changed meanwhile."
                    },
                    "action": {
                        "type": "string",
                        "enum": ["edit", "pause", "resume", "complete", "blocked"],
                        "description": "Lifecycle transition to apply."
                    },
                    "objective": {
                        "type": "string",
                        "description": "Replacement objective; required for action=edit."
                    },
                    "blocked_reason": {
                        "type": "string",
                        "description": "Why the goal is unreachable; required for action=blocked."
                    }
                }),
                &["goal_id", "revision", "action"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: UpdateArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let in_goal_round = ctx.goal_round.is_some();
            let update = match args.action.as_str() {
                "edit" => crate::goal::ToolUpdate::Edit {
                    objective: args.objective.unwrap_or_default(),
                },
                "pause" => crate::goal::ToolUpdate::Pause,
                "resume" => crate::goal::ToolUpdate::Resume,
                "complete" => crate::goal::ToolUpdate::Complete,
                "blocked" => {
                    let reason = args.blocked_reason.unwrap_or_default();
                    if reason.trim().is_empty() {
                        return Err(ToolError::new(
                            "E_BAD_PAYLOAD",
                            "blocked_reason is required for action=blocked",
                        ));
                    }
                    crate::goal::ToolUpdate::Blocked {
                        reason: GoalBlockReason {
                            code: crate::goal::BLOCK_CODE_MODEL.to_owned(),
                            message: reason,
                        },
                    }
                }
                other => {
                    return Err(ToolError::new(
                        "E_BAD_PAYLOAD",
                        format!(
                            "unknown action `{other}`; expected edit|pause|resume|complete|blocked"
                        ),
                    ));
                }
            };
            let goal = crate::goal::apply_tool_update(
                &ctx.core,
                &ctx.request,
                &ctx.session_id,
                &args.goal_id,
                args.revision,
                update,
                turn_kind(ctx),
            )
            .await
            .map_err(goal_error)?;
            // 自主续跑轮里确认完成即收尾：模型把最终交付写完后本轮提示结束。
            Ok(
                if in_goal_round
                    && args.action == "complete"
                    && goal.as_ref().is_some_and(|goal| {
                        goal.phase == moc_protocol::methods::GoalPhase::Complete
                    })
                {
                    ToolOutcome::end_prompt(render(goal.as_ref()))
                } else {
                    ToolOutcome::success(render(goal.as_ref()))
                },
            )
        })
    }
}
