//! 仅允许编辑已经展示的锚点范围；拒绝时直接服务当前文件。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde_json::Value;

use super::{
    Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, ToolOutput, resolve_tool_path,
};
use crate::hashline::apply::{EditError, EditOutput, EditRequest, apply_edit};
use crate::resources::ResourceRouter;

fn reject_resource_path(path: &str) -> Result<(), ToolError> {
    ResourceRouter::write(path).map_err(|error| ToolError::new(error.code, error.message))
}

/// 将兼容载荷归一为单文件编辑请求；授权预览与执行共用此入口。
pub(crate) fn parse_requests(args: Value) -> Result<Vec<EditRequest>, ToolError> {
    let (path, edits, move_lines) = normalize_args(args)?;
    Ok(vec![EditRequest {
        path,
        edits,
        move_lines,
    }])
}

fn normalize_args(
    args: Value,
) -> Result<
    (
        String,
        Vec<[String; 3]>,
        Option<crate::hashline::apply::MoveRequest>,
    ),
    ToolError,
> {
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
        .or_else(|| object.remove("file"))
        .or_else(|| object.remove("file_path"))
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
        let message = if key == "files" {
            format!("unexpected field \"{key}\" (this tool takes one file per call).")
        } else {
            format!("unexpected field \"{key}\".")
        };
        return Err(ToolError::new("E_BAD_PAYLOAD", message));
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
            let from = take_string(&mut object, &["from", "remove_from"], index)?;
            let to = take_string(&mut object, &["to", "remove_to"], index)?;
            let replace = take_string(&mut object, &["replace", "replacement_text"], index)?;
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
    names: &[&str],
    index: usize,
) -> Result<String, ToolError> {
    names
        .iter()
        .find_map(|name| object.remove(*name))
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| {
            ToolError::new(
                "E_BAD_PAYLOAD",
                format!("edits[{index}] object fields must be strings"),
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
            let requests = parse_requests(args)?;

            let mut report = BatchReport::default();
            for request in requests {
                match apply_one(ctx, request).await? {
                    Ok(output) => report.record(output),
                    Err(outcome) => return Ok(report.finish_failure(outcome)),
                }
            }

            let count = ctx.anchors.observe_edit(payload, report.changed);
            if count >= ctx.config.tools.edit.noop_loop_threshold {
                return Err(ToolError::new(
                    "E_NOOP_LOOP",
                    "The same edit produced no change repeatedly; re-check the target content",
                ));
            }
            Ok(ToolOutcome {
                content: report.content.into(),
                is_error: false,
                structured_content: None,
                error_code: None,
                images: Vec::new(),
                control: ToolControl::Continue,
                status: None,
                diff: (!report.files.is_empty()).then_some(moc_protocol::ToolDiff {
                    files: report.files,
                    truncated: report.truncated,
                }),
            })
        })
    }
}

/// 多文件 edit 的累计输出；中途失败时保留此前已提交文件的摘要与 diff。
#[derive(Default)]
struct BatchReport {
    content: String,
    files: Vec<moc_protocol::ToolFileDiff>,
    truncated: bool,
    changed: bool,
}

impl BatchReport {
    fn record(&mut self, output: EditOutput) {
        if !self.content.is_empty() {
            self.content.push('\n');
        }
        self.changed |= output.content != "No changes made.";
        self.content.push_str(&output.content);
        if let Some(diff) = output.diff {
            self.truncated |= diff.truncated;
            self.files.extend(diff.files);
        }
    }

    /// 失败结果补上已应用的内容与 diff：前端与模型都要知道哪些文件已经改过。
    fn finish_failure(self, mut outcome: ToolOutcome) -> ToolOutcome {
        if let ToolOutput::Text(failed) = outcome.content {
            let mut report = String::new();
            if !self.content.is_empty() {
                report.push_str("Already applied before the failure:\n");
                report.push_str(&self.content);
                report.push_str("\n\n");
            }
            report.push_str(&failed);
            outcome.content = report.into();
        }
        if !self.files.is_empty() {
            outcome.diff = Some(moc_protocol::ToolDiff {
                files: self.files,
                truncated: self.truncated,
            });
        }
        outcome
    }
}

/// 授权预览：用与执行相同的计划计算 edit 将产生的结构化 diff。
///
/// 任一文件无法预览（如 E_UNSERVED_RANGE）时整体放弃：宁可没有预览，也不给
/// 出一份与实际执行无关的 diff。
pub(crate) fn preview_arguments(ctx: &ToolCtx, args: &Value) -> Option<moc_protocol::ToolDiff> {
    let requests = parse_requests(args.clone()).ok()?;
    preview_requests(
        &ctx.cwd,
        requests,
        &ctx.anchors,
        ctx.config.tools.edit.max_edits,
        ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
    )
}

