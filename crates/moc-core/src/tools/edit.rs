//! edit 工具：只能编辑已经展示过锚点的范围；锚点出错时在错误里附上当前文件的带锚点内容。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde_json::Value;

use super::{Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, resolve_tool_path};
use crate::hashline::apply::{EditError, EditOutput, EditRequest, apply_edit};
use crate::hashline::{anchor_line_token, parse_anchor};
use crate::resources::ResourceRouter;

/// 这些错误会在结果里附上当前文件的带锚点内容；其它错误原样返回。
const ANCHOR_ERRORS: [&str; 6] = [
    "E_UNSERVED_RANGE",
    "E_STALE_ANCHOR",
    "E_STALE_RANGE",
    "E_BAD_ANCHOR",
    "E_BAD_PAYLOAD",
    "E_OVERLAP",
];
/// 附带内容最多显示的行数，避免错误输出绕过 read 的分页预算。
const FEEDBACK_MAX_LINES: usize = 200;

fn reject_resource_path(path: &str) -> Result<(), ToolError> {
    ResourceRouter::write(path).map_err(|error| ToolError::new(error.code, error.message))
}

/// 把各种兼容写法的参数统一成单文件编辑请求；授权预览和执行都用它。
pub(crate) fn parse_requests(args: Value) -> Result<EditRequest, ToolError> {
    let (path, edits, move_lines) = normalize_args(args)?;
    Ok(EditRequest {
        path,
        edits,
        move_lines,
    })
}

type NormalizedArgs = (
    String,
    Vec<[String; 3]>,
    Option<crate::hashline::apply::MoveRequest>,
);

fn normalize_args(args: Value) -> Result<NormalizedArgs, ToolError> {
    let Value::Object(mut object) = args else {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "edit arguments must be an object",
        ));
    };
    if object.contains_key("files") {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "unexpected field \"files\" (this tool takes one file per call).",
        ));
    }
    let path = object
        .remove("path")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "path must be a string"))?;
    let edits_value = object
        .remove("edits")
        .ok_or_else(|| ToolError::new("E_BAD_PAYLOAD", "edits must be a non-empty array"))?;
    let Value::Array(items) = edits_value else {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "edits must be a non-empty array",
        ));
    };
    if items.is_empty() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "edits must contain at least one item.",
        ));
    }
    let mut edits = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        edits.push(normalize_edit_item(index, item)?);
    }
    let move_lines = object
        .remove("move")
        .map(|value| {
            serde_json::from_value(value)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))
        })
        .transpose()?;
    if let Some((key, _)) = object.into_iter().next() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!("unexpected field \"{key}\"."),
        ));
    }
    Ok((path, edits, move_lines))
}

fn normalize_edit_item(index: usize, item: Value) -> Result<[String; 3], ToolError> {
    match item {
        Value::Array(values) if values.len() == 2 || values.len() == 3 => {
            let strings: Result<Vec<_>, _> = values
                .iter()
                .map(|value| value.as_str().map(str::to_owned).ok_or(()))
                .collect();
            let strings = strings.map_err(|_| {
                ToolError::new(
                    "E_BAD_PAYLOAD",
                    format!("edits[{index}] must contain string items"),
                )
            })?;
            if strings.len() == 2 {
                Ok([strings[0].clone(), strings[0].clone(), strings[1].clone()])
            } else {
                Ok([strings[0].clone(), strings[1].clone(), strings[2].clone()])
            }
        }
        Value::Object(mut object) => {
            let from = take_string(&mut object, "from", index)?;
            let to = take_string(&mut object, "to", index)?;
            let replace = take_string(&mut object, "replace", index)?;
            if let Some((key, _)) = object.into_iter().next() {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    format!("edits[{index}] has unexpected field \"{key}\""),
                ));
            }
            Ok([from, to, replace])
        }
        Value::Array(values) => Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!(
                "edits[{index}] received {} items; expected 2 or 3",
                values.len()
            ),
        )),
        Value::String(_) => Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!("edits[{index}] received a string"),
        )),
        _ => Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!("edits[{index}] received an unsupported shape"),
        )),
    }
}

fn take_string(
    object: &mut serde_json::Map<String, Value>,
    name: &str,
    index: usize,
) -> Result<String, ToolError> {
    object
        .remove(name)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| {
            ToolError::new(
                "E_BAD_PAYLOAD",
                format!("edits[{index}].{name} must be a string"),
            )
        })
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit files with line-number plus two-letter anchors from read. Each edit is [from_anchor, to_anchor, replacement] replacing that inclusive range; identical anchors with non-empty replacement insert after that line; empty replacement deletes."
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new("edit", edit_schema())
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let payload = args.to_string();
            let request = parse_requests(args)?;
            let (content, diff, changed) = match apply_one(ctx, request).await? {
                Ok(output) => {
                    let changed = output.content != "No changes made.";
                    (output.content, output.diff, changed)
                }
                Err(outcome) => return Ok(outcome),
            };
            let count = ctx.anchors.observe_edit(payload, changed);
            if count >= ctx.config.tools.edit.noop_loop_threshold {
                return Err(ToolError::new(
                    "E_NOOP_LOOP",
                    "The same edit produced no change repeatedly; re-check the target content",
                ));
            }
            Ok(ToolOutcome {
                content: content.into(),
                is_error: false,
                structured_content: None,
                error_code: None,
                images: Vec::new(),
                control: ToolControl::Continue,
                status: None,
                diff,
            })
        })
    }
}

