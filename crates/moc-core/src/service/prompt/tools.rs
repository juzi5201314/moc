//! 执行工具批次：并发上限、授权、取消，以及把结果与调用配对。

use super::workspace::{WorkspaceScope, check_workspace};
use crate::{
    error::CoreError,
    service::{CoreInner, SessionRuntime, protocol_to_rpc, snapshot_workspace, to_rpc},
    tools::{ToolControl, ToolCtx, ToolOutcome, ToolOutput},
};
use futures_util::{
    FutureExt,
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use moc_llm::{ContentPart, Message, Role};
use moc_protocol::{
    RpcError, RpcErrorCode,
    methods::{SessionUpdate, ToolExecution, ToolStatus},
};
use serde_json::Value;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Instant};
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
    mode: moc_protocol::methods::SessionMode,
    call: &moc_llm::ToolCall,
    start: Instant,
) -> Result<CommittedTool, CoreError> {
    let (outcome, protocol_fault) = match execute_tool(inner, ctx, mode, call).await {
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

/// 工具执行本身失败（例如未注册）时也留下配对结果，错误文本进入模型可见内容。
async fn commit_failure(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
    error: CoreError,
    duration_ms: u64,
) -> Result<CommittedTool, CoreError> {
    let outcome = ToolOutcome::failure("E_TOOL", error.to_string());
    commit_outcome(inner, ctx, call, outcome, duration_ms).await
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
    inner
        .sessions
        .append_message(
            &ctx.session_id,
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult(result.clone())],
            },
            Some(ToolExecution {
                tool_call_id: call.id.clone(),
                status,
                duration_ms: Some(duration_ms),
            }),
            None,
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

pub(super) async fn execute_tool_batch(
    inner: &CoreInner,
    tool_ctx: &ToolCtx,
    runtime: &Arc<SessionRuntime>,
    record: &Arc<crate::session::SessionRecord>,
    mode: moc_protocol::methods::SessionMode,
    calls: &[moc_llm::ToolCall],
) -> Result<ToolBatchResult, RpcError> {
    let config = &tool_ctx.config;
    let parallel_limit = config.tools.max_concurrent_calls;
    debug_assert!(parallel_limit > 0);
    let session_id = tool_ctx.session_id.as_str();
    let prompt_id = tool_ctx.prompt_id.as_str();
    let cwd = &tool_ctx.cwd;
    let update = tool_ctx.request.updates();
    let mut observations = Vec::with_capacity(calls.len());
    let progress_before = tool_ctx.guard.progress();
    if calls.len() > 1 {
        update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolBatchStarted {
                    prompt_id: prompt_id.to_owned(),
                    batch_id: crate::ids::entry_id(),
                    calls: calls
                        .iter()
                        .map(|call| moc_protocol::methods::ToolBatchCall {
                            tool_call_id: call.id.clone(),
                            name: call.name.clone(),
                        })
                        .collect(),
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    let tool_batch_before = if matches!(
        mode,
        moc_protocol::methods::SessionMode::Normal | moc_protocol::methods::SessionMode::Ptc
    ) && config.workspace.watch
    {
        Some(
            snapshot_workspace(cwd.clone(), config.workspace.max_scan_entries)
                .await
                .map_err(to_rpc)?,
        )
    } else {
        None
    };
    // 每一步在自身完成时就提交到磁盘：并行批次里先完成的工具不再等慢工具，
    // 崩溃最多丢失尚未完成的那一步。完整结果只在通知前端时短暂保留。
    let mut slots: Vec<Option<CommittedTool>> =
        std::iter::repeat_with(|| None).take(calls.len()).collect();
    let mut started = vec![false; calls.len()];
    let mut pending: FuturesUnordered<PendingTool<'_>> = FuturesUnordered::new();
    let mut stop_reason = None;

    'calls: for (index, call) in calls.iter().enumerate() {
        if call.name.starts_with("mcp__") {
            let outcome = ToolOutcome::failure("E_MODE", "Call MCP tools through run_code");
            slots[index] = Some(
                commit_outcome(inner, tool_ctx, call, outcome, 0)
                    .await
                    .map_err(to_rpc)?,
            );
            continue;
        }
        let Some(tool) = inner.tools.get(&call.name) else {
            let error = CoreError::Tool(format!(
                "E_TOOL_NOT_FOUND: tool `{}` is not registered",
                call.name
            ));
            slots[index] = Some(
                commit_failure(inner, tool_ctx, call, error, 0)
                    .await
                    .map_err(to_rpc)?,
            );
            continue;
        };
        let parallel = tool.parallel_safe();

        if !parallel {
            while let Some((done, committed)) = pending.next().await {
                let committed = committed.map_err(to_rpc)?;
                note_protocol_fault(&mut stop_reason, &committed);
                slots[done] = Some(committed);
            }
        }
        started[index] = true;
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolStarted {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    parent_tool_call_id: None,
                    arguments: call.arguments.clone(),
                },
            )
            .await
        {
            stop_reason = Some(protocol_to_rpc(error));
            break 'calls;
        }
        let start = Instant::now();
        if parallel {
            pending.push(Box::pin(async move {
                (index, run_step(inner, tool_ctx, mode, call, start).await)
            }));
            if pending.len() == parallel_limit
                && let Some((done, committed)) = pending.next().await
            {
                let committed = committed.map_err(to_rpc)?;
                note_protocol_fault(&mut stop_reason, &committed);
                slots[done] = Some(committed);
            }
        } else {
            let committed = run_step(inner, tool_ctx, mode, call, start)
                .await
                .map_err(to_rpc)?;
            note_protocol_fault(&mut stop_reason, &committed);
            let end_turn = committed.end_turn;
            slots[index] = Some(committed);
            if stop_reason.is_some() || end_turn {
                break 'calls;
            }
        }
    }

    if stop_reason.is_none() {
        while let Some((done, committed)) = pending.next().await {
            let committed = committed.map_err(to_rpc)?;
            note_protocol_fault(&mut stop_reason, &committed);
            slots[done] = Some(committed);
        }
    }

    let mut end_prompt = false;
    for (index, call) in calls.iter().enumerate() {
        if !started[index]
            && stop_reason.is_none()
            && let Err(error) = update
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::ToolStarted {
                        prompt_id: prompt_id.to_owned(),
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        parent_tool_call_id: None,
                        arguments: call.arguments.clone(),
                    },
                )
                .await
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        // 没执行到的调用在这里补一条结果并提交，保证每个工具调用都有配对结果。
        let committed = match slots[index].take() {
            Some(committed) => committed,
            None => {
                let (code, message) = if stop_reason.is_some() {
                    ("E_CANCELLED", "Prompt aborted; this call was not executed.")
                } else {
                    (
                        "E_TURN_ENDED",
                        "The plan was submitted; later calls were not executed.",
                    )
                };
                let mut outcome = ToolOutcome::failure(code, message);
                outcome.status = Some(ToolStatus::Cancelled);
                commit_outcome(inner, tool_ctx, call, outcome, 0)
                    .await
                    .map_err(to_rpc)?
            }
        };
        if let Some(observation) = committed.observation {
            observations.push(observation);
        }
        let CommittedTool {
            status,
            duration_ms,
            diff,
            end_turn,
            result: tool_result,
            ..
        } = committed;
        if mode == moc_protocol::methods::SessionMode::ClmExperimental {
            let message = Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult(tool_result.clone())],
            };
            crate::tools::clm::append_message(
                &inner.sessions,
                &tool_ctx.state,
                session_id,
                &message,
            )
            .await
            .map_err(to_rpc)?;
        }
        if config.openviking.capture
            && let Some(client) = tool_ctx.openviking.as_ref()
            && let Some(peer) = tool_ctx.openviking_peer.as_deref()
        {
            let session = format!(
                "moc-{}",
                session_id.replace(
                    |character: char| !character.is_ascii_alphanumeric()
                        && character != '-'
                        && character != '_',
                    "-"
                )
            );
            let message = serde_json::json!({"role":"tool","tool_call_id":call.id,"tool_name":call.name,"parts":[{"type":"text","text":tool_result.content.chars().take(4096).collect::<String>()}]});
            let _ = client.append_messages(&session, &[message], peer).await;
        }
        // todo 停滞提醒的计数：除 todo 外，每次工具调用都加一。
        if call.name != "todo" {
            tool_ctx.state.lock().await.tool_calls_since_todo += 1;
        }
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolFinished {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    parent_tool_call_id: None,
                    arguments: call.arguments.clone(),
                    status,
                    result: tool_result,
                    duration_ms: Some(duration_ms),
                    diff,
                },
            )
            .await
            && stop_reason.is_none()
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        end_prompt |= end_turn;
    }

    check_workspace(
        &update,
        runtime,
        record,
        WorkspaceScope {
            session_id,
            cwd,
            config,
            mode,
            before_tools: tool_batch_before.as_ref(),
        },
    )
    .await?;
    if let Some(error) = stop_reason {
        return Err(error);
    }
    if tool_ctx.guard.progress() != progress_before {
        for (_, changed, _) in &mut observations {
            *changed = true;
        }
    }
    // 全是只读调用的批次不必在意完成顺序；含写入屏障的批次必须按顺序执行。
    if calls.iter().all(|call| {
        inner
            .tools
            .get(&call.name)
            .is_some_and(|tool| tool.parallel_safe())
    }) {
        observations.sort_unstable_by_key(|(fingerprint, _, _)| *fingerprint);
    }
    Ok(ToolBatchResult {
        control_end: end_prompt,
        observations,
    })
}
async fn execute_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    mode: moc_protocol::methods::SessionMode,
    call: &moc_llm::ToolCall,
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
            "Call MCP tools through run_code",
        ));
    }
    let selected = if ctx.nested {
        ctx.catalog.get(name)
    } else {
        inner.tools.get(name)
    };
    let Some(tool) = selected else {
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
    if name == "yield" && !ctx.subagent {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "yield is available only to subagents",
        ));
    }
    if !crate::service::mode::mode_allows(mode, name, tool.tier()) {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "This tool is not allowed in the current mode",
        ));
    }
    if !crate::service::mode::tool_enabled(
        name,
        inner.client_supports_ask(request.connection_id()),
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
    // bash 执行前先过静态规则，并把命令里没加引号的内部 URI 换成 urlfs 目录里的路径：
    // 沙盒判定和审批提示看到的都是真实路径，URI 交给 shell 也不会悄悄失效。
    if name == "bash"
        && let Some(command) = args.get("command").and_then(Value::as_str)
    {
        if let Err(error) =
            crate::tools::bash::reject_denied(&config.tools.bash.deny_patterns, command)
        {
            return Ok(error.into_outcome());
        }
        match crate::tools::bash::urlfs::prepare(&ctx.paths, &ctx.skills, command) {
            Ok(Some(rewritten)) => args["command"] = Value::String(rewritten),
            Ok(None) => {}
            Err(error) => return Ok(error.into_outcome()),
        }
    }
    let execution = match authorize(ctx, &call.id, name, &args).await? {
        Authorization::Denied => {
            return Ok(ToolOutcome {
                content: "[E_PERMISSION_DENIED] The user denied this operation".into(),
                is_error: true,
                structured_content: None,
                error_code: Some("E_PERMISSION_DENIED".into()),
                images: Vec::new(),
                control: ToolControl::Continue,
                status: Some(ToolStatus::Denied),
                diff: None,
            });
        }
        Authorization::SandboxDenied(message) => {
            return Ok(ToolOutcome {
                content: format!("[E_SANDBOX_DENIED] {message}").into(),
                is_error: true,
                structured_content: None,
                error_code: Some("E_SANDBOX_DENIED".into()),
                images: Vec::new(),
                control: ToolControl::Continue,
                status: Some(ToolStatus::Denied),
                diff: None,
            });
        }
        Authorization::Unavailable => {
            return Ok(ToolOutcome {
                content: "[E_PERMISSION_UNAVAILABLE] This frontend cannot show permission prompts; approve from an interactive frontend or switch the session permission".into(),
                is_error: true,
                structured_content: None,
                error_code: Some("E_PERMISSION_UNAVAILABLE".into()),
                images: Vec::new(),
                control: ToolControl::Continue,
                status: Some(ToolStatus::Denied),
                diff: None,
            });
        }
        Authorization::Allowed(execution) => execution,
    };
    if ctx.cancel.is_cancelled() {
        return Ok(cancelled_outcome());
    }
    let mut execution_ctx = ctx.clone();
    execution_ctx.permission = execution.permission;
    execution_ctx.call_grants = execution.call_grants;
    execution_ctx.tool_call_id = call.id.clone();
    let execution = AssertUnwindSafe(tool.run(&execution_ctx, args)).catch_unwind();
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
        Ok(Ok(outcome)) => {
            if outcome.diff.is_some() {
                ctx.guard.record_change();
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

/// 执行 PTC 脚本发起的子工具调用：沿用当前提示的权限，结果不写入模型消息。
pub(crate) async fn execute_nested_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
) -> Result<ToolOutcome, CoreError> {
    if call.name == "run_code" {
        return Ok(ToolOutcome::failure(
            "E_PTC_RECURSION",
            "Nested run_code calls are not allowed",
        ));
    }
    let mut nested = ctx.clone();
    nested.nested = true;
    nested.call_grants = crate::sandbox::SandboxGrants::default();
    // 一次性的 bash 升级只属于那次调用，不能泄漏给 PTC 嵌套工具。
    nested.permission = ctx.state.lock().await.permission;
    let mode = if ctx.mode == moc_protocol::SessionMode::Ptc {
        moc_protocol::SessionMode::Normal
    } else {
        ctx.mode
    };
    execute_tool(inner, &nested, mode, call).await
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

/// 授权入口：`full` 不询问（yolo）；其余模式按沙盒策略检查读/写路径，bash 的
/// 升级由模型显式带 `sandbox_permissions` + `justification` 发起。
async fn authorize(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    let policy = ctx.sandbox_policy();
    match tool_name {
        "bash" => authorize_bash(ctx, tool_call_id, arguments).await,
        "read" | "find" | "grep" => {
            authorize_read(ctx, tool_call_id, tool_name, arguments, &policy).await
        }
        "write" | "edit" => authorize_write(ctx, tool_call_id, tool_name, arguments, &policy).await,
        _ => Ok(Authorization::Allowed(AllowedExecution::current(ctx))),
    }
}

async fn authorize_read(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<Authorization, CoreError> {
    let Some(target) = arguments.get("path").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    if crate::resources::is_resource_uri(target) {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    let path = absolute_target(&ctx.cwd, std::path::Path::new(target));
    match policy.check(&path, crate::sandbox::Access::Read) {
        crate::sandbox::Decision::Allow => {
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        crate::sandbox::Decision::Deny => Ok(Authorization::SandboxDenied(format!(
            "read access to {} is denied by sandbox.deny_read_paths",
            path.display()
        ))),
        crate::sandbox::Decision::Ask => {
            request_paths(
                ctx,
                tool_call_id,
                tool_name,
                vec![path],
                crate::sandbox::Access::Read,
                format!("允许在沙盒外读取：{target}"),
                None,
                None,
            )
            .await
        }
    }
}

fn absolute_target(cwd: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd.join(target)
    }
}

/// 找出 write/edit 的全部目标；解析失败留给工具自己返回 `E_BAD_PAYLOAD`。
fn write_targets(
    cwd: &std::path::Path,
    tool_name: &str,
    arguments: &Value,
) -> Result<Vec<std::path::PathBuf>, crate::tools::ToolError> {
    let targets: Vec<String> = if tool_name == "write" {
        vec![crate::tools::write::parse_args(arguments.clone())?.path]
    } else {
        let request = crate::tools::edit::parse_requests(arguments.clone())?;
        std::iter::once(request.path)
            .chain(
                request
                    .move_lines
                    .into_iter()
                    .map(|movement| movement.target_path),
            )
            .collect()
    };
    Ok(targets
        .into_iter()
        .filter(|target| !crate::resources::is_resource_uri(target))
        .map(|target| absolute_target(cwd, std::path::Path::new(&target)))
        .collect())
}

async fn authorize_write(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<Authorization, CoreError> {
    let targets = match write_targets(&ctx.cwd, tool_name, arguments) {
        Ok(targets) => targets,
        Err(_) => return Ok(Authorization::Allowed(AllowedExecution::current(ctx))),
    };
    let mut outside = Vec::new();
    for target in targets {
        match policy.check(&target, crate::sandbox::Access::Write) {
            crate::sandbox::Decision::Allow => {}
            crate::sandbox::Decision::Deny => {
                return Ok(Authorization::SandboxDenied(format!(
                    "write access to {} is denied by sandbox.deny_write_paths",
                    target.display()
                )));
            }
            crate::sandbox::Decision::Ask => outside.push(target),
        }
    }
    if outside.is_empty() {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    let paths = outside
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let (diff, content) = match tool_name {
        "write" => match crate::tools::write::parse_args(arguments.clone()) {
            Ok(args) => (
                crate::tools::write::preview_arguments(ctx, &args).await,
                Some(args.content),
            ),
            Err(_) => (None, None),
        },
        _ => (crate::tools::edit::preview_arguments(ctx, arguments), None),
    };
    let mut body = format!(
        "以下写入目标在沙盒允许范围之外：\n{paths}\n\n当前权限：{}。允许后本次写入会真实落盘。",
        ctx.permission.as_str()
    );
    if let Some(content) = &content {
        body.push_str(&format!(
            "\n\n将写入的内容（前 {WRITE_PREVIEW_LINES} 行，共 {} 字节）：\n{}",
            content.len(),
            write_preview(content)
        ));
    }
    let title = format!(
        "允许在工作区外写入：{}",
        outside
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    request_paths(
        ctx,
        tool_call_id,
        tool_name,
        outside,
        crate::sandbox::Access::Write,
        title,
        Some(body),
        diff,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn request_paths(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    targets: Vec<std::path::PathBuf>,
    access: crate::sandbox::Access,
    title: String,
    body: Option<String>,
    diff: Option<moc_protocol::methods::ToolDiff>,
) -> Result<Authorization, CoreError> {
    if ctx.config.sandbox.on_denied == crate::config::OnDenied::Deny {
        return Ok(Authorization::SandboxDenied(format!(
            "sandbox.on_denied=deny；已拒绝以下路径的访问：\n{}",
            targets
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    match ask_permission(
        ctx,
        tool_call_id,
        tool_name,
        title,
        body,
        diff,
        moc_protocol::methods::path_options(),
    )
    .await?
    {
        PermissionAnswer::Unavailable => Ok(Authorization::Unavailable),
        PermissionAnswer::Dismissed => Ok(Authorization::Denied),
        PermissionAnswer::Selected(option) if option == "allow_once" => {
            let mut grants = crate::sandbox::SandboxGrants::default();
            for target in &targets {
                grants.grant_path(target, access);
            }
            Ok(Authorization::Allowed(AllowedExecution {
                permission: ctx.permission,
                call_grants: grants,
            }))
        }
        PermissionAnswer::Selected(option) if option == "allow_session" => {
            let mut grants = ctx.sandbox_grants.write();
            for target in &targets {
                grants.grant_path(target, access);
            }
            Ok(Authorization::Allowed(AllowedExecution::current(ctx)))
        }
        PermissionAnswer::Selected(_) => Ok(Authorization::Denied),
    }
}

async fn authorize_bash(
    ctx: &ToolCtx,
    tool_call_id: &str,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    let target = arguments
        .get("sandbox_permissions")
        .and_then(Value::as_str)
        .and_then(parse_permission);
    let Some(target) = target else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    let Some(justification) = arguments.get("justification").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    };
    if justification.trim().is_empty() || justification.len() > 500 {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    // 非法或变窄的目标交给 bash 工具返回 E_BAD_PAYLOAD，不在授权层重复报错。
    if target == moc_protocol::SessionPermission::WorkspaceOnly
        || target.width() <= ctx.permission.width()
    {
        return Ok(Authorization::Allowed(AllowedExecution::current(ctx)));
    }
    if ctx.config.sandbox.on_denied == crate::config::OnDenied::Deny {
        return Ok(Authorization::SandboxDenied(format!(
            "sandbox.on_denied=deny；命令不能升级到 {} 执行",
            target.as_str()
        )));
    }
    if target == moc_protocol::SessionPermission::Full && !ctx.config.sandbox.allow_unsandboxed {
        return Ok(Authorization::SandboxDenied(
            "sandbox.allow_unsandboxed=false；不能申请沙盒外执行".into(),
        ));
    }
    if let Some(granted) = ctx.sandbox_grants.read().mode_for_command(command)
        && granted.width() >= target.width()
    {
        return Ok(Authorization::Allowed(AllowedExecution {
            permission: target,
            call_grants: crate::sandbox::SandboxGrants::default(),
        }));
    }

    let prefix = crate::sandbox::command_prefix(command);
    let body = format!(
        "命令：\n{command}\n\n理由：\n{justification}\n\n当前权限：{}；申请权限：{}。",
        ctx.permission.as_str(),
        target.as_str()
    );
    match ask_permission(
        ctx,
        tool_call_id,
        "bash",
        format!(
            "允许以 {} 执行命令：{}",
            target.as_str(),
            command.lines().next().unwrap_or(command)
        ),
        Some(body),
        None,
        moc_protocol::methods::command_options(prefix.as_deref()),
    )
    .await?
    {
        PermissionAnswer::Unavailable => Ok(Authorization::Unavailable),
        PermissionAnswer::Dismissed => Ok(Authorization::Denied),
        PermissionAnswer::Selected(option) if option == "allow_once" => {
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(option) if option == "allow_session" => {
            ctx.sandbox_grants
                .write()
                .grant_command(command, false, target);
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(option)
            if option == "allow_session_prefix" && prefix.is_some() =>
        {
            ctx.sandbox_grants.write().grant_command(
                prefix.as_deref().unwrap_or_default(),
                true,
                target,
            );
            Ok(Authorization::Allowed(AllowedExecution {
                permission: target,
                call_grants: crate::sandbox::SandboxGrants::default(),
            }))
        }
        PermissionAnswer::Selected(_) => Ok(Authorization::Denied),
    }
}

fn parse_permission(value: &str) -> Option<moc_protocol::SessionPermission> {
    match value {
        "full" => Some(moc_protocol::SessionPermission::Full),
        "workspace" => Some(moc_protocol::SessionPermission::Workspace),
        "workspace_only" | "workspace-only" => Some(moc_protocol::SessionPermission::WorkspaceOnly),
        _ => None,
    }
}

/// 权限弹窗中写入内容预览的行数与字节上限。
const WRITE_PREVIEW_LINES: usize = 40;
const WRITE_PREVIEW_BYTES: usize = 4096;

/// 生成写入内容预览：取开头若干行，超出上限时注明已截断。
fn write_preview(content: &str) -> String {
    let mut preview = String::new();
    let mut bytes = 0usize;
    for (line_index, line) in content.lines().enumerate() {
        let width = line.len() + 1;
        if line_index >= WRITE_PREVIEW_LINES || bytes + width > WRITE_PREVIEW_BYTES {
            preview.push_str("\n[...]");
            break;
        }
        preview.push_str(line);
        preview.push('\n');
        bytes += width;
    }
    preview
}

enum PermissionAnswer {
    Unavailable,
    Selected(String),
    Dismissed,
}

/// 发起一次权限询问；前端不支持时返回 Unavailable，用户关闭弹窗按 Dismissed。
async fn ask_permission(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    title: String,
    body: Option<String>,
    diff: Option<moc_protocol::methods::ToolDiff>,
    options: Vec<moc_protocol::methods::PermissionOption>,
) -> Result<PermissionAnswer, CoreError> {
    let request = &ctx.request;
    if !ctx
        .core
        .capabilities
        .read()
        .get(&request.connection_id())
        .is_some_and(|caps| caps.permission_requests)
    {
        return Ok(PermissionAnswer::Unavailable);
    }
    let request_id = crate::ids::entry_id();
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionRequested {
                prompt_id: ctx.prompt_id.clone(),
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.to_owned(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    let answer = request
        .ask_permission(moc_protocol::methods::PermissionRequestParams {
            session_id: ctx.session_id.clone(),
            prompt_id: ctx.prompt_id.clone(),
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            title,
            body,
            diff,
            options,
        })
        .await;
    let selected = answer.ok().map(|answer| answer.option_id);
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionResolved {
                prompt_id: ctx.prompt_id.clone(),
                request_id,
                option_id: selected.clone(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    Ok(match selected {
        None => PermissionAnswer::Dismissed,
        Some(option) => PermissionAnswer::Selected(option),
    })
}

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
        for entry in &state.transcript {
            pending.extend(entry.message.tool_calls().map(|call| call.id.clone()));
            for result in entry.message.tool_results() {
                pending.retain(|id| id != &result.tool_call_id);
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
mod tests {
    use super::*;

    fn workspace(directory: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = directory.path().join("work");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (
            root.canonicalize().unwrap(),
            outside.canonicalize().unwrap(),
        )
    }

    fn outside_workspace_targets(
        cwd: &std::path::Path,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, crate::tools::ToolError> {
        Ok(write_targets(cwd, tool_name, arguments)?
            .into_iter()
            .filter(|path| crate::tools::workspace_escape(cwd, path).is_some())
            .collect())
    }

    #[test]
    fn writes_inside_the_workspace_need_no_approval() {
        let directory = tempfile::tempdir().unwrap();
        let (root, _) = workspace(&directory);
        for arguments in [
            serde_json::json!({"path":"src/main.rs","content":"x"}),
            serde_json::json!({"path":root.join("nested/new.txt").display().to_string(),"content":"x"}),
        ] {
            assert!(
                outside_workspace_targets(&root, "write", &arguments)
                    .unwrap()
                    .is_empty(),
                "{arguments}"
            );
        }
        let edit = serde_json::json!({
            "path":"src/main.rs",
            "edits":[["1aa","1aa","change"]]
        });
        assert!(
            outside_workspace_targets(&root, "edit", &edit)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn write_and_move_targets_outside_the_workspace_require_approval() {
        let directory = tempfile::tempdir().unwrap();
        let (root, outside) = workspace(&directory);
        let write = serde_json::json!({
            "path": outside.join("result.txt").display().to_string(),
            "content": "x"
        });
        assert_eq!(
            outside_workspace_targets(&root, "write", &write).unwrap(),
            vec![outside.join("result.txt")]
        );
        // move 的 target_path 与源文件一起授权，越出工作区的目标也必须检查出来。
        let edit = serde_json::json!({
            "path": "src/main.rs",
            "edits": [["1aa","1aa","change"]],
            "move": {"from":"1aa","to":"1aa","target_path": outside.join("moved.txt").display().to_string()}
        });
        assert_eq!(
            outside_workspace_targets(&root, "edit", &edit).unwrap(),
            vec![outside.join("moved.txt")]
        );
    }

    #[test]
    fn write_preview_carries_content_and_marks_truncation() {
        let body = write_preview("first\nsecond");
        assert!(body.contains("first\nsecond"));
        let long = (0..WRITE_PREVIEW_LINES + 5)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = write_preview(&long);
        assert!(preview.contains("line 0") && !preview.contains("line 44"));
        assert!(preview.ends_with("[...]"));
        let wide = "x".repeat(WRITE_PREVIEW_BYTES + 1);
        assert!(write_preview(&wide).ends_with("[...]"));
    }

    #[test]
    fn malformed_write_arguments_surface_the_tool_error() {
        let directory = tempfile::tempdir().unwrap();
        let (root, _) = workspace(&directory);
        let error =
            outside_workspace_targets(&root, "edit", &serde_json::json!({"files":[]})).unwrap_err();
        assert_eq!(error.code, "E_BAD_PAYLOAD");
    }
}
