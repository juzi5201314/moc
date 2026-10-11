//! 子代理工具：父会话通过统一 ToolCtx 调用 core registry。

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use moc_protocol::methods::{AgentStatus, AgentWaitResult};
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};

fn text(value: &str, label: &str) -> Result<String, ToolError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ToolError::new("E_BAD_PAYLOAD", format!("{label} 不能为空")));
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
struct AgentMessageArgs {
    agent_id: String,
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
    agent_id: String,
    timeout_secs: Option<u64>,
}

pub struct SpawnAgentTool;
pub struct ListAgentsTool;
pub struct SendAgentTool;
pub struct SteerAgentTool;
pub struct StopAgentTool;
pub struct WaitAgentTool;

impl Tool for SpawnAgentTool {
    fn name(&self) -> &'static str {
        "spawn_agent"
    }
    fn description(&self) -> &'static str {
        "Create a direct child agent. model accepts inherit, default, explore, or plan."
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
                    "model 只允许 inherit、default、explore、plan",
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
                    "status": status_name(result.agent.status),
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
        "List direct child agents of the current session."
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
                return Err(ToolError::new("E_BAD_PAYLOAD", "list_agents 不接受参数"));
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

impl Tool for SendAgentTool {
    fn name(&self) -> &'static str {
        "send_agent"
    }
    fn description(&self) -> &'static str {
        "Send a new message to an idle child agent."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "send_agent",
            schema(
                serde_json::json!({
                    "agent_id":{"type":"string"}, "message":{"type":"string"}
                }),
                &["agent_id", "message"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: AgentMessageArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let result = crate::agents::send(
                std::sync::Arc::clone(&ctx.core),
                ctx.request.clone(),
                ctx.session_id.clone(),
                text(&args.agent_id, "agent_id")?,
                text(&args.message, "message")?,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::json!({
                    "accepted": result.accepted, "status": status_name(result.status)
                })
                .to_string(),
            ))
        })
    }
}

impl Tool for SteerAgentTool {
    fn name(&self) -> &'static str {
        "steer_agent"
    }
    fn description(&self) -> &'static str {
        "Steer a running child agent at its next safe boundary."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        ToolDef::new(
            "steer_agent",
            schema(
                serde_json::json!({
                    "agent_id":{"type":"string"}, "message":{"type":"string"}
                }),
                &["agent_id", "message"],
            ),
        )
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: AgentMessageArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let result = crate::agents::steer(
                &ctx.core,
                ctx.session_id.clone(),
                text(&args.agent_id, "agent_id")?,
                text(&args.message, "message")?,
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::json!({"accepted": result.accepted}).to_string(),
            ))
        })
    }
}

impl Tool for StopAgentTool {
    fn name(&self) -> &'static str {
        "stop_agent"
    }
    fn description(&self) -> &'static str {
        "Request cancellation of a running child agent."
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
                    "stopped": result.stopped, "status": status_name(result.status)
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
        "Wait for a child agent and return its latest assistant message."
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
                    "agent_id":{"type":"string"}, "timeout_secs":{"type":"integer","minimum":1}
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
            let args: WaitArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            let result: AgentWaitResult = crate::agents::wait(
                &ctx.core,
                ctx.session_id.clone(),
                text(&args.agent_id, "agent_id")?,
                args.timeout_secs
                    .map(|seconds| seconds.saturating_mul(1000)),
            )
            .await
            .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::success(
                serde_json::json!({
                    "completed": result.completed,
                    "status": status_name(result.agent.status),
                    "message": result.message.as_ref().map(moc_llm::Message::text),
                })
                .to_string(),
            ))
        })
    }
}

fn status_name(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Completed => "completed",
        AgentStatus::Stopped => "stopped",
        AgentStatus::Failed => "failed",
        AgentStatus::Idle => "idle",
    }
}
