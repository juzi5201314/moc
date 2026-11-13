use crate::{
    error::CoreError,
    service::CoreInner,
    tools::{ToolControl, ToolCtx, ToolOutcome, ToolOutput},
};
use futures_util::{FutureExt, future::BoxFuture};
use moc_llm::{ContentPart, FinishReason, Message, Role};
use moc_protocol::{
    RpcError, RpcErrorCode,
    methods::{ToolExecution, ToolStatus},
};
use serde_json::Value;
use std::{panic::AssertUnwindSafe, time::Instant};
type PendingTool<'a> = BoxFuture<'a, (usize, Result<CommittedTool, CoreError>)>;

/// 一个已经持久化的工具步骤。
///
/// 完整结果在提交时已经写入 transcript，这里只在批次收尾前短暂保留发送通知所需的副本；
/// 批次里先完成的工具不再等待后一个工具结束。
struct CommittedTool {
    status: ToolStatus,
    duration_ms: u64,
    diff: Option<moc_protocol::methods::ToolDiff>,
    end_turn: bool,
    observation: Option<(u128, bool, bool)>,
    /// 上游协议错误：本步骤仍以失败结果入库，但必须终止本批次。
    protocol_fault: Option<String>,
    result: moc_llm::ToolResult,
}

fn note_protocol_fault(stop_reason: &mut Option<RpcError>, committed: &CommittedTool) {
    if committed.protocol_fault.is_some() && stop_reason.is_none() {
        *stop_reason = Some(RpcError::of(RpcErrorCode::InternalError));
    }
}

/// 执行一次工具并把完成的结果立即提交，只有持久化失败才返回错误。
async fn run_step(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    start: Instant,
) -> Result<CommittedTool, CoreError> {
    let (outcome, protocol_fault) = match execute_tool(inner, ctx, call, true).await {
        Ok(outcome) => (outcome, None),
        Err(CoreError::Protocol(message)) => (
            ToolOutcome::failure("E_PROTOCOL", message.clone()),
            Some(message),
        ),
        Err(error) => (ToolOutcome::failure("E_TOOL", error.to_string()), None),
    };
    let duration_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let mut committed = commit_outcome(inner, ctx, call, outcome, duration_ms).await?;
    committed.protocol_fault = protocol_fault;
    Ok(committed)
}

/// 把一次执行结果归档、格式化并追加到 transcript；返回通知前端所需的小结构。
async fn commit_outcome(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    outcome: ToolOutcome,
    duration_ms: u64,
) -> Result<CommittedTool, CoreError> {
    let observation = if ctx.config.limits.loop_detection {
        Some(super::guard::observe_tool(ctx, call, &outcome).await?)
    } else {
        None
    };
    commit_prepared(inner, ctx, call, outcome, duration_ms, observation).await
}

async fn commit_prepared(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    mut outcome: ToolOutcome,
    duration_ms: u64,
    observation: Option<(u128, bool, bool)>,
) -> Result<CommittedTool, CoreError> {
    let content = match crate::tools::output::prepare_outcome(
        &outcome,
        &ctx.config.tools.output,
        &ctx.artifact_store(),
    )
    .await
    {
        Ok(content) => content,
        Err(error) => {
            outcome.is_error = true;
            outcome.error_code = Some("E_ARTIFACT".into());
            format!("[E_ARTIFACT] Tool ran, but its output could not be delivered: {error}")
        }
    };
    let status = outcome.status.unwrap_or(if outcome.is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Completed
    });
    let result = moc_llm::ToolResult {
        tool_call_id: call.id.clone(),
        content,
        is_error: outcome.is_error,
        images: outcome.images.clone(),
    };
    // EndTurn 工具（yield/exit_plan）的结果带上「正常结束」标记：transcript 尾部
    // 据此区分已完成回合与「工具跑完、下一次模型请求失败」——条目级字段，不进协议。
    let finish_reason = (outcome.control == ToolControl::EndTurn).then_some(FinishReason::Stop);
    inner
        .sessions
        .append_message(
            &ctx.session_id,
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult(result.clone())].into(),
            },
            Some(ToolExecution {
                tool_call_id: call.id.clone(),
                status,
                duration_ms: Some(duration_ms),
                diff: outcome.diff.clone(),
            }),
            finish_reason,
        )
        .await?;
    Ok(CommittedTool {
        status,
        duration_ms,
        diff: outcome.diff,
        end_turn: outcome.control == ToolControl::EndTurn,
        observation,
        protocol_fault: None,
        result,
    })
}

pub(super) struct ToolBatchResult {
    pub control_end: bool,
    pub observations: Vec<(u128, bool, bool)>,
}

