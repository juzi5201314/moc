//! 持久化计划并结束当前计划轮次。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{PlanStatus, SessionUpdate};
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
        "Save the completed plan for review. Do not modify workspace files in plan mode."
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
            let title = args.title.trim().to_owned();
            let path = ctx.paths.plans().join(format!("{plan_id}.md"));
            tokio::fs::create_dir_all(ctx.paths.plans())
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            tokio::fs::write(&path, &args.markdown)
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            let path_text = format!("local://{plan_id}.md");
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::Plan {
                        plan_id: plan_id.clone(),
                        title,
                        path: path_text.clone(),
                        markdown: args.markdown,
                        status: PlanStatus::Submitted,
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            Ok(ToolOutcome::success(format!(
                "Plan {plan_id} saved at {path_text}. Call exit_plan with {{}} to end this plan round and request review."
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
        "End the current plan round and request review of the last saved plan."
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
            let plan = ctx
                .state
                .lock()
                .await
                .plans
                .iter()
                .rev()
                .find(|plan| plan.status == PlanStatus::Submitted)
                .cloned()
                .ok_or_else(|| ToolError::new("E_PLAN_NOT_FOUND", "no saved plan is available"))?;
            ctx.request
                .updates()
                .emit(
                    ctx.session_id.clone(),
                    SessionUpdate::PlanSubmitted {
                        plan_id: plan.plan_id.clone(),
                        title: plan.title.clone(),
                        path: plan.path.clone(),
                        markdown: plan.markdown.clone(),
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
            Ok(ToolOutcome::end_prompt(format!(
                "Plan {} is ready for review at {}. Do not continue this plan round.",
                plan.plan_id, plan.path
            )))
        })
    }
}
