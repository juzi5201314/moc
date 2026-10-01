//! 仅允许编辑已经展示的锚点范围；拒绝时直接服务当前文件。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde_json::Value;

use super::{Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, resolve_tool_path, schema};
use crate::hashline::apply::{EditError, EditRequest, apply_edit, preview_edit};
use crate::resources::ResourceRouter;
use moc_protocol::ToolDiff;

/// 授权预览：按工具参数计算 edit 将产生的 diff，不落盘、不更新锚点状态。
/// 参数不合法或范围未服务时返回 `None`，真实错误由执行阶段给出。
pub(crate) fn preview_edit_diff(ctx: &ToolCtx, arguments: &serde_json::Value) -> Option<ToolDiff> {
    let mut request: EditRequest = serde_json::from_value(arguments.clone()).ok()?;
    reject_resource_path(&request.path).ok()?;
    let source_path = resolve_tool_path(&ctx.cwd, &request.path).ok()?;
    request.path = source_path.to_string_lossy().into_owned();
    let target_path = request
        .move_lines
        .as_ref()
        .map(|movement| {
            reject_resource_path(&movement.target_path)?;
            resolve_tool_path(&ctx.cwd, &movement.target_path)
        })
        .transpose()
        .ok()?;
    let source_served = ctx.anchors.served(&source_path).unwrap_or_default();
    let target_served = target_path
        .as_ref()
        .and_then(|path| ctx.anchors.served(path));
    preview_edit(
        &ctx.cwd,
        &request,
        &source_served,
        target_served.as_ref(),
        &ctx.anchors,
        ctx.config.tools.edit.max_edits,
        ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
    )
    .ok()
}

fn reject_resource_path(path: &str) -> Result<(), ToolError> {
    ResourceRouter::write(path).map_err(|error| ToolError::new(error.code, error.message))
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "使用 read 返回的三字符锚点精准修改文件。每项 edits 为 [from_anchor, to_anchor, replacement]；同锚点且非空 replacement 表示在锚点行之后插入，空 replacement 删除。move 在同一调用中把原始快照锚点行跨文件搬运。"
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "edit",
            schema(
                serde_json::json!({
                    "path": {"type":"string"},
                    "edits": {
                        "type":"array", "minItems":1,
                        "items":{"type":"array","minItems":3,"maxItems":3,"items":{"type":"string"}}
                    },
                    "move": {
                        "type":"object",
                        "properties": {
                            "from":{"type":"string"},
                            "to":{"type":"string"},
                            "target_path":{"type":"string"},
                            "target_anchor":{"type":"string"}
                        },
                        "required":["from","to","target_path"],
                        "additionalProperties":false
                    }
                }),
                &["path", "edits"],
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
            let mut request: EditRequest = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            reject_resource_path(&request.path)?;
            let source_path = resolve_tool_path(&ctx.cwd, &request.path)?;
            request.path = source_path.to_string_lossy().into_owned();
            let target_path = request
                .move_lines
                .as_ref()
                .map(|movement| {
                    reject_resource_path(&movement.target_path)?;
                    resolve_tool_path(&ctx.cwd, &movement.target_path)
                })
                .transpose()?;
            let mut source_served = ctx.anchors.served(&source_path).unwrap_or_default();
            let mut target_served = target_path
                .as_ref()
                .and_then(|path| ctx.anchors.served(path));
            let result = apply_edit(
                &ctx.cwd,
                &request,
                &mut source_served,
                target_served.as_mut(),
                &ctx.anchors,
                ctx.config.tools.edit.max_edits,
                ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
            )
            .await;
            match result {
                Ok(output) => {
                    let count = ctx
                        .anchors
                        .observe_edit(payload, output.content != "No changes made.");
                    if count >= ctx.config.tools.edit.noop_loop_threshold {
                        return Err(ToolError::new(
                            "E_NOOP_LOOP",
                            "同一编辑连续未产生变化，请重新检查目标内容",
                        ));
                    }
                    Ok(ToolOutcome {
                        content: output.content.into(),
                        is_error: false,
                        images: Vec::new(),
                        control: ToolControl::Continue,
                        status: None,
                        diff: output.diff,
                    })
                }
                Err(EditError::Tool(mut content)) => {
                    if ["E_UNSERVED_RANGE", "E_STALE_RANGE", "E_STALE_ANCHOR"]
                        .iter()
                        .any(|code| content.contains(code))
                    {
                        for path in std::iter::once(&source_path).chain(target_path.iter()) {
                            let fresh = super::read::ReadTool
                                .run(ctx, serde_json::json!({"path":path}))
                                .await?;
                            let super::ToolOutput::Text(text) = fresh.content else {
                                unreachable!("read 返回文本分页结果");
                            };
                            content.push_str(&format!("\n当前内容的新锚点：\n{text}"));
                        }
                    }
                    Ok(ToolOutcome {
                        content: content.into(),
                        is_error: true,
                        images: Vec::new(),
                        control: ToolControl::Continue,
                        status: None,
                        diff: None,
                    })
                }
                Err(error) => Err(ToolError::new("E_ACCESS", error.to_string())),
            }
        })
    }
}