/// 授权预览：用与执行相同的计划算出 edit 会产生的结构化 diff。
///
/// 任何一个文件无法预览（如 E_UNSERVED_RANGE）就整体放弃：宁可没有预览，也不给一份
/// 和实际执行对不上的 diff。
pub(crate) fn preview_arguments(ctx: &ToolCtx, args: &Value) -> Option<moc_protocol::ToolDiff> {
    let request = parse_requests(args.clone()).ok()?;
    reject_resource_path(&request.path).ok()?;
    let source_path = resolve_tool_path(&ctx.cwd, &request.path).ok()?;
    let target_path = request
        .move_lines
        .as_ref()
        .map(|movement| resolve_tool_path(&ctx.cwd, &movement.target_path))
        .transpose()
        .ok()?;
    let served = ctx.anchors.served(&source_path).unwrap_or_default();
    let target_served = target_path
        .as_ref()
        .and_then(|path| ctx.anchors.served(path));
    let mut resolved = request;
    resolved.path = source_path.to_string_lossy().into_owned();
    crate::hashline::apply::preview_edit(
        &ctx.cwd,
        &resolved,
        &served,
        target_served.as_ref(),
        ctx.config.tools.edit.max_edits,
        ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
    )
    .ok()
}

fn edit_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type":"string","description":"File to edit; the same path you read."},
            "edits": {
                "type":"array","minItems":1,
                "description":"Ordered edits; applied together, and the whole call is rejected if any one fails.",
                "items": {
                    "anyOf": [
                        {"type":"array","minItems":3,"maxItems":3,"items":{"type":"string"},"description":"[from, to, replace] with inclusive anchors."},
                        {"type":"array","minItems":2,"maxItems":2,"items":{"type":"string"},"description":"[anchor, text] inserts text after that line."},
                        {"type":"object","properties":{"from":{"type":"string"},"to":{"type":"string"},"replace":{"type":"string"}},"required":["from","to","replace"],"additionalProperties":false,"description":"Object form of [from, to, replace]."}
                    ]
                }
            },
            "move": {
                "type":"object","additionalProperties":false,
                "properties": {
                    "from": {"type":"string","description":"First line of the block to move, inclusive, as a bare anchor copied verbatim from read output (line number + two-letter id, e.g. \"42sr\")."},
                    "to": {"type":"string","description":"Last line of the block to move, inclusive, same anchor form as `from`."},
                    "target_path": {"type":"string","description":"Destination file; it must have been read in this session."},
                    "target_anchor": {"type":"string","description":"Insert the moved block AFTER this anchor in the destination; omit to append at the end of the destination."}
                },
                "required":["from","to","target_path"]
            }
        },
        "required":["path","edits"],
        "additionalProperties": false
    })
}

async fn apply_one(
    ctx: &ToolCtx,
    mut request: EditRequest,
) -> Result<Result<EditOutput, ToolOutcome>, ToolError> {
    reject_resource_path(&request.path)?;
    let source_path = resolve_tool_path(&ctx.cwd, &request.path)?;
    // 绕过授权入口的调用也要检查，判断方式与授权阶段相同。
    crate::tools::ensure_workspace_write(&ctx.cwd, ctx.permission, &source_path)?;
    request.path = source_path.to_string_lossy().into_owned();
    let target_path = request
        .move_lines
        .as_ref()
        .map(|movement| {
            reject_resource_path(&movement.target_path)?;
            let path = resolve_tool_path(&ctx.cwd, &movement.target_path)?;
            crate::tools::ensure_workspace_write(&ctx.cwd, ctx.permission, &path)?;
            Ok(path)
        })
        .transpose()?;
    let source_served = ctx.anchors.served(&source_path).unwrap_or_default();
    let target_served = target_path
        .as_ref()
        .and_then(|path| ctx.anchors.served(path));
    match apply_edit(
        &ctx.cwd,
        &request,
        &source_served,
        target_served.as_ref(),
        &ctx.anchors,
        ctx.config.tools.edit.max_edits,
        ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
    )
    .await
    {
        Ok(output) => Ok(Ok(output)),
        Err(EditError::Tool(mut content)) => {
            let error_code = content
                .split_once(']')
                .and_then(|(prefix, _)| prefix.strip_prefix('['))
                .filter(|code| code.starts_with("E_"))
                .unwrap_or("E_TOOL")
                .to_owned();
            if ANCHOR_ERRORS.contains(&error_code.as_str()) {
                append_anchor_feedback(
                    ctx,
                    &request,
                    &source_path,
                    target_path.as_deref(),
                    &mut content,
                );
            }
            Ok(Err(ToolOutcome {
                content: content.into(),
                is_error: true,
                structured_content: None,
                error_code: Some(error_code),
                images: Vec::new(),
                control: ToolControl::Continue,
                status: None,
                diff: None,
            }))
        }
        Err(error) => Err(ToolError::new("E_ACCESS", error.to_string())),
    }
}

