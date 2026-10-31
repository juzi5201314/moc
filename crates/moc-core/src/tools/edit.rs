//! edit 工具：按文件 tag + 行号编辑已展示过的行；定位出错时在错误里附上当前行与新 tag。

use std::fmt::Write as _;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde_json::Value;

use super::{Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, resolve_tool_path};
use crate::hashline::apply::{EditError, EditOp, EditOutput, EditRequest, MoveRequest, apply_edit};
use crate::hashline::{numbered_line, parse_tag, tag_text};
use crate::resources::ResourceRouter;

/// 这些错误会在结果里附上当前文件的行与新 tag；其它错误原样返回。
const FEEDBACK_ERRORS: [&str; 6] = [
    "E_STALE_TAG",
    "E_STALE_LINE",
    "E_UNSEEN",
    "E_OVERLAP",
    "E_BAD_PAYLOAD",
    "E_RANGE_TOO_LARGE",
];
/// 附带内容最多显示的行数，避免错误输出绕过 read 的分页预算。
const FEEDBACK_MAX_LINES: usize = 200;

fn reject_resource_path(path: &str) -> Result<(), ToolError> {
    ResourceRouter::write(path).map_err(|error| ToolError::new(error.code, error.message))
}

/// 把参数解析成单文件编辑请求；授权预览和执行都用它。
pub(crate) fn parse_requests(args: Value) -> Result<EditRequest, ToolError> {
    serde_json::from_value(args).map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit a file by line number. Pass the file's tag from the [path#TAG] header of the latest read or edit output, then ops: replace lines from-to with text, insert text after a line (0 = top), or delete lines from-to. Line numbers refer to that tag; untouched lines stay addressable through older tags."
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
/// 任何一个文件无法预览（如 E_STALE_TAG）就整体放弃：宁可没有预览，也不给一份
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

/// `edits` 数组元素的 schema；`edit` 与 `plan` 共用同一组写法。
pub(crate) fn edits_items_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["op"],
        "properties": {
            "op": {"type":"string","enum":["replace","insert","delete"]},
            "from": {"type":"integer","minimum":1,"description":"replace/delete: first line, inclusive."},
            "to": {"type":"integer","minimum":1,"description":"replace/delete: last line, inclusive; equal to from for one line."},
            "after": {"type":"integer","minimum":0,"description":"insert: line to insert after; 0 inserts at the top."},
            "text": {"type":"string","description":"replace/insert: the new lines verbatim, no line-number prefixes."}
        }
    })
}

fn edit_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type":"string","description":"File to edit; the same path you read."},
            "tag": {"type":"string","description":"4-hex tag from the [path#TAG] header of the latest read or edit output of this file; line numbers refer to that version."},
            "edits": {
                "type":"array","minItems":1,
                "description":"Edits applied together; all line numbers refer to `tag`, never shifted by earlier edits in the same call. The whole call is rejected if any one fails.",
                "items": edits_items_schema()
            },
            "move": {
                "type":"object","additionalProperties":false,
                "properties": {
                    "from": {"type":"integer","minimum":1,"description":"First source line to move, inclusive; refers to `tag`."},
                    "to": {"type":"integer","minimum":1,"description":"Last source line to move, inclusive; refers to `tag`."},
                    "target_path": {"type":"string","description":"Destination file; it must have been read in this session."},
                    "target_tag": {"type":"string","description":"4-hex tag from the latest read or edit output of the destination."},
                    "after": {"type":"integer","minimum":0,"description":"Destination line to insert after; 0 inserts at the top; omit to append at the end."}
                },
                "required":["from","to","target_path","target_tag"]
            }
        },
        "required":["path","tag"],
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
    crate::tools::ensure_path_access(
        ctx,
        &source_path.to_string_lossy(),
        crate::sandbox::Access::Write,
    )?;
    request.path = source_path.to_string_lossy().into_owned();
    let target_path = request
        .move_lines
        .as_ref()
        .map(|movement| {
            reject_resource_path(&movement.target_path)?;
            let path = resolve_tool_path(&ctx.cwd, &movement.target_path)?;
            crate::tools::ensure_path_access(
                ctx,
                &path.to_string_lossy(),
                crate::sandbox::Access::Write,
            )?;
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
        Err(EditError::Tool(content)) => Ok(Err(error_outcome(
            ctx,
            &request,
            &source_path,
            target_path.as_deref(),
            content,
        ))),
        Err(error) => Err(ToolError::new("E_ACCESS", error.to_string())),
    }
}

