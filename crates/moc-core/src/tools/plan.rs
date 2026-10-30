//! 计划工具：`plan` 保存或修订计划草稿，`exit_plan` 提交工作草稿并结束本轮。
//!
//! `plan` 的两种用法：`markdown` 全量写草稿（起草期复用同一个 `plan_id`，反复
//! 写是原地覆盖）；`edits` 按 hashline 锚点增量修订——锚点来自 `read` 的展示行：
//! 有工作草稿时原地修订，没有草稿时以最近一版已评审计划为底播种新草稿。提交过
//! 的版本始终冻结，批注按行锚定它们。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::{PlanInfo, PlanStatus, SessionUpdate, ToolDiff};
use serde::Deserialize;
use serde_json::Value;

use super::edit::{edits_items_schema, error_outcome, normalize_edit_item};
use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::hashline::apply::{EditError, EditRequest, apply_edit, encode_file, plan_edit};
use crate::session::entries::EntryPayload;

/// 单个计划文件的字节上限。
const MAX_PLAN_BYTES: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArgs {
    title: Option<String>,
    markdown: Option<String>,
    edits: Option<Vec<Value>>,
}

pub struct PlanTool;

impl Tool for PlanTool {
    fn name(&self) -> &'static str {
        "plan"
    }

    fn description(&self) -> &'static str {
        "Save or refine the session plan draft. markdown writes or overwrites the working draft (title required on the first save). edits patches the plan with anchored line edits copied from read of the plan file — on the working draft in place, or on the last reviewed plan to produce the next draft. Call exit_plan to submit the draft for review."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "plan",
            schema(
                serde_json::json!({
                    "title": {"type":"string","description":"Plan title; required for the first draft, afterwards kept or overridden."},
                    "markdown": {"type":"string","description":"Full plan text; writes or overwrites the working draft. Mutually exclusive with edits."},
                    "edits": {
                        "type":"array","minItems":1,
                        "description":"Anchored edits like the edit tool's, applied together; patches the working draft in place, or turns the latest reviewed plan into the next draft. Mutually exclusive with markdown.",
                        "items": edits_items_schema()
                    },
                }),
                &[],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let payload = args.to_string();
            let args: PlanArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let title = normalized_title(args.title);
            match (args.markdown, args.edits) {
                (Some(markdown), None) => write_draft(ctx, title, markdown).await,
                (None, Some(edits)) => apply_edits(ctx, title, edits, payload).await,
                (None, None) => Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "plan takes markdown (write the draft) or edits (anchored patch); read the plan file for anchors",
                )),
                (Some(_), Some(_)) => Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "pass markdown or edits, not both",
                )),
            }
        })
    }
}

fn normalized_title(title: Option<String>) -> Option<String> {
    title
        .map(|title| title.trim().to_owned())
        .filter(|title| !title.is_empty())
}

async fn write_draft(
    ctx: &ToolCtx,
    title: Option<String>,
    markdown: String,
) -> Result<ToolOutcome, ToolError> {
    if markdown.trim().is_empty() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "plan markdown must not be empty",
        ));
    }
    if markdown.len() > MAX_PLAN_BYTES {
        return Err(ToolError::new("E_PLAN_TOO_LARGE", "plan exceeds 512 KiB"));
    }
    let draft = {
        let state = ctx.state.lock().await;
        state
            .plans
            .iter()
            .rev()
            .find(|plan| plan.status == PlanStatus::Draft)
            .cloned()
    };
    let title = title
        .or_else(|| draft.as_ref().map(|plan| plan.title.clone()))
        .ok_or_else(|| {
            ToolError::new(
                "E_BAD_PAYLOAD",
                "plan title is required for the first draft",
            )
        })?;
    // 起草期复用同一个 plan_id：工作草稿是唯一活动计划，反复写是原地覆盖；
    // 只有经 exit_plan 提交的版本才占审阅版本号。
    let plan_id = draft
        .as_ref()
        .map(|plan| plan.plan_id.clone())
        .unwrap_or_else(crate::ids::plan_id);
    let path = ctx.paths.plans().join(format!("{plan_id}.md"));
    tokio::fs::create_dir_all(ctx.paths.plans())
        .await
        .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
    tokio::fs::write(&path, &markdown)
        .await
        .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
    let path_text = format!("local://{plan_id}.md");
    let plan = PlanInfo {
        plan_id: plan_id.clone(),
        title,
        path: path_text.clone(),
        markdown,
        status: PlanStatus::Draft,
        version: 0,
        previous_plan_id: draft.and_then(|plan| plan.previous_plan_id),
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
        "Plan {plan_id} saved at {path_text}. Read it to refine with edits, or call exit_plan to submit it for review."
    )))
}

/// `plan(edits)` 的锚定目标：工作草稿原地修订，或以最近一版已评审计划为底播种。
enum EditTarget {
    /// 有工作草稿：锚点落在草稿文件上，原地改。
    Patch(PlanInfo),
    /// 无草稿：锚点落在被评审版本上，改出的结果另存为新草稿，底版保持冻结。
    Seed(PlanInfo),
}

