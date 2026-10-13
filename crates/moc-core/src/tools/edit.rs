//! 仅允许编辑已经展示的锚点范围；拒绝时直接服务当前文件。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, ToolOutput, resolve_tool_path};
use crate::hashline::apply::{EditError, EditOutput, EditRequest, apply_edit};
use crate::resources::ResourceRouter;

fn reject_resource_path(path: &str) -> Result<(), ToolError> {
    ResourceRouter::write(path).map_err(|error| ToolError::new(error.code, error.message))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EditPayload {
    Single(EditRequest),
    Batch { files: Vec<EditRequest> },
}

impl EditPayload {
    fn into_requests(self) -> Vec<EditRequest> {
        match self {
            Self::Single(request) => vec![request],
            Self::Batch { files } => files,
        }
    }
}

/// 解析 edit 参数；授权（含工作区边界与预览）与执行共用同一份解析。
pub(crate) fn parse_requests(args: Value) -> Result<Vec<EditRequest>, ToolError> {
    let requests = serde_json::from_value::<EditPayload>(args)
        .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?
        .into_requests();
    if requests.is_empty() {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            "files must contain at least one file",
        ));
    }
    Ok(requests)
}

pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit existing files using the three-character hashline anchors returned by read. Use this for local changes; do not use bash, sed, perl, Python, redirection, or similar commands. The files form accepts multiple files and applies them in order. Each file contains edits as [from_anchor, to_anchor, replacement]; a non-empty replacement with identical anchors inserts after that line, and an empty replacement deletes it. move transfers a range between files using the original snapshot anchors."
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
    let properties = edit_file_properties();
    let required = ["path", "edits"];
    serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": edit_file_properties(),
                            "required": required,
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["files"],
                "additionalProperties": false
            }
        ]
    })
}

fn edit_file_properties() -> Value {
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
            if ["E_UNSERVED_RANGE", "E_STALE_RANGE", "E_STALE_ANCHOR"]
                .iter()
                .any(|code| content.contains(code))
            {
                for path in std::iter::once(&source_path).chain(target_path.iter()) {
                    let fresh = super::read::ReadTool
                        .run(ctx, serde_json::json!({"path":path}))
                        .await?;
                    let super::ToolOutput::Text(text) = fresh.content else {
                        unreachable!("read returned a paginated text result");
                    };
                    content.push_str(&format!("\nFresh anchors for the current content:\n{text}"));
                }
            }
            let error_code = content
                .split_once(']')
                .and_then(|(prefix, _)| prefix.strip_prefix('['))
                .filter(|code| code.starts_with("E_"))
                .unwrap_or("E_TOOL")
                .to_owned();
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

    fn diff(path: &str) -> moc_protocol::ToolDiff {
        moc_protocol::ToolDiff {
            files: vec![moc_protocol::ToolFileDiff {
                path: path.into(),
                hunks: Vec::new(),
            }],
            truncated: false,
        }
    }

    #[test]
    fn batch_failure_keeps_applied_files_and_their_diff() {
        let mut report = BatchReport::default();
        report.record(EditOutput {
            content: "[a.rs]\n- AA1│old\n+ AA1│new".into(),
            diff: Some(diff("a.rs")),
        });
        let outcome = report.finish_failure(ToolOutcome::failure(
            "E_STALE_RANGE",
            "b.rs changed externally after read",
        ));
        assert!(outcome.is_error);
        let ToolOutput::Text(text) = outcome.content else {
            panic!("失败结果是文本");
        };
        assert!(text.contains("Already applied before the failure:"));
        assert!(text.contains("[a.rs]"));
        assert!(text.contains("E_STALE_RANGE"));
        let applied = outcome.diff.expect("失败结果保留已应用 diff");
        assert_eq!(applied.files.len(), 1);
        assert_eq!(applied.files[0].path, "a.rs");
    }

    #[test]
    fn preview_reports_the_diff_the_approval_flow_shows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "alpha\nbeta\n").unwrap();
        let snapshot = crate::hashline::served::read_snapshot(&path, 4096).unwrap();
        let anchors = crate::hashline::AnchorStore::new();
        let rows: Vec<_> = snapshot.hashes.iter().copied().enumerate().collect();
        anchors.record_served(path.clone(), &rows, snapshot.clone());
        let anchor = crate::hashline::encode_anchor(snapshot.hashes[0]).unwrap();
        let requests = parse_requests(serde_json::json!({
            "path": "file.txt",
            "edits": [[anchor.clone(), anchor, "inserted"]],
        }))
        .unwrap();
        let diff = preview_requests(dir.path(), requests, &anchors, 32, 4096).expect("预览成功");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, path.display().to_string());
        assert!(diff.files[0].hunks.iter().any(|hunk| hunk
            .lines
            .iter()
            .any(|line| line.kind == moc_protocol::DiffLineKind::Insert
                && line.text == "inserted")));
        // 未读取过的文件没有可预览的计划：整体放弃而不是给出误导性 diff。
        let missing = parse_requests(serde_json::json!({
            "path": "other.txt",
            "edits": [["AAA", "AAA", "inserted"]],
        }))
        .unwrap();
        assert!(preview_requests(dir.path(), missing, &anchors, 32, 4096).is_none());
    }

    #[test]
    fn failure_without_applied_files_keeps_the_original_error() {
        let outcome = BatchReport::default().finish_failure(ToolOutcome::failure(
            "E_BAD_PAYLOAD",
            "edits must contain at least one item",
        ));
        let ToolOutput::Text(text) = outcome.content else {
            panic!("失败结果是文本");
        };
        assert_eq!(text, "[E_BAD_PAYLOAD] edits must contain at least one item");
        assert!(outcome.diff.is_none());
    }
}
