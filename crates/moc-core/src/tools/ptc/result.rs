//! 子工具结果：JS 拿到完整结果，模型看到的部分受展示预算限制。
use super::state::error;
use crate::tools::{ToolCtx, ToolOutcome, ToolOutput};
use moc_protocol::ToolStatus;
use moc_ptc_protocol::wire::{ScriptError, ScriptToolResult};
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
        ToolOutput::Text(text) | ToolOutput::Verbatim(text) => result.content.clone_from(text),
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
            result.content = decode_result_bytes(&bytes);
        }
    }
    let bytes = serde_json::to_vec(&result).expect("结果可序列化");
    if bytes.len() > limit {
        if result.artifact.is_none() {
            let text = if result.content.is_empty() {
                serde_json::to_string_pretty(&result.structured_content)
                    .expect("结构化结果可序列化")
            } else {
                std::mem::take(&mut result.content)
            };
            result.artifact = Some(
                artifacts
                    .archive_text(&text)
                    .await
                    .map_err(|err| error("E_RESULT_READ", err))?,
            );
        }
        return Err(oversized(result));
    }
    if outcome.is_error {
        let code = outcome.error_code.as_deref().unwrap_or("E_TOOL");
        let prefix = format!("[{code}] ");
        Err(ScriptError {
            code: code.into(),
            message: result
                .content
                .strip_prefix(&prefix)
                .unwrap_or(&result.content)
                .to_owned(),
            result: Some(Box::new(result)),
        })
    } else {
        Ok(result)
    }
}

/// 仅在脚本失败后回访已提交的子调用；成功路径不复制或重复归档工具正文。
pub(super) async fn recover_children(ctx: &ToolCtx, outcome: &mut ToolOutcome) {
    let path = ctx
        .paths
        .artifacts()
        .join(format!("{}-execute-results.txt", crate::ids::entry_id()));
    let target = path.clone();
    let transcript = ctx.paths.transcript.clone();
    let parent = ctx.tool_call_id.clone();
    let recovered =
        tokio::task::spawn_blocking(move || -> Result<usize, crate::error::CoreError> {
            use std::io::Write;
            #[derive(serde::Deserialize)]
            struct Record {
                call: Option<moc_protocol::methods::session::TranscriptChildCall>,
            }
            std::fs::create_dir_all(target.parent().expect("归档有父目录"))?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            let mut output = std::io::BufWriter::new(file);
            let mut count = 0;
            crate::session::log::for_each_line(&transcript, &mut |line| {
                if !line
                    .windows(parent.len())
                    .any(|part| part == parent.as_bytes())
                {
                    return Ok(());
                }
                let record: Record = serde_json::from_slice(line)
                    .map_err(|error| crate::error::CoreError::Session(error.to_string()))?;
                if let Some(call) = record.call
                    && call.parent_tool_call_id == parent
                {
                    writeln!(
                        output,
                        "## {} — {} ({:?})\n\n{}\n",
                        call.id, call.name, call.status, call.result
                    )?;
                    count += 1;
                }
                Ok(())
            })?;
            output.flush()?;
            Ok(count)
        })
        .await;
    let notice = match recovered {
        Ok(Ok(count)) => match ctx.artifact_store().uri_for_path(&path) {
            Ok(uri) => format!(
                "\n{count} nested call results were preserved at {uri}. Read this artifact before retrying; do not repeat successful calls. The script failure did not roll back completed writes."
            ),
            Err(error) => format!("\nCould not expose nested call results: {error}"),
        },
        Ok(Err(error)) => format!("\nCould not preserve nested call results: {error}"),
        Err(error) => format!("\nCould not preserve nested call results: {error}"),
    };
    if let ToolOutput::Text(text) = &mut outcome.content {
        text.push_str(&notice);
    }
}

/// 和其它出口一样宽松地解码结果文件：遇到二进制内容用替换字符代替，不整体失败。
fn decode_result_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn oversized(mut result: ScriptToolResult) -> ScriptError {
    result.content = "Tool ran, but its result exceeded the code-read limit".into();
    result.structured_content = None;
    result.is_error = true;
    result.status = "error".into();
    ScriptError {
        code: "E_RESULT_LIMIT".into(),
        message: result.content.clone(),
        result: Some(Box::new(result)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_result_bytes_decode_lossily_instead_of_failing() {
        assert_eq!(
            decode_result_bytes(&[0x68, 0x69, 0xff, 0x00, b'\n']),
            "hi\u{fffd}\0\n"
        );
    }
}