/// 预览主体：与 [`apply_one`] 相同的解析、快照与计划计算，只是不落盘。
fn preview_requests(
    cwd: &std::path::Path,
    requests: Vec<EditRequest>,
    anchors: &crate::hashline::AnchorStore,
    max_edits: usize,
    max_file_bytes: u64,
) -> Option<moc_protocol::ToolDiff> {
    let mut files = Vec::new();
    let mut truncated = false;
    for request in requests {
        reject_resource_path(&request.path).ok()?;
        let source_path = resolve_tool_path(cwd, &request.path).ok()?;
        let target_path = request
            .move_lines
            .as_ref()
            .map(|movement| resolve_tool_path(cwd, &movement.target_path))
            .transpose()
            .ok()?;
        let served = anchors.served(&source_path).unwrap_or_default();
        let target_served = target_path.as_ref().and_then(|path| anchors.served(path));
        let mut resolved = request;
        resolved.path = source_path.to_string_lossy().into_owned();
        let diff = crate::hashline::apply::preview_edit(
            cwd,
            &resolved,
            &served,
            target_served.as_ref(),
            anchors,
            max_edits,
            max_file_bytes,
        )
        .ok()?;
        truncated |= diff.truncated;
        files.extend(diff.files);
    }
    Some(moc_protocol::ToolDiff { files, truncated })
}

fn edit_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type":"string","description":"File to edit; the same path you read."},
            "edits": {
                "type":"array","minItems":1,
                "description":"Ordered edits; applied together, and the whole call is rejected if any one fails.",
                "items": {"type":"array","minItems":3,"maxItems":3,"items":{"type":"string"},"description":"[from, to, replace]. `from` and `to` are inclusive anchors copied verbatim from read output (line number + two-letter id, e.g. \"42sr\"). `replace` is the replacement text with \\n as the line separator. Empty `replace` deletes the inclusive range. Equal `from` and `to` with non-empty `replace` inserts the text AFTER that line."}
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
    // 授权入口之外的调用也要受检；判定与授权阶段同源。
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
    let mut source_served = ctx.anchors.served(&source_path).unwrap_or_default();
    let mut target_served = target_path
        .as_ref()
        .and_then(|path| ctx.anchors.served(path));
    match apply_edit(
        &ctx.cwd,
        &request,
        &mut source_served,
        target_served.as_mut(),
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
            if [
                "E_UNSERVED_RANGE",
                "E_STALE_ANCHOR",
                "E_STALE_RANGE",
                "E_BAD_ANCHOR",
                "E_BAD_PAYLOAD",
                "E_OVERLAP",
                "E_NOOP_LOOP",
            ]
            .contains(&error_code.as_str())
            {
                let snapshot = crate::hashline::served::read_snapshot(
                    &source_path,
                    ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024),
                )
                .map_err(|error| ToolError::new("E_READ", error.to_string()))?;
                if let Some(served) = ctx.anchors.served(&source_path) {
                    let mut block = String::new();
                    for (position, shown) in served.served.iter().copied().enumerate() {
                        if shown {
                            block.push_str(&format!(
                                "  {}\n",
                                crate::hashline::anchor_line(
                                    position + 1,
                                    &snapshot.lines[position]
                                )
                            ));
                        }
                    }
                    content.push_str(&format!("\n\nCurrent anchors for {}:\n{}\nUse the anchors above directly; no re-read is needed unless these lines changed.", source_path.display(), block));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_two_item_edits_as_insertions() {
        let requests = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [["42sr", "inserted"]],
        }))
        .expect("二元组应归一化");
        assert_eq!(
            requests[0].edits,
            vec![["42sr".to_owned(), "42sr".to_owned(), "inserted".to_owned()]]
        );
    }

    #[test]
    fn folds_file_alias_and_rejects_files_field() {
        let request = parse_requests(serde_json::json!({
            "file": "file.txt",
            "edits": [["42sr", "42sr", "replacement"]],
        }))
        .expect("file 别名应折叠");
        assert_eq!(request[0].path, "file.txt");
        let error = parse_requests(serde_json::json!({"files": []})).expect_err("files 应拒绝");
        assert!(error.message.contains("takes one file per call"));
    }

    #[test]
    fn normalizes_object_edit_shape() {
        let requests = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [{"from":"1aa", "to":"2bb", "replace":"new"}],
        }))
        .expect("对象编辑应归一化");
        assert_eq!(requests[0].edits[0], ["1aa", "2bb", "new"]);
    }

    #[test]
    fn rejects_empty_edits_early() {
        let error = parse_requests(serde_json::json!({"path":"file.txt", "edits":[]}))
            .expect_err("空编辑应拒绝");
        assert!(error.message.contains("at least one item"));
    }
}
