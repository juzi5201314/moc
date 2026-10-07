//! 使用固定 WASM guest 执行 PTC 代码，宿主保留工具执行和会话权限。

mod result;
pub(crate) mod runtime;
mod scheduler;
mod state;
mod wire;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde_json::Value;
use wire::StartRequest;

pub struct RunCodeTool;

impl Tool for RunCodeTool {
    fn name(&self) -> &str {
        "run_code"
    }
    fn description(&self) -> &str {
        "在隔离的 QuickJS/WASM 中运行 JavaScript，通过 await tools.<name>(args) 调用工具。"
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
                    "code": {"type": "string", "description": "要执行的 JavaScript"},
                    "timeout_secs": {"type": "integer", "minimum": 1}
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
            let code = args
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::new("E_ARGUMENT", "run_code 需要字符串参数 code"))?;
            let limit = ctx.config.tools.ptc.max_output_bytes;
            if code.len() > limit {
                return Err(ToolError::new("E_OUTPUT_LIMIT", "脚本代码超过配置上限"));
            }
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(ctx.config.tools.ptc.default_timeout_secs)
                .min(ctx.config.tools.ptc.max_timeout_secs);
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
            let runtime = ctx
                .core
                .ptc
                .get_or_try_init(|| async {
                    tokio::task::spawn_blocking(runtime::PtcRuntime::compile)
                        .await
                        .map_err(|err| ToolError::new("E_PTC_RUNTIME", err.to_string()))
                        .and_then(std::convert::identity)
                })
                .await?;
            let guest = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => return Err(ToolError::new("E_CANCELLED", "脚本已取消")),
                _ = tokio::time::sleep_until(deadline) => return Err(ToolError::new("E_TIMEOUT", "脚本执行超时")),
                result = runtime.instantiate(ctx.config.tools.ptc.memory_limit_bytes) => result?,
            };
            let input = serde_json::to_vec(&StartRequest {
                code: code.to_owned(),
                memory_limit_bytes: ctx.config.tools.ptc.memory_limit_bytes,
                max_output_bytes: limit,
                max_pending_calls: ctx.config.tools.ptc.max_pending_calls,
            })
            .expect("启动参数可序列化");
            Ok(scheduler::run(ctx, guest, input, deadline).await)
        })
    }
}
