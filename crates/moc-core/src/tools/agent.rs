//! 子代理相关工具：通过 ToolCtx 调用 core 的代理注册表。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::methods::{AgentMessageOutcome, AgentMessageResult};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

fn text(value: &str, label: &str) -> Result<String, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ToolError::new("E_BAD_PAYLOAD", format!("{label} must not be empty")));
    }
    Ok(value.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task: String,
    title: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageArgs {
    target: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    agent_id: String,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    agent_ids: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YieldArgs {
    report: String,
}

pub struct SpawnAgentTool;
pub struct ListAgentsTool;
pub struct MessageAgentTool;
pub struct StopAgentTool;
pub struct WaitAgentTool;
pub struct YieldTool;

impl Tool for SpawnAgentTool {
    fn name(&self) -> &'static str {
        "spawn_agent"
    }
    fn description(&self) -> &'static str {
        "Create a direct child agent."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "spawn_agent",
            schema(
                serde_json::json!({
                    "task": {"type":"string"}, "title":{"type":"string"},
                    "model":{"type":"string","enum":["inherit","default","explore","plan"]}
                }),
                &["task"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: SpawnArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let task = text(&args.task, "task")?;
            if let Some(model) = args.model.as_deref()
                && !matches!(model, "inherit" | "default" | "explore" | "plan")
            {
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "model must be inherit, default, explore, or plan",
                ));
            }
            let result = crate::agents::spawn(
                std::sync::Arc::clone(&ctx.core),
                ctx.request.clone(),
                ctx.session_id.clone(),
                task,
                args.title,
                args.model,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::json!({
                    "agent_id": result.agent.id,
                    "status": crate::agents::status_word(result.agent.status),
                    "model": result.agent.model,
                })
                .to_string(),
            ))
        })
    }
}

impl Tool for ListAgentsTool {
    fn name(&self) -> &'static str {
        "list_agents"
    }
    fn description(&self) -> &'static str {
        "List agents you can message: direct children, or siblings inside a subagent."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new("list_agents", schema(serde_json::json!({}), &[]))
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            if !args.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(ToolError::new("E_BAD_PAYLOAD", "list_agents takes no arguments"));
            }
            let result = crate::agents::list(&ctx.core, ctx.session_id.clone())
                .await
                .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::to_string(&result.agents).expect("AgentInfo 可序列化"),
            ))
        })
    }
}

impl Tool for MessageAgentTool {
    fn name(&self) -> &'static str {
        "message_agent"
    }
    fn description(&self) -> &'static str {
        "Message agents. target: an agent id, \"parent\", or \"all\" (running direct children; siblings inside a subagent). Running targets receive it at their next safe boundary; idle targets start a turn. Returns one receipt per target."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "message_agent",
            schema(
                serde_json::json!({
                    "target":{"type":"string"}, "message":{"type":"string"}
                }),
                &["target", "message"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: MessageArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let result = crate::agents::message(
                std::sync::Arc::clone(&ctx.core),
                ctx.request.clone(),
                ctx.session_id.clone(),
                text(&args.target, "target")?,
                text(&args.message, "message")?,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            let body = serde_json::to_string(&result).expect("AgentMessageResult 可序列化");
            let mut outcome = ToolOutcome::success(body);
            // 全部失败时标记为错误结果，内容仍是原始回执，模型照样能看到每个目标的失败原因。
            outcome.is_error = all_failed(&result);
            Ok(outcome)
        })
    }
}

/// 所有目标都投递失败时按错误返回：模型必须知道没有任何代理收到消息。
fn all_failed(result: &AgentMessageResult) -> bool {
    !result.receipts.is_empty()
        && result
            .receipts
            .iter()
            .all(|receipt| receipt.outcome == AgentMessageOutcome::Failed)
}

impl Tool for StopAgentTool {
    fn name(&self) -> &'static str {
        "stop_agent"
    }
    fn description(&self) -> &'static str {
        "Cancel a running child agent."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "stop_agent",
            schema(
                serde_json::json!({
                    "agent_id":{"type":"string"}, "reason":{"type":"string"}
                }),
                &["agent_id"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: StopArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let result = crate::agents::stop(
                &ctx.core,
                ctx.session_id.clone(),
                text(&args.agent_id, "agent_id")?,
                args.reason,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::json!({
                    "stopped": result.stopped,
                    "status": crate::agents::status_word(result.status),
                })
                .to_string(),
            ))
        })
    }
}

impl Tool for WaitAgentTool {
    fn name(&self) -> &'static str {
        "wait_agent"
    }
    fn description(&self) -> &'static str {
        "Wait until a watched child settles, a child message arrives, or timeout_secs elapses (default 180). agent_ids defaults to all running children. Returns every watched agent's snapshot; settled ones include their final message."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "wait_agent",
            schema(
                serde_json::json!({
                    "agent_ids":{"type":"array","items":{"type":"string"}},
                    "timeout_secs":{"type":"integer","minimum":1}
                }),
                &[],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: WaitArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            if args.timeout_secs == Some(0) {
                return Err(ToolError::new("E_BAD_PAYLOAD", "timeout_secs must be greater than 0"));
            }
            let progress = crate::agents::WaitProgress {
                updates: ctx.request.updates(),
                session_id: ctx.session_id.clone(),
                prompt_id: ctx.prompt_id.clone(),
                tool_call_id: ctx.tool_call_id.clone(),
            };
            let result = crate::agents::wait(
                &ctx.core,
                &ctx.session_id,
                args.agent_ids,
                args.timeout_secs,
                Some(&progress),
                &ctx.cancel,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            let json = serde_json::to_value(&result).expect("AgentWaitResult 可序列化");
            let mut outcome = ToolOutcome::success(json.to_string());
            outcome.structured_content = Some(json);
            Ok(outcome)
        })
    }
}

impl Tool for YieldTool {
    fn name(&self) -> &'static str {
        "yield"
    }
    fn description(&self) -> &'static str {
        "Deliver your complete final report to the parent and end the turn; call once when the task is done."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "yield",
            schema(serde_json::json!({"report":{"type":"string"}}), &["report"]),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: YieldArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let report = text(&args.report, "report")?;
            crate::agents::record_yield(&ctx.core, &ctx.session_id, report)
                .await
                .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::end_prompt(
                "Final report delivered to the parent agent; this turn ends now.",
            ))
        })
    }
}
