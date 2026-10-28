//! 计划工具：write_plan 保存计划版本，exit_plan 提交最后一份并结束本轮。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{PlanInfo, PlanStatus, SessionUpdate};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::session::entries::EntryPayload;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArgs {
    title: String,
    markdown: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitPlanArgs {}

pub struct WritePlanTool;

impl Tool for WritePlanTool {
    fn name(&self) -> &'static str {
        "write_plan"
    }

    fn description(&self) -> &'static str {
        "Save the plan you are working on. Call it as often as needed while drafting or revising; the last saved plan is the one exit_plan submits."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "write_plan",
            schema(
                serde_json::json!({"title":{"type":"string"},"markdown":{"type":"string"}}),
                &["title", "markdown"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: PlanArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if args.title.trim().is_empty() || args.markdown.trim().is_empty() {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "plan title and markdown must not be empty",
                ));
            }
            if args.markdown.len() > 512 * 1024 {
                return Err(ToolError::new("E_PLAN_TOO_LARGE", "plan exceeds 512 KiB"));
            }
            let plan_id = crate::ids::plan_id();
            let path = ctx.paths.plans().join(format!("{plan_id}.md"));
            tokio::fs::create_dir_all(ctx.paths.plans())
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            tokio::fs::write(&path, &args.markdown)
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            let path_text = format!("local://{plan_id}.md");
            // 每次调用都是一个新版本，但在 exit_plan 之前只是草稿：审阅列表只认
            // 通过 exit_plan 提交过的那一版。
            let plan = PlanInfo {
                plan_id: plan_id.clone(),
                title: args.title.trim().to_owned(),
                path: path_text.clone(),
                markdown: args.markdown,
                status: PlanStatus::Draft,
                version: 0,
                previous_plan_id: None,
                annotations: Vec::new(),
            };
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::plan(&plan, PlanStatus::Draft),
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            Ok(ToolOutcome::success(format!(
                "Plan {plan_id} saved at {path_text}. Call exit_plan to submit it for review."
            )))
        })
    }
}

pub struct ExitPlanTool;

impl Tool for ExitPlanTool {
    fn name(&self) -> &'static str {
        "exit_plan"
    }

    fn description(&self) -> &'static str {
        "Submit the last saved plan for user review and end this prompt. No tool call runs after this one."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new("exit_plan", schema(serde_json::json!({}), &[]))
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let _: ExitPlanArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let (plan, previous, version) = {
                let state = ctx.state.lock().await;
                let plans = &state.plans;
                let plan = plans
                    .iter()
                    .rev()
                    .find(|plan| plan.status == PlanStatus::Draft)
                    // 重复调用 exit_plan 时把刚提交的那一版再发一次，行为幂等。
                    .or_else(|| {
                        plans
                            .iter()
                            .rev()
                            .find(|plan| plan.status == PlanStatus::Submitted)
                    })
                    .cloned()
                    .ok_or_else(|| {
                        ToolError::new("E_PLAN_NOT_FOUND", "no saved plan is available")
                    })?;
                let is_draft = plan.status == PlanStatus::Draft;
                let previous = if is_draft {
                    plans
                        .iter()
                        .rev()
                        .find(|other| {
                            other.status == PlanStatus::Revised && other.plan_id != plan.plan_id
                        })
                        .cloned()
                } else {
                    None
                };
                let version = if is_draft {
                    // 审阅版本号只数已提交过的版本，起草中被丢弃的计划不占号。
                    let reviewed = plans
                        .iter()
                        .filter(|other| other.status != PlanStatus::Draft)
                        .count();
                    u32::try_from(reviewed)
                        .unwrap_or(u32::MAX)
                        .saturating_add(1)
                } else {
                    plan.version
                };
                (plan, previous, version)
            };
            let mut submitted = plan;
            submitted.status = PlanStatus::Submitted;
            submitted.version = version;
            if submitted.previous_plan_id.is_none() {
                submitted.previous_plan_id =
                    previous.as_ref().map(|previous| previous.plan_id.clone());
            }
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::plan(&submitted, PlanStatus::Submitted),
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            if let Some(mut previous) = previous {
                previous.status = PlanStatus::Superseded;
                ctx.sessions
                    .append(
                        &ctx.session_id,
                        EntryPayload::plan(&previous, PlanStatus::Superseded),
                    )
                    .await
                    .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
                ctx.request
                    .updates()
                    .emit(
                        ctx.session_id.clone(),
                        SessionUpdate::PlanUpdated { plan: previous },
                    )
                    .await
                    .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
            }
            let summary = format!(
                "Plan {} (v{}) is ready for review at {}.",
                submitted.plan_id, submitted.version, submitted.path
            );
            ctx.request
                .updates()
                .emit(
                    ctx.session_id.clone(),
                    SessionUpdate::PlanSubmitted {
                        plan: submitted.clone(),
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
            Ok(ToolOutcome::end_prompt(summary))
        })
    }
}
