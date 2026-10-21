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

/// `description` 参数的模型可见说明；与 dsh 的 run_code 工具保持同一前端卡片契约。
const DESCRIPTION_PARAM: &str = "Clear, concise description of what this program does in active voice, 5-10 words (shown in the UI). Examples: \"Count TODO markers across crates\"; \"Read failing test and its fixture\"; \"Rename a config key across files\".";

/// 模型为前端卡片提供的一句话说明；目前只校验非空，展示留给后续 UI 消费。
fn validate_description(args: &Value) -> Result<(), ToolError> {
    if args
        .get("description")
        .and_then(Value::as_str)
        .is_none_or(|description| description.trim().is_empty())
    {
        return Err(ToolError::new(
            "E_ARGUMENT",
            "run_code requires a non-empty description argument",
        ));
    }
    Ok(())
}

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
                    "description": {"type": "string", "description": DESCRIPTION_PARAM},
                    "timeout_secs": {"type": "integer", "minimum": 1, "description": "Seconds before the script is stopped"}
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
                ToolError::new("E_ARGUMENT", "run_code requires a string code argument")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_requires_a_ui_description() {
        let parameters = RunCodeTool.def().parameters;
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