mod authorization;
use authorization::authorize;
use authorization::confirm_dangerous;
use authorization::denied_outcome;
mod batch;
pub(super) use batch::execute_tool_batch;

/// prefer_builtin 建议对应的内置工具在当前上下文是否可用：已注册、引擎/模式
/// 允许、且未被 preset 策略禁用。
fn builtin_available(ctx: &ToolCtx, name: &str) -> bool {
    ctx.catalog.get(name).is_some_and(|tool| {
        tool.permitted_in(ctx.execution_engine(), ctx.mode)
            && ctx
                .tool_policy
                .as_ref()
                .is_none_or(|policy| policy.permits(name))
    })
}

async fn execute_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    hooks: bool,
) -> Result<ToolOutcome, CoreError> {
    if ctx.cancel.is_cancelled() {
        return Ok(cancelled_outcome());
    }
    let name = call.name.as_str();
    let arguments = call.arguments.as_str();
    let config = &ctx.config;
    let request = &ctx.request;
    if !ctx.nested && name.starts_with("mcp__") {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "Call MCP tools through execute",
        ));
    }
    // 原生、execute 子调用与插件内部回调共享本轮工具快照。
    let Some(tool) = ctx.catalog.get(name) else {
        return Ok(ToolOutcome::failure(
            "E_TOOL_NOT_FOUND",
            format!("tool `{name}` is not registered"),
        ));
    };
    if !ctx.allow_agents && matches!(name, "spawn_agent" | "stop_agent" | "wait_agent") {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "Subagents cannot create or control other subagents",
        ));
    }
    // preset 快照策略：普通、嵌套（PTC）与批量路径共用这一道门，藏 schema 只是附带效果。
    if ctx
        .tool_policy
        .as_ref()
        .is_some_and(|policy| !policy.permits(name))
    {
        return Ok(ToolOutcome::failure(
            "E_TOOL_DISABLED",
            "This tool is not available under the active agent preset",
        ));
    }
    if name == "yield" && !ctx.subagent {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "yield is available only to subagents",
        ));
    }
    if !tool.permitted_in(ctx.execution_engine(), ctx.mode) {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "This tool is not allowed in the current mode",
        ));
    }
    if !crate::service::mode::tool_enabled(
        name,
        tool.origin(),
        inner.ask_available(
            request.connection_id(),
            ctx.state.lock().await.session.autonomy,
        ),
        config,
    ) {
        return Ok(ToolOutcome::failure(
            "E_TOOL_DISABLED",
            "This tool is disabled",
        ));
    }
    let mut args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(_) => {
            return Ok(ToolOutcome::failure(
                "E_TOOL_ARGUMENTS",
                format!(
                    "Tool arguments are not valid JSON: {}",
                    arguments.chars().take(200).collect::<String>(),
                ),
            ));
        }
    };
    // 模型发起的原生和 execute 子调用都应用目标工具钩子；插件内部回调不重入。
    // 改写后的参数才是沙盒、规则和工具真正看到的那份。
    if hooks
        && !ctx.plugins.is_empty()
        && let Some(denied) = plugin_before_tool_call(ctx, name, &mut args).await
    {
        return Ok(ToolOutcome {
            content: format!("[E_PLUGIN_DENIED] {denied}").into(),
            is_error: true,
            structured_content: None,
            error_code: Some("E_PLUGIN_DENIED".into()),
            images: Vec::new(),
            control: ToolControl::Continue,
            status: Some(ToolStatus::Denied),
            diff: None,
        });
    }
    // bash 执行前先过危险规则，并把命令里没加引号的内部 URI 换成 urlfs 目录里的路径：
    // 危险规则命中改为向用户申请确认（用户看到的是模型写的原命令），
    // 沙盒判定和审批提示看到的都是真实路径，URI 交给 shell 也不会悄悄失效。
    let mut history_view = false;
    if name == "bash"
        && let Some(command) = args.get("command").and_then(Value::as_str)
    {
        // 内置工具优先：只对 POSIX shell 生效；命令只是读/搜/列/改工作区文件时
        // 拒绝并给出等价调用。resolve_cwd 失败时跳过策略，让 bash 自己报错；
        // force_bash 为模型兜底逃生口，但危险命令规则照常执行。
        if args.get("force_bash").and_then(Value::as_bool) != Some(true)
            && config.tools.bash.prefer_builtin
            && matches!(
                crate::shell::detect().map(|choice| choice.kind),
                Some(
                    crate::shell::ShellKind::Bash
                        | crate::shell::ShellKind::Zsh
                        | crate::shell::ShellKind::Sh
                        | crate::shell::ShellKind::GitBash
                )
            )
            && let Ok(dir) =
                crate::tools::bash::resolve_cwd(&ctx.cwd, args.get("cwd").and_then(Value::as_str))
            && let Some(verdict) = crate::tools::bash::prefer_builtin_check(
                command,
                &ctx.cwd,
                &dir,
                args.get("stdin")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty()),
            )
            && verdict
                .suggestions
                .iter()
                .all(|suggestion| builtin_available(ctx, suggestion.tool))
        {
            return Ok(ToolOutcome::failure(
                "E_PREFER_BUILTIN",
                crate::tools::bash::render_prefer_builtin(&verdict),
            ));
        }
        match crate::tools::bash::denied_rule(&config.tools.bash.deny_patterns, command) {
            Err(error) => return Ok(error.into_outcome()),
            Ok(Some(rule)) if !ctx.sandbox_grants.read().dangerous_allowed(command) => {
                match confirm_dangerous(
                    ctx,
                    &call.id,
                    command,
                    &config.tools.bash.deny_patterns[rule],
                )
                .await?
                {
                    Authorization::Allowed(_) => {}
                    denied => return Ok(denied_outcome(denied)),
                }
            }
            Ok(_) => {}
        }
        match crate::tools::bash::urlfs::prepare(&ctx.paths, &ctx.skills, command) {
            Ok(Some(prepared)) => {
                history_view = prepared.history_view;
                args["command"] = Value::String(prepared.command);
            }
            Ok(None) => {}
            Err(error) => return Ok(error.into_outcome()),
        }
    }
    let execution = match authorize(ctx, &call.id, name, &args).await? {
        Authorization::Allowed(execution) => execution,
        denied => return Ok(denied_outcome(denied)),
    };
    if ctx.cancel.is_cancelled() {
        return Ok(cancelled_outcome());
    }
    let mut execution_ctx = ctx.clone();
    execution_ctx.permission = execution.permission;
    execution_ctx.call_grants = execution.call_grants;
    execution_ctx.tool_call_id = call.id.clone();
    if history_view
        && let Err(error) = crate::tools::bash::urlfs::refresh_history(
            &execution_ctx.paths,
            &execution_ctx.sandbox_policy(),
        )
    {
        return Ok(error.into_outcome());
    }
    let execution = AssertUnwindSafe(tool.run(&execution_ctx, args.clone())).catch_unwind();
    let result = if tool.parallel_safe() || name == "ask" {
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(cancelled_outcome()),
            result = execution => result,
        }
    } else {
        // 写入事务必须完整提交，不能中途取消；进程类工具自己响应取消并回收进程组。
        execution.await
    };
    match result {
        Ok(Ok(mut outcome)) => {
            if outcome.diff.is_some() {
                ctx.guard.record_change();
            }
            if hooks && !ctx.plugins.is_empty() {
                let result = moc_plugin::ToolResult {
                    content: match &outcome.content {
                        crate::tools::ToolOutput::Text(text)
                        | crate::tools::ToolOutput::Verbatim(text) => text.clone(),
                        crate::tools::ToolOutput::File { path, .. } => path.display().to_string(),
                        crate::tools::ToolOutput::Excerpt { text, .. } => text.clone(),
                    },
                    structured: outcome.structured_content.clone(),
                    is_error: outcome.is_error,
                };
                // 改写必须发生在归档、落库、回灌模型与推前端之前：四处看到的是同一份结果。
                let rewrite = ctx
                    .plugins
                    .after_tool_call(name, &args, &result, &ctx.plugin_call_context())
                    .await;
                if let Some(content) = rewrite.content {
                    outcome.content = if let crate::tools::ToolOutput::Verbatim(_) = outcome.content
                    {
                        crate::tools::ToolOutput::Verbatim(content)
                    } else {
                        crate::tools::ToolOutput::Text(content)
                    };
                }
                if let Some(structured) = rewrite.structured {
                    outcome.structured_content = (!structured.is_null()).then_some(structured);
                }
            }
            Ok(outcome)
        }
        Ok(Err(error)) if error.code == "E_UPDATE" => Err(CoreError::Protocol(error.to_string())),
        Ok(Err(error)) => Ok(error.into_outcome()),
        Err(_) => Ok(ToolOutcome::failure(
            "E_TOOL_PANIC",
            format!("Tool {name} panicked during execution"),
        )),
    }
}

