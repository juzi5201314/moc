//! 持久化提交的计划并结束当前模型轮次。

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

pub struct SubmitPlanTool;

impl Tool for SubmitPlanTool {
    fn name(&self) -> &'static str {
        "submit_plan"
    }
    fn description(&self) -> &'static str {
        "提交计划给用户审阅；提交后本轮立即结束，不要继续执行计划。"
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "submit_plan",
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
                    "计划 title 与 markdown 不能为空",
                ));
            }
            if args.markdown.len() > 512 * 1024 {
                return Err(ToolError::new("E_PLAN_TOO_LARGE", "计划内容超过 512 KiB"));
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
            let path_text = path.to_string_lossy().into_owned();
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::Plan {
                        plan_id: plan_id.clone(),
                        title: title.clone(),
                        path: path_text.clone(),
                        markdown: args.markdown.clone(),
                        status: PlanStatus::Submitted,
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            ctx.request
                .updates()
                .emit(
                    ctx.session_id.clone(),
                    SessionUpdate::PlanSubmitted {
                        plan_id: plan_id.clone(),
                        title,
                        path: path_text,
                        markdown: args.markdown,
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_UPDATE", error.to_string()))?;
            Ok(ToolOutcome::end_turn(format!(
                "计划已提交（{plan_id}）。等待用户批准，不要继续修改任何文件。"
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_schema_contains_title_and_markdown() {
        let schema = SubmitPlanTool.def().parameters;
        assert_eq!(schema["required"], serde_json::json!(["title", "markdown"]));
    }
}
