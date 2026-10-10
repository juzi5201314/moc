//! CLM 实验模式的上下文维护。

use futures_util::future::BoxFuture;
use moc_llm::{Message, ToolDef};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::clm::{self, MAX_CONTEXT_BYTES};
use crate::session::state::SessionState;
use crate::session::{SessionTable, entries::EntryPayload};
use std::sync::Arc;

pub struct ClmContextTool;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Read,
    Replace,
    Append,
}

#[derive(Debug, Deserialize)]
struct Request {
    op: Operation,
    content: Option<String>,
}

/// 首次进入 CLM 请求时建立工作上下文快照。
pub async fn ensure_context(
    sessions: &SessionTable,
    session_id: &str,
    state: &SessionState,
    messages: &[Message],
) -> Result<String, crate::error::CoreError> {
    if let Some(context) = &state.clm_context {
        return Ok(context.clone());
    }
    let context = clm::render(messages);
    sessions
        .append(
            session_id,
            EntryPayload::ClmContext {
                content: context.clone(),
            },
        )
        .await?;
    Ok(context)
}

/// 将模型或工具消息追加到当前 CLM 工作上下文并持久化版本。
pub async fn append_message(
    sessions: &SessionTable,
    state: &Arc<tokio::sync::Mutex<SessionState>>,
    session_id: &str,
    message: &Message,
) -> Result<(), crate::error::CoreError> {
    let current = state.lock().await.clm_context.clone();
    let Some(mut context) = current else {
        return Ok(());
    };
    context.push_str("\n");
    context.push_str(&serde_json::to_string(message).expect("模型消息必须可序列化"));
    if context.len() > MAX_CONTEXT_BYTES {
        return Err(crate::error::CoreError::ContextOverflow(
            "CLM live context exceeded its byte limit".into(),
        ));
    }
    sessions
        .append(session_id, EntryPayload::ClmContext { content: context })
        .await?;
    Ok(())
}

impl Tool for ClmContextTool {
    fn name(&self) -> &'static str {
        "clm_context"
    }

    fn description(&self) -> &'static str {
        "Manage the live working context in CLM experimental mode. Read it, replace it, or append to it; the immutable transcript remains available as the audit source."
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    fn parallel_safe(&self) -> bool {
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            self.name(),
            schema(
                serde_json::json!({
                    "op": {"type": "string", "enum": ["read", "replace", "append"]},
                    "content": {"type": "string", "description": "The complete replacement or text to append."}
                }),
                &["op"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let request: Request = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_ARGUMENT", error.to_string()))?;
            let current = ctx
                .state
                .lock()
                .await
                .clm_context
                .clone()
                .unwrap_or_default();
            let content = match request.op {
                Operation::Read => return Ok(ToolOutcome::success(current)),
                Operation::Replace => request.content.ok_or_else(|| {
                    ToolError::new("E_ARGUMENT", "content is required for replace")
                })?,
                Operation::Append => {
                    let addition = request.content.ok_or_else(|| {
                        ToolError::new("E_ARGUMENT", "content is required for append")
                    })?;
                    let mut content = current;
                    content.push_str(&addition);
                    content
                }
            };
            if content.len() > MAX_CONTEXT_BYTES {
                return Err(ToolError::new(
                    "E_CONTEXT_LIMIT",
                    format!("live context exceeds {MAX_CONTEXT_BYTES} bytes"),
                ));
            }
            ctx.sessions
                .append(
                    &ctx.session_id,
                    EntryPayload::ClmContext {
                        content: content.clone(),
                    },
                )
                .await
                .map_err(|error| ToolError::new("E_CONTEXT_WRITE", error.to_string()))?;
            Ok(ToolOutcome::success(format!(
                "CLM live context updated ({} bytes); the next model request uses the new view.",
                content.len()
            )))
        })
    }
}