/// 执行 execute 或插件内部发起的子调用；模型发起的调用保留完整工具钩子。
pub(crate) async fn execute_nested_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    from_model: bool,
) -> Result<ToolOutcome, CoreError> {
    if call.name == "execute" {
        return Ok(ToolOutcome::failure(
            "E_PTC_RECURSION",
            "Nested execute calls are not allowed",
        ));
    }
    let mut nested = ctx.clone();
    nested.nested = true;
    nested.call_grants = crate::sandbox::SandboxGrants::default();
    // 一次性的 bash 升级只属于那次调用，不能泄漏给 PTC 嵌套工具。
    nested.permission = ctx.state.lock().await.permission;
    execute_tool(inner, &nested, call, from_model).await
}

/// 调用全部插件的 `beforeToolCall`；拒绝时返回原因，允许时就地改写参数。
async fn plugin_before_tool_call(ctx: &ToolCtx, name: &str, args: &mut Value) -> Option<String> {
    match ctx
        .plugins
        .before_tool_call(name, args, &ctx.plugin_call_context())
        .await
    {
        moc_plugin::BeforeToolCall::Allow(rewritten) => {
            *args = rewritten;
            None
        }
        moc_plugin::BeforeToolCall::Deny(reason) => Some(reason),
    }
}

enum Authorization {
    Allowed(AllowedExecution),
    /// 用户拒绝了本次操作。
    Denied,
    /// 沙盒策略硬拒绝；不发起权限询问。
    SandboxDenied(String),
    /// 前端不支持权限询问，拿不到一次性授权。
    Unavailable,
}

