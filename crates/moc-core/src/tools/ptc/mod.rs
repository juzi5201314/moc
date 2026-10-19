//! run_code 工具：在独立 QuickJS worker 进程里执行 PTC 脚本；工具调用和会话权限仍由宿主负责。

mod process;
mod result;
mod scheduler;
mod state;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_ptc_protocol::wire::StartRequest;
use serde_json::Value;
use std::time::Duration;

pub struct RunCodeTool;

impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        "run_code"
    }
    fn description(&self) -> &str {
        "Run JavaScript in a separate QuickJS worker process; call tools with await tools.<name>(args)."
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
                    "code": {"type": "string", "description": "JavaScript to execute"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "description": "Seconds before the script is stopped"}
                }),
                &["code"],
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
                ToolError::new("E_ARGUMENT", "run_code requires a string code argument")
            })?;
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
            let worker = ctx
                .core
                .boot
                .ptc_worker
                .clone()
                .ok_or_else(|| ToolError::new("E_PTC_WORKER", "PTC worker 未配置"))?;
            let guest = process::Guest::spawn(&worker)?;
            let max_message_bytes = ctx
                .config
                .tools
                .ptc
                .max_result_bytes
                .max(limit)
                .saturating_add(4096);
            let input = serde_json::to_vec(&StartRequest {
                code: code.to_owned(),
                memory_limit_bytes: ctx.config.tools.ptc.memory_limit_bytes,
                max_output_bytes: limit,
                max_message_bytes,
                max_pending_calls: ctx.config.tools.ptc.max_pending_calls,
            })
            .expect("启动参数可序列化");
            Ok(scheduler::run(ctx, guest, input, deadline).await)
        })
    }
}