/// `EditError::Tool` 的结果包装：解析错误码；定位类错误附上当前行与新 tag。
///
/// `edit` 与 `plan`（计划文件增量修订）共用，错误回显保持一致。
pub(crate) fn error_outcome(
    ctx: &ToolCtx,
    request: &EditRequest,
    source_path: &std::path::Path,
    target_path: Option<&std::path::Path>,
    mut content: String,
) -> ToolOutcome {
    let error_code = content
        .split_once(']')
        .and_then(|(prefix, _)| prefix.strip_prefix('['))
        .filter(|code| code.starts_with("E_"))
        .unwrap_or("E_TOOL")
        .to_owned();
    if FEEDBACK_ERRORS.contains(&error_code.as_str()) {
        append_current_lines(ctx, request, source_path, target_path, &mut content);
    }
    ToolOutcome {
        content: content.into(),
        is_error: true,
        structured_content: None,
        error_code: Some(error_code),
        images: Vec::new(),
        control: ToolControl::Continue,
        status: None,
        diff: None,
    }
}

/// 定位出错后附上出错位置附近的当前行，并把它们登记为已展示。
///
/// 提示位置优先沿版本链映射到当前坐标；登记后返回的新 tag 可直接用于下一次编辑。
/// 附带的内容最多 [`FEEDBACK_MAX_LINES`] 行，避免绕过 read 的分页预算。
fn append_current_lines(
    ctx: &ToolCtx,
    request: &EditRequest,
    source_path: &std::path::Path,
    target_path: Option<&std::path::Path>,
    content: &mut String,
) {
    let mut files = vec![(source_path, request.tag.as_str(), source_hints(request))];
    if let (Some(target), Some(movement)) = (target_path, &request.move_lines) {
        files.push((target, movement.target_tag.as_str(), target_hints(movement)));
    }
    let max_bytes = ctx.config.tools.output.max_file_bytes.min(4 * 1024 * 1024);
    for (path, tag, hints) in files {
        let Ok(snapshot) = crate::hashline::served::read_snapshot(path, max_bytes) else {
            let _ = write!(
                content,
                "\n\nCould not re-read {}; read it again.",
                path.display()
            );
            continue;
        };
        let len = snapshot.lines.len();
        let served = ctx
            .anchors
            .served(path)
            .filter(|served| served.epoch == Some(snapshot.epoch));
        let version = served.as_ref().and_then(|served| {
            let version = served.resolve(parse_tag(tag)?).ok()?;
            Some((served, version))
        });
        let positions: Vec<usize> = hints
            .iter()
            .map(|&hint| {
                version
                    .and_then(|(served, version)| served.map_line(version, hint))
                    .unwrap_or(hint)
                    .min(len.saturating_sub(1))
            })
            .collect();
        let windows = feedback_windows(&positions, len, FEEDBACK_MAX_LINES);
        let current = ctx
            .anchors
            .record_served(path.to_path_buf(), &snapshot, &windows);
        let _ = write!(
            content,
            "\n\nCurrent [{}#{}] (these lines are now shown and editable):",
            path.display(),
            tag_text(current)
        );
        if len == 0 {
            content.push_str("\n(empty file)");
        }
        for (index, window) in windows.iter().enumerate() {
            if index > 0 {
                content.push_str("\n…");
            }
            for position in window.clone() {
                content.push('\n');
                content.push_str(&numbered_line(position + 1, &snapshot.lines[position]));
            }
        }
    }
}

