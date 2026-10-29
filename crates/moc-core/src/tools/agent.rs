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
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!("{label} must not be empty"),
        ));
    }
    Ok(value.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnTask {
    task: String,
    title: Option<String>,
    model: Option<String>,
    preset: Option<String>,
    output_schema: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    task: Option<String>,
    tasks: Option<Vec<SpawnTask>>,
    title: Option<String>,
    model: Option<String>,
    preset: Option<String>,
    context: Option<String>,
    output_schema: Option<Value>,
}

/// `tasks[]` 批量 spawn 的条数上限。
const MAX_SPAWN_TASKS: usize = 16;

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
    result: Option<Value>,
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
        "Create child agents. Single spawn: pass task (+ optional title/model/preset/context/output_schema). Batch: pass tasks[] (same fields per item, up to 16) with optional shared context. preset selects a registered agent preset (e.g. \"explore\"); output_schema makes the child's `yield` deliver a validated structured result."
    }
    fn tier(&self) -> Tier {
        Tier::Write
    }
    fn parallel_safe(&self) -> bool {
        false
    }
    fn def(&self) -> ToolDef {
        // `task`/`tasks` 互斥走 `oneOf`；批量上界与空任务在 schema 层直接可见，
        // run() 里的同款检查是执行契约（直接调用不经 schema）。
        ToolDef::new(
            "spawn_agent",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {"type":"string","minLength":1},
                    "tasks": {"type":"array","minItems":1,"maxItems":MAX_SPAWN_TASKS,"items":{
                        "type":"object","properties":{
                            "task":{"type":"string","minLength":1},
                            "title":{"type":"string"},
                            "model":{"type":"string","enum":["inherit","default","explore","plan"]},
                            "preset":{"type":"string"},
                            "output_schema":{"type":"object"}
                        },"required":["task"],"additionalProperties":false}},
                    "title": {"type":"string"},
                    "model":{"type":"string","enum":["inherit","default","explore","plan"]},
                    "preset":{"type":"string"},
                    "context":{"type":"string"},
                    "output_schema":{"type":"object"}
                },
                "additionalProperties": false,
                "oneOf": [{"required":["task"]},{"required":["tasks"]}]
            }),
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
            let items = match (args.task, args.tasks) {
                (Some(task), None) => vec![SpawnTask {
                    task,
                    title: args.title,
                    model: args.model,
                    preset: args.preset,
                    output_schema: args.output_schema,
                }],
                (None, Some(tasks)) => {
                    if tasks.is_empty() || tasks.len() > MAX_SPAWN_TASKS {
                        return Err(ToolError::new(
                            "E_BAD_PAYLOAD",
                            format!("tasks must contain 1..={MAX_SPAWN_TASKS} items"),
                        ));
                    }
                    if args.title.is_some()
                        || args.model.is_some()
                        || args.preset.is_some()
                        || args.output_schema.is_some()
                    {
                        return Err(ToolError::new(
                            "E_BAD_PAYLOAD",
                            "title/model/preset/output_schema belong inside each tasks[] item",
                        ));
                    }
                    tasks
                }
                (Some(_), Some(_)) => {
                    return Err(ToolError::new(
                        "E_BAD_PAYLOAD",
                        "task and tasks[] are mutually exclusive",
                    ));
                }
                (None, None) => {
                    return Err(ToolError::new(
                        "E_BAD_PAYLOAD",
                        "spawn_agent requires task or tasks[]",
                    ));
                }
            };
            let mut agents = Vec::with_capacity(items.len());
            let mut errors = Vec::new();
            for (index, item) in items.into_iter().enumerate() {
                if let Some(model) = item.model.as_deref()
                    && !matches!(model, "inherit" | "default" | "explore" | "plan")
                {
                    errors.push(serde_json::json!({
                        "index": index,
                        "error": "model must be inherit, default, explore, or plan",
                    }));
                    continue;
                }
                let spec = crate::agents::SpawnSpec {
                    task: item.task,
                    title: item.title,
                    model: item.model,
                    preset: item.preset,
                    context: args.context.clone(),
                    output_schema: item.output_schema,
                };
                match crate::agents::spawn(
                    std::sync::Arc::clone(&ctx.core),
                    ctx.request.clone(),
                    ctx.session_id.clone(),
                    spec,
                )
                .await
                {
                    Ok(result) => {
                        let mut agent = serde_json::json!({
                            "agent_id": result.agent.id,
                            "status": crate::agents::status_word(result.agent.status),
                            "model": result.agent.model,
                        });
                        if let Some(preset) = result.agent.preset {
                            agent["preset"] = serde_json::Value::String(preset);
                        }
                        agents.push(agent);
                    }
                    Err(error) => errors.push(serde_json::json!({
                        "index": index,
                        "error": error.to_string(),
                    })),
                }
            }
            let body = serde_json::json!({"agents": agents, "errors": errors});
            let mut outcome = ToolOutcome::success(body.to_string());
            // 批量全部失败时按错误返回，单项失败保留在 errors 数组里给模型看。
            outcome.is_error = body["agents"].as_array().is_some_and(Vec::is_empty);
            Ok(outcome)
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
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "list_agents takes no arguments",
                ));
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
                return Err(ToolError::new(
                    "E_BAD_PAYLOAD",
                    "timeout_secs must be greater than 0",
                ));
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
            schema(
                serde_json::json!({"report":{"type":"string"}, "result":{}}),
                &["report"],
            ),
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
            crate::agents::record_yield(&ctx.core, &ctx.session_id, report, args.result)
                .await
                .map_err(|error| ToolError::new("E_AGENT", error.to_string()))?;
            Ok(ToolOutcome::end_prompt(
                "Final report delivered to the parent agent; this turn ends now.",
            ))
        })
    }
}
