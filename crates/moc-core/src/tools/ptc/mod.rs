//! execute 工具：在 Core 级长期 QuickJS Host 中执行独立 Runtime；工具调用和会话权限仍由宿主负责。

mod process;
mod result;
mod scheduler;
mod state;

pub(crate) use process::PtcHost;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_ptc_protocol::wire::StartRequest;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub struct ExecuteTool;

/// `description` 参数的模型可见说明，用作前端执行卡片的摘要。
const DESCRIPTION_PARAM: &str = "Concise 5-10 word active-voice summary shown in the UI (e.g. \"Count TODO markers across crates\").";

/// 校验前端卡片摘要非空。
fn validate_description(args: &Value) -> Result<(), ToolError> {
    if args
        .get("description")
        .and_then(Value::as_str)
        .is_none_or(|description| description.trim().is_empty())
    {
        return Err(ToolError::new(
            "E_ARGUMENT",
            "execute requires a non-empty description argument",
        ));
    }
    Ok(())
}

impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }
    fn description(&self) -> &str {
        "Run JavaScript with await tools.<name>(args); return needed results or use console.log/print. Standard mode also supports direct tool calls."
    }
    fn tier(&self) -> Tier {
        Tier::Exec
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            self.name(),
            schema(
                serde_json::json!({
                    "code": {"type": "string", "description": "JavaScript function body; await every host call. tools, catalog and state are host namespaces: do not shadow them. Use JSON strings for Markdown containing backticks."},
                    "description": {"type": "string", "description": DESCRIPTION_PARAM},
                    "timeout_secs": {"type": "integer", "minimum": 1, "description": "Execution deadline in seconds, excluding time waiting for user answers or permission decisions"}
                }),
                &["code", "description"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let code = args.get("code").and_then(Value::as_str).ok_or_else(|| {
                ToolError::new("E_ARGUMENT", "execute requires a string code argument")
            })?;
            validate_description(&args)?;
            let limit = ctx.config.tools.ptc.max_output_bytes;
            if code.len() > limit {
                return Err(ToolError::new(
                    "E_OUTPUT_LIMIT",
                    "script code exceeds the configured limit",
                ));
            }
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(ctx.config.tools.ptc.default_timeout_secs)
                .min(ctx.config.tools.ptc.max_timeout_secs);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
            let worker =
                ctx.core.boot.ptc_worker.clone().ok_or_else(|| {
                    ToolError::new("E_PTC_WORKER", "PTC worker is not configured")
                })?;
            let host = {
                let mut slot = ctx.core.ptc_host.lock().await;
                Arc::clone(slot.get_or_insert_with(|| Arc::new(PtcHost::new(worker))))
            };
            let max_message_bytes = ctx
                .config
                .tools
                .ptc
                .max_result_bytes
                .max(limit)
                .saturating_add(4096);
            let request = StartRequest {
                code: code.to_owned(),
                memory_limit_bytes: ctx.config.tools.ptc.memory_limit_bytes,
                max_output_bytes: limit,
                max_message_bytes,
                max_pending_calls: ctx.config.tools.ptc.max_pending_calls,
                max_total_calls: ctx.config.tools.ptc.max_total_calls,
                timeout_ms: timeout_secs.saturating_mul(1000),
            };
            let guest = host.start(request, &ctx.config.tools.ptc)?;
            Ok(scheduler::run(ctx, guest, deadline).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_requires_a_ui_description() {
        let parameters = ExecuteTool.def().parameters;
        assert_eq!(
            parameters["required"],
            serde_json::json!(["code", "description"])
        );
        let description = parameters["properties"]["description"]["description"]
            .as_str()
            .expect("description 参数必须有说明");
        assert!(description.contains("shown in the UI"));
    }

    #[test]
    fn description_must_be_non_empty() {
        for (args, ok) in [
            (
                serde_json::json!({"code":"return 1","description":"Sum two numbers"}),
                true,
            ),
            (
                serde_json::json!({"code":"return 1","description":"  "}),
                false,
            ),
            (serde_json::json!({"code":"return 1"}), false),
            (
                serde_json::json!({"code":"return 1","description":42}),
                false,
            ),
        ] {
            assert_eq!(validate_description(&args).is_ok(), ok, "{args}");
        }
    }
}