async fn apply_edits(
    ctx: &ToolCtx,
    title: Option<String>,
    edits: Vec<Value>,
    payload: String,
) -> Result<ToolOutcome, ToolError> {
    if edits.is_empty() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "edits must contain at least one item.",
        ));
    }
    let mut normalized = Vec::with_capacity(edits.len());
    for (index, item) in edits.into_iter().enumerate() {
        normalized.push(normalize_edit_item(index, item)?);
    }
    let target = {
        let state = ctx.state.lock().await;
        let plans = &state.plans;
        if let Some(draft) = plans
            .iter()
            .rev()
            .find(|plan| plan.status == PlanStatus::Draft)
        {
            EditTarget::Patch(draft.clone())
        } else {
            match plans
                .iter()
                .rev()
                .find(|plan| plan.status != PlanStatus::Draft)
            {
                Some(plan) if plan.status == PlanStatus::Submitted => {
                    return Err(ToolError::new(
                        "E_PLAN_UNDER_REVIEW",
                        format!(
                            "plan {} is under review; wait for the review result or start a new draft with markdown",
                            plan.plan_id
                        ),
                    ));
                }
                Some(plan) => EditTarget::Seed(plan.clone()),
                None => {
                    return Err(ToolError::new(
                        "E_PLAN_NOT_FOUND",
                        "no saved plan to edit; write one with markdown first",
                    ));
                }
            }
        }
    };
    let base = match &target {
        EditTarget::Patch(plan) | EditTarget::Seed(plan) => plan,
    };
    let base_path = std::fs::canonicalize(ctx.paths.plans().join(format!("{}.md", base.plan_id)))
        .map_err(|error| ToolError::new("E_PLAN_READ", error.to_string()))?;
    let served = ctx.anchors.served(&base_path).unwrap_or_default();
    let request = EditRequest {
        path: base_path.to_string_lossy().into_owned(),
        edits: normalized,
        move_lines: None,
    };
    let max_edits = ctx.config.tools.edit.max_edits;
    let max_bytes = ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024);
    match target {
        EditTarget::Patch(draft) => {
            let output = match apply_edit(
                &ctx.cwd,
                &request,
                &served,
                None,
                &ctx.anchors,
                max_edits,
                max_bytes,
            )
            .await
            {
                Ok(output) => output,
                Err(EditError::Tool(content)) => {
                    return Ok(error_outcome(ctx, &request, &base_path, None, content));
                }
                Err(error) => return Err(ToolError::new("E_PLAN_WRITE", error.to_string())),
            };
            let changed = output.diff.is_some();
            observe_noop(ctx, payload, changed)?;
            if !changed {
                return Ok(ToolOutcome::success(output.content));
            }
            let markdown = tokio::fs::read_to_string(&base_path)
                .await
                .map_err(|error| ToolError::new("E_PLAN_READ", error.to_string()))?;
            let mut plan = draft;
            plan.markdown = markdown;
            if let Some(title) = title {
                plan.title = title;
            }
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::plan(&plan, PlanStatus::Draft),
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            let mut outcome = ToolOutcome::success(format!(
                "{}\n\nPlan draft {} saved at {}.",
                output.content, plan.plan_id, plan.path
            ));
            outcome.diff = redirect_diff_path(output.diff, &plan.path);
            Ok(outcome)
        }
        EditTarget::Seed(base) => {
            let edit_plan = match plan_edit(&ctx.cwd, &request, &served, None, max_edits, max_bytes)
            {
                Ok(edit_plan) => edit_plan,
                Err(EditError::Tool(content)) => {
                    return Ok(error_outcome(ctx, &request, &base_path, None, content));
                }
                Err(error) => return Err(ToolError::new("E_PLAN_READ", error.to_string())),
            };
            observe_noop(ctx, payload, !edit_plan.no_changes)?;
            if edit_plan.no_changes {
                return Ok(ToolOutcome::success("No changes made."));
            }
            let plan_id = crate::ids::plan_id();
            let path = ctx.paths.plans().join(format!("{plan_id}.md"));
            let bytes = encode_file(&edit_plan.snapshot, &edit_plan.lines);
            tokio::fs::create_dir_all(ctx.paths.plans())
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            tokio::fs::write(&path, &bytes)
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            let path_text = format!("local://{plan_id}.md");
            let plan = PlanInfo {
                plan_id: plan_id.clone(),
                title: title.unwrap_or(base.title),
                path: path_text.clone(),
                markdown: String::from_utf8_lossy(&bytes).into_owned(),
                status: PlanStatus::Draft,
                version: 0,
                // 新草稿是底版的直接后继：提交时 plan_diff 对照的就是这个被评审版本。
                previous_plan_id: Some(base.plan_id),
                annotations: Vec::new(),
            };
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::plan(&plan, PlanStatus::Draft),
                )
                .await
                .map_err(|error| ToolError::new("E_PLAN_WRITE", error.to_string()))?;
            let mut outcome = ToolOutcome::success(format!(
                "{}\n\nSaved as plan draft {plan_id} at {path_text}; read it for anchors before further edits, then call exit_plan to submit.",
                edit_plan.output
            ));
            outcome.diff = redirect_diff_path(Some(edit_plan.diff), &path_text);
            Ok(outcome)
        }
    }
}

/// 与 `edit` 一致的复读防护：同一批锚点反复打不出变化时让模型停下重读。
fn observe_noop(ctx: &ToolCtx, payload: String, changed: bool) -> Result<(), ToolError> {
    let count = ctx.anchors.observe_edit(payload, changed);
    if count >= ctx.config.tools.edit.noop_loop_threshold {
        return Err(ToolError::new(
            "E_NOOP_LOOP",
            "The same edit produced no change repeatedly; re-check the target content",
        ));
    }
    Ok(())
}

/// 计划 diff 的文件路径统一显示为 `local://` URI，与计划身份的呈现一致。
fn redirect_diff_path(diff: Option<ToolDiff>, uri: &str) -> Option<ToolDiff> {
    diff.map(|mut diff| {
        for file in &mut diff.files {
            file.path = uri.to_owned();
        }
        diff
    })
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
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ExitPlanArgs {}
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
