//! JS 完整结果与模型展示预算分离。
use super::{
    state::error,
    wire::{ScriptError, ScriptToolResult},
};
use crate::tools::{ToolCtx, ToolOutcome, ToolOutput};
use moc_protocol::ToolStatus;
use tokio::io::AsyncReadExt;

pub(super) fn status(outcome: &ToolOutcome) -> ToolStatus {
    outcome.status.unwrap_or(if outcome.is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Completed
    })
}

pub(super) async fn script_result(
    ctx: &ToolCtx,
    outcome: &ToolOutcome,
) -> Result<ScriptToolResult, ScriptError> {
    let artifacts = ctx.artifact_store();
    let limit = ctx.config.tools.ptc.max_result_bytes;
    let mut result = ScriptToolResult {
        content: String::new(),
        summary: None,
        structured_content: outcome.structured_content.clone(),
        is_error: outcome.is_error,
        status: match status(outcome) {
            ToolStatus::Completed => "completed",
            ToolStatus::Denied => "denied",
            ToolStatus::Cancelled => "cancelled",
            _ => "error",
        }
        .into(),
        artifact: None,
    };
    match &outcome.content {
        ToolOutput::Text(text) => result.content.clone_from(text),
        ToolOutput::Excerpt { text, path } => {
            result.content.clone_from(text);
            result.artifact = Some(
                artifacts
                    .uri_for_path(path)
                    .map_err(|err| error("E_RESULT_READ", err))?,
            );
        }
        ToolOutput::File { path, suffix } => {
            result.artifact = Some(
                artifacts
                    .uri_for_path(path)
                    .map_err(|err| error("E_RESULT_READ", err))?,
            );
            result.summary = Some(suffix.clone());
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|err| error("E_RESULT_READ", err))?;
            let mut bytes = Vec::new();
            file.take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|err| error("E_RESULT_READ", err))?;
            if bytes.len() > limit {
                return Err(oversized(result));
            }
            result.content = String::from_utf8(bytes).map_err(|err| error("E_RESULT_READ", err))?;
        }
    }
    let bytes = serde_json::to_vec(&result).expect("结果可序列化");
    if bytes.len() > limit {
        if result.artifact.is_none() {
            result.artifact = Some(
                artifacts
                    .archive_text(std::str::from_utf8(&bytes).expect("JSON UTF-8"))
                    .await
                    .map_err(|err| error("E_RESULT_READ", err))?,
            );
        }
        return Err(oversized(result));
    }
    if outcome.is_error {
        Err(ScriptError {
            code: outcome.error_code.as_deref().unwrap_or("E_TOOL").into(),
            message: result.content.clone(),
            result: Some(result),
        })
    } else {
        Ok(result)
    }
}

fn oversized(mut result: ScriptToolResult) -> ScriptError {
    result.content = "Tool ran, but its result exceeded the code-read limit".into();
    result.structured_content = None;
    result.is_error = true;
    result.status = "error".into();
    ScriptError {
        code: "E_RESULT_LIMIT".into(),
        message: result.content.clone(),
        result: Some(result),
    }
}
