//! 仅允许编辑已经展示的锚点范围；拒绝时直接服务当前文件。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolControl, ToolCtx, ToolError, ToolOutcome, resolve_tool_path};
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
            let requests = serde_json::from_value::<EditPayload>(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?
                .into_requests();
            if requests.is_empty() {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "files must contain at least one file",
                ));
            }

            let mut content = String::new();
            let mut files = Vec::new();
            let mut truncated = false;
            let mut changed = false;
            for request in requests {
                match apply_one(ctx, request).await? {
                    Ok(output) => {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        changed |= output.content != "No changes made.";
                        content.push_str(&output.content);
                        if let Some(diff) = output.diff {
                            truncated |= diff.truncated;
                            files.extend(diff.files);
                        }
                    }
                    Err(outcome) => return Ok(outcome),
                }
            }

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
                diff: (!files.is_empty()).then_some(moc_protocol::ToolDiff { files, truncated }),
            })
        })
    }
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