/// 锚点出错后附上当前令牌。
///
/// 只在 epoch 和行数都对得上时刷新，否则保留原错误并提示重新读取。附带的内容限制在出错
/// 锚点附近最多 [`FEEDBACK_MAX_LINES`] 行，避免绕过 read 的分页预算。
fn append_anchor_feedback(
    ctx: &ToolCtx,
    request: &EditRequest,
    source_path: &std::path::Path,
    target_path: Option<&std::path::Path>,
    content: &mut String,
) {
    let mut paths = vec![(source_path, source_hints(request))];
    if let Some(target) = target_path {
        paths.push((target, target_hints(request)));
    }
    let max_bytes = ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024);
    for (path, hints) in paths {
        let shown = ctx
            .anchors
            .served(path)
            .is_some_and(|record| record.entries.iter().any(Option::is_some));
        if !shown {
            content.push_str(&format!(
                "\n\n{} has no shown lines yet; read it before editing.",
                path.display()
            ));
            continue;
        }
        let snapshot = match crate::hashline::served::read_snapshot(path, max_bytes) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                content.push_str(&format!(
                    "\n\nCould not re-read {}; read the file again to get current anchors.",
                    path.display()
                ));
                continue;
            }
        };
        let Some(tokens) = ctx
            .anchors
            .refresh_nearest(path, &snapshot, &hints, FEEDBACK_MAX_LINES)
        else {
            content.push_str(&format!(
                "\n\n{} changed since it was shown; read it again to get current anchors.",
                path.display()
            ));
            continue;
        };
        if tokens.is_empty() {
            content.push_str(&format!(
                "\n\nNo lines of {} are currently editable; read the file again.",
                path.display()
            ));
            continue;
        }
        let mut block = String::new();
        for (position, token) in tokens {
            block.push_str(&format!(
                "  {}\n",
                anchor_line_token(token, &snapshot.lines[position])
            ));
        }
        content.push_str(&format!(
            "\n\nCurrent anchors for {} (nearest to the failed anchors, up to {FEEDBACK_MAX_LINES} lines):\n{block}Use these anchors directly; re-read the file if the line you need is not shown.",
            path.display()
        ));
    }
}

/// 源文件附带内容围绕的行号：编辑锚点和 move 的源锚点。
fn source_hints(request: &EditRequest) -> Vec<usize> {
    let mut hints = Vec::new();
    for [from, to, _] in &request.edits {
        for raw in [from, to] {
            if let Ok(parsed) = parse_anchor(raw) {
                hints.push(parsed.line);
            }
        }
    }
    if let Some(movement) = &request.move_lines {
        for raw in [&movement.from, &movement.to] {
            if let Ok(parsed) = parse_anchor(raw) {
                hints.push(parsed.line);
            }
        }
    }
    hints
}

/// 目标文件附带内容围绕的行号：move 的插入锚点。
fn target_hints(request: &EditRequest) -> Vec<usize> {
    request
        .move_lines
        .as_ref()
        .and_then(|movement| movement.target_anchor.as_deref())
        .and_then(|raw| parse_anchor(raw).ok())
        .map(|parsed| vec![parsed.line])
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_two_item_edits_as_insertions() {
        let request = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [["42sr", "inserted"]],
        }))
        .expect("二元组应统一成三元组");
        assert_eq!(
            request.edits,
            vec![["42sr".to_owned(), "42sr".to_owned(), "inserted".to_owned()]]
        );
    }

    #[test]
    fn rejects_files_field_and_unknown_fields() {
        let error = parse_requests(serde_json::json!({"files": []})).expect_err("应拒绝 files 字段");
        assert!(error.message.contains("takes one file per call"));
        let error = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [["42sr", "42sr", "replacement"]],
            "file": "other.txt",
        }))
        .expect_err("应拒绝未声明的字段");
        assert!(error.message.contains("unexpected field"));
    }

    #[test]
    fn normalizes_object_edit_shape() {
        let request = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [{"from":"1aa", "to":"2bb", "replace":"new"}],
        }))
        .expect("对象写法应统一成三元组");
        assert_eq!(request.edits[0], ["1aa", "2bb", "new"]);
    }

    #[test]
    fn rejects_empty_edits_early() {
        let error = parse_requests(serde_json::json!({"path":"file.txt", "edits":[]}))
            .expect_err("应拒绝空的 edits");
        assert!(error.message.contains("at least one item"));
    }
}