struct AllowedExecution {
    permission: moc_protocol::SessionPermission,
    call_grants: crate::sandbox::SandboxGrants,
}

impl AllowedExecution {
    fn current(ctx: &ToolCtx) -> Self {
        Self {
            permission: ctx.permission,
            call_grants: crate::sandbox::SandboxGrants::default(),
        }
    }
}

/// 权限弹窗中写入内容预览的行数与字节上限。
const WRITE_PREVIEW_LINES: usize = 40;
const WRITE_PREVIEW_BYTES: usize = 4096;

enum PermissionAnswer {
    Unavailable,
    Selected(String),
    Dismissed,
}

/// 提示被取消时，为「模型请求过、但这批工具还没开始跑」的调用补的结果正文。
///
/// `/retry` 靠这句原文区分「确实没执行」（可以安全重放）与「执行到一半被打断」
/// （结果未知，只能让模型重新决定），所以改文案会改变重试行为。
pub(super) const NOT_EXECUTED: &str = "Prompt cancelled; this call was not executed.";

pub(super) async fn persist_tool_result(
    inner: &CoreInner,
    session_id: &str,
    tool_call_id: String,
    content: String,
    is_error: bool,
    status: ToolStatus,
    duration_ms: u64,
) -> Result<(), CoreError> {
    inner
        .sessions
        .append_message(
            session_id,
            Message::tool_result(tool_call_id.clone(), content, is_error),
            Some(ToolExecution {
                tool_call_id,
                status,
                duration_ms: Some(duration_ms),
                diff: None,
            }),
            None,
        )
        .await?;
    Ok(())
}

fn cancelled_outcome() -> ToolOutcome {
    let mut outcome = ToolOutcome::failure(
        "E_CANCELLED",
        "Prompt cancelled; this call was not executed or was terminated.",
    );
    outcome.status = Some(ToolStatus::Cancelled);
    outcome
}

/// 恢复时给没有记录的调用补结果。
///
/// 进程可能在工具已经产生副作用、但结果还没提交时退出，所以这里不能声称“未执行”：
/// 模型必须先去核实外部状态，而不是把非幂等操作直接重跑一遍。
fn unknown_outcome() -> ToolOutcome {
    let mut outcome = ToolOutcome::failure(
        "E_OUTCOME_UNKNOWN",
        "Crash recovery: this call has no recorded result. Its side effects are unknown; \
         verify external state before retrying non-idempotent operations.",
    );
    outcome.status = Some(ToolStatus::Cancelled);
    outcome
}

pub(super) async fn repair_pending_calls(
    inner: &CoreInner,
    session_id: &str,
    record: &crate::session::SessionRecord,
) -> Result<(), CoreError> {
    let pending = {
        let state = record.state.lock().await;
        let mut pending = Vec::new();
        for entry in state.transcript.iter() {
            pending.extend(entry.message.content.tool_call_ids().map(str::to_owned));
            for result_id in entry.message.content.tool_result_ids() {
                pending.retain(|id| id != result_id);
            }
        }
        pending
    };
    for id in pending {
        let ToolOutput::Text(content) = unknown_outcome().content else {
            unreachable!("未知结果必须是文本");
        };
        persist_tool_result(
            inner,
            session_id,
            id,
            content,
            true,
            ToolStatus::Cancelled,
            0,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