/// 源文件附带内容围绕的 0 基位置（指请求 tag 的版本）：编辑范围与 move 源范围。
fn source_hints(request: &EditRequest) -> Vec<usize> {
    let mut hints = Vec::new();
    for op in &request.edits {
        match op {
            EditOp::Replace { from, to, .. } | EditOp::Delete { from, to } => {
                hints.extend([from.saturating_sub(1), to.saturating_sub(1)]);
            }
            EditOp::Insert { after, .. } => hints.push(after.saturating_sub(1)),
        }
    }
    if let Some(movement) = &request.move_lines {
        hints.extend([movement.from.saturating_sub(1), movement.to.saturating_sub(1)]);
    }
    hints
}

/// 目标文件附带内容围绕的 0 基位置：move 的插入点。
fn target_hints(movement: &MoveRequest) -> Vec<usize> {
    movement
        .after
        .map(|after| vec![after.saturating_sub(1)])
        .unwrap_or_default()
}

/// 回显窗口：每个提示取前 3 行到后 3 行并合并；整体跨度不超过 `max_lines`
/// 时用首尾之间的一个连续窗口，否则保留各自的小窗口并按总行数截断。
fn feedback_windows(
    hints: &[usize],
    len: usize,
    max_lines: usize,
) -> Vec<std::ops::Range<usize>> {
    if hints.is_empty() {
        let all = 0..len.min(max_lines);
        return vec![all];
    }
    let mut sorted = hints.to_vec();
    sorted.sort_unstable();
    let mut windows: Vec<std::ops::Range<usize>> = Vec::new();
    for hint in sorted {
        let window = hint.saturating_sub(3)..(hint + 4).min(len);
        match windows.last_mut() {
            Some(last) if window.start <= last.end => last.end = last.end.max(window.end),
            _ => windows.push(window),
        }
    }
    let first = windows[0].start;
    let last = windows[windows.len() - 1].end;
    if last - first <= max_lines {
        let span = first..last;
        return vec![span];
    }
    let mut budget = max_lines;
    windows
        .into_iter()
        .map_while(|window| {
            let take = window.len().min(budget);
            budget -= take;
            (take > 0).then_some(window.start..window.start + take)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_ops_including_insert_at_top() {
        let request = parse_requests(serde_json::json!({
            "path": "file.txt",
            "tag": "3F2A",
            "edits": [
                {"op": "insert", "after": 0, "text": "top"},
                {"op": "replace", "from": 2, "to": 3, "text": "x"},
                {"op": "delete", "from": 5, "to": 5},
            ],
        }))
        .expect("类型化 op 应能解析");
        assert!(matches!(request.edits[0], EditOp::Insert { after: 0, .. }));
        assert!(matches!(request.edits[1], EditOp::Replace { from: 2, to: 3, .. }));
        assert!(matches!(request.edits[2], EditOp::Delete { from: 5, to: 5 }));
    }

    #[test]
    fn rejects_unknown_ops_and_fields() {
        for args in [
            serde_json::json!({"path":"f","tag":"0000","edits":[{"op":"swap","from":1,"to":1}]}),
            serde_json::json!({"path":"f","tag":"0000","edits":[{"op":"delete","from":1,"to":1,"text":"x"}]}),
            serde_json::json!({"path":"f","tag":"0000","edits":[],"files":[]}),
        ] {
            let error = parse_requests(args).expect_err("应拒绝未知 op 或字段");
            assert_eq!(error.code, "E_BAD_PAYLOAD");
        }
    }

    #[test]
    fn rejects_missing_tag() {
        let error = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [{"op": "delete", "from": 1, "to": 1}],
        }))
        .expect_err("缺少 tag 必须拒绝");
        assert!(error.message.contains("tag"), "{}", error.message);
    }
}
