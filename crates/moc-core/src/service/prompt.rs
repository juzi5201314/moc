//! 提示的流式编排、取消后的清理、持久化与工具执行。

use crate::{
    compact,
    compact::replay::model_messages,
    service::{CoreInner, flush_plugin_notices, protocol_to_rpc, snapshot_workspace, to_rpc},
    session::store::now_ms,
    system_prompt::{build, merge_reminders, mode_contract, reminder_message, sandbox_contract},
    tools::ToolCtx,
};
use moc_llm::{FinishReason, Message, Usage};
use moc_protocol::{
    RequestContext, RpcError, RpcErrorCode,
    methods::{PromptResult, SessionUpdate, ToolStatus},
};
use serde_json::Value;
use std::{sync::Arc, time::Instant};

pub(crate) mod guard;
pub(crate) mod request;
mod stream;
pub(crate) mod tools;
mod workspace;

use guard::{CycleAction, PromptGuard, ToolCycles};
use request::{CacheAnchors, build_request, project_input, upstream_mismatches};
use stream::forward_event;
use tools::{execute_tool_batch, persist_tool_result, repair_pending_calls};
use workspace::{WorkspaceScope, check_workspace};

pub async fn run_prompt(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
) -> Result<PromptResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let prompt = record.try_prompt_lock().map_err(to_rpc)?;
    run_prompt_with_lock(inner, ctx, params, record, prompt).await
}

/// 用调用方已经拿到的提示锁跑一次提示。
///
/// 代理唤醒会在派发前先抢提示锁：抢不到说明该会话还有提示没结束，调用方就把消息
/// 放进收件箱，而不是和那个提示争同一个会话。
pub(crate) async fn run_prompt_with_lock(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
    prompt: tokio::sync::OwnedMutexGuard<()>,
) -> Result<PromptResult, RpcError> {
    let session_id = params.session_id.clone();
    let _prompt = prompt;
    let _epoch = inner.config_epoch.read().await;
    let started = Instant::now();
    inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let update = ctx.updates();
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::LockChanged {
                lock: inner.sessions.lease_info(&params.session_id, &record),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    repair_pending_calls(&inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    let runtime = inner.runtime(&session_id).await;
    runtime.steering.lock().await.accepting = true;
    runtime.agent_inbox.lock().await.accepting = true;
    let parent_id = record.state.lock().await.session.parent_session_id.clone();
    let parent_guard = match parent_id {
        Some(parent_id) => inner.runtime(&parent_id).await.guard.lock().clone(),
        None => None,
    };
    let guard = PromptGuard::new(ctx.cancel().child_token(), parent_guard);
    *runtime.guard.lock() = Some(Arc::clone(&guard));
    let scoped_ctx = ctx.with_cancel(guard.cancel.clone());
    let result =
        run_prompt_locked(Arc::clone(&inner), &scoped_ctx, params, Arc::clone(&record)).await;
    let result = if let Some(violation) = guard.violation() {
        async {
            let prompt_id = match &result {
                Ok(result) => result.prompt_id.clone(),
                Err(_) => crate::ids::prompt_id(record.state.lock().await.prompt_sequence),
            };
            let reason = FinishReason::LoopDetected;
            let message = Message::assistant_text(format!(
                "已停止本次提示：{violation}。已完成的工具操作会保留，不会自动重试。"
            ));
            inner
                .sessions
                .append_message(&session_id, message.clone(), None, Some(reason.clone()))
                .await
                .map_err(to_rpc)?;
            update
                .emit(
                    session_id.clone(),
                    SessionUpdate::GuardNotice {
                        prompt_id: prompt_id.clone(),
                        message: message.text(),
                        terminal: true,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
            let usage = result
                .as_ref()
                .map_or(Usage::default(), |result| result.usage);
            Ok(PromptResult {
                session_id: session_id.clone(),
                prompt_id,
                message,
                finish_reason: reason,
                usage,
            })
        }
        .await
    } else {
        result
    };
    *runtime.guard.lock() = None;
    // 推送失败只影响前端能否看到，不能跳过后面的清理：steering 复位、修复工具调用配对、
    // 写入补充消息和广播解锁都必须执行。
    let finished = match &result {
        Ok(result) => update
            .emit(
                session_id.clone(),
                SessionUpdate::PromptFinished {
                    prompt_id: result.prompt_id.clone(),
                    message: result.message.clone(),
                    finish_reason: result.finish_reason.clone(),
                    usage: result.usage,
                },
            )
            .await
            .map_err(protocol_to_rpc)
            .err(),
        Err(_) => None,
    };
    inner
        .runtime(&session_id)
        .await
        .steering
        .lock()
        .await
        .accepting = false;
    repair_pending_calls(&inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    for entry in super::management::drain_steering(&inner, &session_id, true)
        .await
        .map_err(to_rpc)?
    {
        // 收尾阶段的推送失败只影响前端能否看到，不能跳过后面的清理与解锁。
        let _ = update
            .emit(
                session_id.clone(),
                SessionUpdate::SteeringApplied {
                    id: entry.id,
                    text: entry.text,
                },
            )
            .await;
    }
    // 结束阶段收到的代理消息照样写入会话：模型这轮看不到，但界面和下次唤醒要能看到。
    crate::agents::drain_inbox(&inner, &update, &session_id, true)
        .await
        .map_err(to_rpc)?;
    // 写租约不随提示结束释放：它属于「这个 core 进程打开了该会话」，由 unload 或进程
    // 退出释放，否则另一个进程可以在两次提示之间抢走会话。
    let result = match result {
        Err(error) => Err(error),
        Ok(outcome) => finished.map_or(Ok(outcome), Err),
    };
    // `afterPrompt` 在清理之后触发：失败、取消与循环终止的路径同样能被插件观察到。
    let plugin_state = record.state.lock().await.clone();
    if let Some(host) = crate::service::plugin_events::host_for(
        &inner,
        std::path::Path::new(&plugin_state.session.cwd),
    )
    .await
    {
        let context = crate::service::plugin_events::context(
            &session_id,
            &plugin_state.session.cwd,
            plugin_state.mode,
            plugin_state.permission,
            &plugin_state.model,
            plugin_state.session.is_subagent(),
        );
        let (prompt_id, finish_reason, usage, error) = match &result {
            Ok(outcome) => (
                outcome.prompt_id.clone(),
                Some(outcome.finish_reason.clone()),
                outcome.usage,
                None,
            ),
            Err(error) => (
                crate::ids::prompt_id(plugin_state.prompt_sequence),
                None,
                Usage::default(),
                Some(format!("{error}")),
            ),
        };
        let loop_detected = matches!(finish_reason, Some(FinishReason::LoopDetected));
        crate::service::plugin_events::after_prompt(
            &host,
            &context,
            &prompt_id,
            finish_reason.as_ref(),
            usage,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            loop_detected,
            error.as_deref(),
        )
        .await;
    }
    result
}

/// 一轮请求需要的会话状态副本。
///
/// 只复制构建请求所需的部分，避免 `'turns` 循环每轮深拷贝整个 [`SessionState`]
/// （长会话带 base64 图片时一次可达数十 MB）。
struct TurnInputs {
    messages: Vec<Message>,
    context_tokens: u64,
    native_input: Option<Vec<Value>>,
    server_compaction_disabled: bool,
}

/// 在调用方持有的会话状态锁内生成 [`TurnInputs`]；持锁期间没有 await。
fn turn_inputs(
    state: &crate::session::state::SessionState,
    system: &[Message],
    model: &crate::ResolvedModel,
    tools: &[moc_llm::ToolDef],
    additional_tokens: u64,
) -> TurnInputs {
    let messages = model_messages(state, Some(system), Some(model));
    let context_tokens =
        compact::context_tokens_with_additional(state, &messages, tools, additional_tokens);
    TurnInputs {
        messages,
        context_tokens,
        native_input: state.native_input.clone(),
        server_compaction_disabled: state.server_compaction_disabled,
    }
}

async fn run_prompt_locked(
    inner: Arc<CoreInner>,
    ctx: &RequestContext,
    params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
) -> Result<PromptResult, RpcError> {
    let update = ctx.updates();
    let state_snapshot = record.state.lock().await.clone();
    let project_config = inner
        .project_config(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    let model = project_config
        .resolve_model(&state_snapshot.model)
        .map_err(to_rpc)?;
    let user_message = project_input(
        params.input,
        model.capabilities.image,
        std::path::Path::new(&state_snapshot.session.cwd),
    )
    .await
    .map_err(to_rpc)?;
    if user_message.content.is_empty() {
        return Err(RpcError::of(RpcErrorCode::InvalidParams));
    }
    // 插件宿主与事件上下文在落库前就要就绪：`messageReceived` 是正文改写的提交点。
    let plugins = inner
        .project_plugins(std::path::Path::new(&state_snapshot.session.cwd))
        .await
        .map_err(to_rpc)?;
    // CLM 是会话级模式，临时的 `/ask` 不能覆盖已启用的 CLM。
    let mode = if state_snapshot.mode == moc_protocol::methods::SessionMode::ClmExperimental {
        moc_protocol::methods::SessionMode::ClmExperimental
    } else {
        params.mode.unwrap_or(state_snapshot.mode)
    };
    let permission = inner.effective_permission(state_snapshot.permission);
    let plugin_context = crate::service::plugin_events::context(
        &params.session_id,
        &state_snapshot.session.cwd,
        mode,
        permission,
        &model.selector,
        state_snapshot.session.is_subagent(),
    );
    let agent_meta = params.agent_message.clone();
    let source = if agent_meta.is_some() { "agent" } else { "user" };
    let images = user_message
        .content
        .iter()
        .filter(|part| matches!(part, moc_llm::ContentPart::Image { .. }))
        .count();
    let rewrite = crate::service::plugin_events::message_received(
        &plugins,
        &plugin_context,
        source,
        &user_message.text(),
        images,
        agent_meta.as_ref(),
    )
    .await;
    let user_message = match rewrite.text {
        Some(text) => {
            if text.trim().is_empty() {
                return Err(RpcError::of(RpcErrorCode::InvalidParams));
            }
            crate::service::plugin_events::rewrite_message_text(user_message, text)
        }
        None => user_message,
    };
    // 代理消息原样写入会话：skill 调用对用户输入的改写不适用于代理投递的消息。
    let user_message = if agent_meta.is_none() {
        apply_skill_invocation(&inner, user_message, &state_snapshot.session.cwd).await?
    } else {
        user_message
    };
    let first_prompt = state_snapshot.prompt_sequence == 0;
    match &agent_meta {
        Some(meta) => {
            inner
                .sessions
                .append_agent_message(&params.session_id, user_message.clone(), meta.clone())
                .await
                .map_err(to_rpc)?;
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::AgentMessage {
                        meta: meta.clone(),
                        body: user_message.text(),
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
        None => {
            inner
                .sessions
                .append_message(&params.session_id, user_message.clone(), None, None)
                .await
                .map_err(to_rpc)?;
        }
    }
    if state_snapshot.session.title == "新会话" {
        let title = user_message
            .text()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|character| !character.is_control())
            .take(60)
            .collect::<String>();
        if !title.trim().is_empty() {
            let info = inner
                .sessions
                .rename(
                    &params.session_id,
                    moc_protocol::methods::SessionRenameParams {
                        session_id: params.session_id.clone(),
                        title: title.trim().to_owned(),
                    },
                )
                .await
                .map_err(to_rpc)?;
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::TitleChanged { title: info.title },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
    }
    let runtime = inner.runtime(&params.session_id).await;
    let guard = runtime
        .guard
        .lock()
        .clone()
        .expect("运行中的提示一定持有共享预算");
    let mut tool_cycles = ToolCycles::default();
    let mut state = record.state.lock().await;
    state.todo_reminders_sent = 0;
    let prompt_id = crate::ids::prompt_id(state.prompt_sequence);
    let clm_active = mode == moc_protocol::methods::SessionMode::ClmExperimental;
    let cwd = std::path::PathBuf::from(&state.session.cwd);
    drop(state);
    if clm_active {
        let current = record.state.lock().await.clone();
        if current.clm_context.is_none() {
            let initial = model_messages(&current, None, Some(&model));
            crate::tools::clm::ensure_context(
                &inner.sessions,
                &params.session_id,
                &current,
                &initial,
            )
            .await
            .map_err(to_rpc)?;
        } else {
            crate::tools::clm::append_message(
                &inner.sessions,
                &record.state,
                &params.session_id,
                &user_message,
            )
            .await
            .map_err(to_rpc)?;
        }
    }

    if project_config.workspace.watch && runtime.workspace.lock().await.is_none() {
        let baseline = snapshot_workspace(cwd.clone(), project_config.workspace.max_scan_entries)
            .await
            .map_err(to_rpc)?;
        *runtime.workspace.lock().await = Some(baseline);
    }
    crate::service::plugin_events::before_prompt(
        &plugins,
        &plugin_context,
        crate::service::plugin_events::prompt_started_payload(
            &prompt_id,
            first_prompt,
            &model.selector,
            mode,
            permission,
            source,
            &user_message.text(),
            images,
        ),
    )
    .await;
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::PromptStarted {
                prompt_id: prompt_id.clone(),
                model: model.selector.clone(),
                at_ms: now_ms(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    let cancel = ctx.cancel();
    // 一次提示内 MCP 的工具发现和执行用同一份目录，服务器刷新到下一次提示才生效。
    let mcp = inner
        .project_mcp(std::path::Path::new(&state_snapshot.session.cwd))
        .await
        .map_err(to_rpc)?;
    mcp.start().await;
    for notice in mcp.drain_notices().await {
        record
            .state
            .lock()
            .await
            .notices
            .push(format!("MCP `{}`：{}", notice.server, notice.message));
    }
    let mcp_snapshot = mcp.snapshot().await;
    // 加载期失败的插件只有在宿主建好后才有通知：先把积压送达模型与前端。
    flush_plugin_notices(&plugins, &record.state, &update, &params.session_id).await?;
    let plugin_tools = crate::service::plugin_tools_of(&plugins);
    let catalog = Arc::new(
        inner
            .tools
            .catalog(&mcp_snapshot, Arc::clone(&mcp), plugin_tools.clone())
            .map_err(|error| to_rpc(crate::CoreError::Tool(error.to_string())))?,
    );
    let skills = inner
        .project_skills(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    // 会话身份只看会话头。系统提示、工具可见性和代理列表都必须以它为准，
    // 否则会出现「系统提示说我是子代理，工具表里却没有 yield」这类矛盾。
    let subagent = state_snapshot.session.is_subagent();
    let ask_supported = inner.client_supports_ask(ctx.connection_id());
    let tools_available = inner.tools.definitions(
        mode,
        ask_supported,
        &project_config,
        crate::tools::ToolScope {
            control_agents: !subagent,
            subagent,
        },
        &plugin_tools,
    );
    let mut last_message = Message::default();
    // 只在 break 前赋值；循环没有别的出口，所以不需要初值。
    let mut final_reason: FinishReason;
    let mut usage = Usage::default();
    let mut length_continues = 0usize;
    let mut yield_reminders = 0usize;
    let mut stream_overflows = 0usize;
    let mut cycle_redirect = None;
    let summary = catalog.summary();
    // 声明与工具表用同一份快照渲染，保证提示里的签名和实际可调用的集合一致。
    let tool_declarations = crate::tools::declarations::render(
        &catalog,
        mode,
        ask_supported,
        &project_config,
        model.context_window,
    )
    .unwrap_or_default();
    let context_files = inner.project_context_files(&params.session_id, &cwd).await;
    let subagent_prompt = crate::agents::subagent_prompt(&inner, &params.session_id).await;
    let plugin_prompt = plugins.prompt_text(&plugin_context).await;
    let system = build(
        &project_config,
        &model,
        &cwd,
        &context_files,
        &skills,
        &summary,
        &tool_declarations,
        &plugin_prompt,
        subagent_prompt.as_ref().map(|prompt| prompt.block.as_str()),
    );
    // 代理列表随兄弟代理的状态变化，放在尾部提醒里，免得每次变化都让系统前缀缓存失效。
    let roster = subagent_prompt.map(|prompt| prompt.roster);

    let mut is_first_turn = true;
    'turns: loop {
        // 本地提醒只在首轮注入；同一提示内因 continue 进入的轮次不算首轮。
        let first_turn = std::mem::take(&mut is_first_turn);
        if cancel.is_cancelled() {
            final_reason = FinishReason::Cancelled;
            break;
        }
        for entry in super::management::drain_steering(&inner, &params.session_id, false)
            .await
            .map_err(to_rpc)?
        {
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::SteeringApplied {
                        id: entry.id,
                        text: entry.text,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
        crate::agents::drain_inbox(&inner, &update, &params.session_id, false)
            .await
            .map_err(to_rpc)?;
        let mut reminder_messages = Vec::new();
        // todo 停滞提醒：仍有未完成项，且距上次 todo 变更已累计足够多次工具调用时注入一次。
        // 每个提示最多 `reminders_max` 次；前端把它显示成一行工具样式的占位，没有参数和结果。
        if project_config.tools.todo.enabled
            && project_config.tools.todo.reminders
            && mode != moc_protocol::methods::SessionMode::Plan
            && tools_available.iter().any(|def| def.name == "todo")
        {
            let mut state = record.state.lock().await;
            let open_items = state
                .todos
                .iter()
                .flat_map(|phase| &phase.items)
                .filter(|item| {
                    matches!(
                        item.status,
                        moc_protocol::TodoStatus::Pending | moc_protocol::TodoStatus::InProgress
                    )
                })
                .count();
            let due = open_items > 0
                && state.tool_calls_since_todo >= project_config.tools.todo.reminders_after
                && state.todo_reminders_sent < project_config.tools.todo.reminders_max;
            if due {
                state.tool_calls_since_todo = 0;
                state.todo_reminders_sent += 1;
                drop(state);
                reminder_messages.push(reminder_message(format!(
                    "<system-reminder>{open_items} todo items still open. If you finished a task since the last todo update, mark it done now; otherwise keep working.</system-reminder>"
                )));
                update
                    .emit(
                        params.session_id.clone(),
                        SessionUpdate::TodoReminder {
                            prompt_id: prompt_id.clone(),
                            open_items,
                        },
                    )
                    .await
                    .map_err(protocol_to_rpc)?;
            }
        }
        if let Some(contract) = mode_contract(mode) {
            reminder_messages.push(contract);
        }
        reminder_messages.push(sandbox_contract(permission, &project_config.sandbox));
        if let Some(roster) = &roster {
            reminder_messages.push(reminder_message(format!(
                "<system-reminder>{roster}</system-reminder>"
            )));
        }
        if let Some(detail) = cycle_redirect.take() {
            reminder_messages.push(reminder_message(format!("<system-interrupt reason=\"tool_loop\">{detail}. Stop repeating it: gather new evidence or summarize and finish. Another no-progress loop ends this prompt.</system-interrupt>")));
        }
        if first_turn {
            let mut reminder_state = runtime.reminders.lock().await;
            if let Some(reminder) =
                reminder_state.render(&crate::system_prompt::reminder::local_time())
            {
                reminder_messages.push(reminder_message(reminder));
            }
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        } else {
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        }
        let reminder_messages = merge_reminders(reminder_messages);
        let mut inputs = {
            let state = record.state.lock().await;
            turn_inputs(
                &state,
                &system.messages,
                &model,
                &tools_available,
                compact::messages_tokens(&reminder_messages),
            )
        };
        inputs.messages.extend(reminder_messages.iter().cloned());
        let context_tokens = inputs.context_tokens;
        // 提前摘要只改变 soft 压缩的输入来源，不改变 remote/shake 的顺序；触发条件在函数内部判断。
        compact::grok_build::prefire::maybe_prefire(
            &inner,
            &params.session_id,
            &project_config,
            &model,
            context_tokens,
            compact::grok_build::PromptSkeleton {
                system: &system.messages,
                static_prefix: &system.static_prefix,
                tools: &tools_available,
            },
            &ctx.cancel(),
        )
        .await;
        if mode != moc_protocol::methods::SessionMode::ClmExperimental
            && project_config.compaction.enabled
            && project_config.compaction.auto
            && (first_turn || project_config.compaction.mid_prompt)
            && context_tokens > compact::threshold(&project_config, model.context_window)
        {
            match compact::compact(
                &inner,
                ctx,
                &params.session_id,
                compact::CompactReason::Threshold,
                None,
                compact::ContextBudget {
                    additional_tokens: compact::messages_tokens(&reminder_messages),
                },
                None,
                None,
            )
            .await
            {
                Ok(report) => {
                    if !report.created_headroom {
                        tracing::warn!(
                            session_id = %params.session_id,
                            tokens_after = report.result.tokens_after,
                            recovery_band = compact::recovery_band(&project_config, model.context_window),
                            "压缩没有腾出足够的上下文空间，停止自动续写，避免反复压缩"
                        );
                    }
                    if !report.created_headroom || !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'turns;
                    }
                    inputs = {
                        let state = record.state.lock().await;
                        turn_inputs(
                            &state,
                            &system.messages,
                            &model,
                            &tools_available,
                            compact::messages_tokens(&reminder_messages),
                        )
                    };
                    inputs.messages.extend(reminder_messages.iter().cloned());
                }
                Err(error) => {
                    tracing::warn!(session_id = %params.session_id, %error, "自动上下文压缩失败，继续请求")
                }
            }
        }
        let mut request = build_request(
            &model,
            &project_config,
            std::mem::take(&mut inputs.messages),
            tools_available.clone(),
            inputs.native_input.clone(),
            &params.session_id,
            CacheAnchors {
                instruction_static: &system.static_prefix,
                trailing_reminders: reminder_messages.len(),
            },
        );
        let mut overflow_attempts = 0;
        // 请求耗时过长只发一条警告，生成阶段不设硬性时限。
        let request_warning = RequestWarning::spawn(
            &update,
            &params.session_id,
            &prompt_id,
            project_config.limits.request_warn_secs,
        );
        // 首 token 等待：从发出请求到收到第一个内容增量；每次重试都重新计时。
        let mut first_token_ms: Option<u64>;
        let mut attempt = 0u32;
        let (mut stream, request_permit, request_started) = loop {
            attempt += 1;
            let mut llm_config = model.llm.clone();
            if project_config.compaction.enabled
                && project_config.compaction.auto
                && project_config.compaction.remote.enabled
                && project_config.compaction.remote.server_side
                && model.remote_compaction
                && !inputs.server_compaction_disabled
                && model.llm.protocol == moc_llm::Protocol::OpenAiResponses
            {
                llm_config.server_compaction_threshold =
                    Some(compact::threshold(&project_config, model.context_window));
            }
            let mut permit = match inner.provider_permit(&model, &cancel).await {
                Ok(permit) => Some(permit),
                Err(crate::CoreError::Cancelled) => {
                    final_reason = FinishReason::Cancelled;
                    break 'turns;
                }
                Err(error) => return Err(to_rpc(error)),
            };
            let request_started = Instant::now();
            first_token_ms = None;
            crate::service::plugin_events::before_model_call(
                &plugins,
                &plugin_context,
                &prompt_id,
                attempt,
                &model.provider_id,
                &model.selector,
                Some(model.thinking_level.as_str()),
                request.messages.len(),
                request.tools.len(),
                context_tokens,
                model.max_output_tokens,
            )
            .await;
            let streamed = tokio::select! {
                _ = cancel.cancelled() => { final_reason = FinishReason::Cancelled; break 'turns; }
                result = inner.client.stream(&llm_config, &request) => result,
            };
            // 错误恢复可能再次请求同一个供应商，必须先归还失败请求占用的并发名额。
            if streamed.is_err() {
                crate::service::record_model_usage(
                    &inner,
                    &params.session_id,
                    &model.selector,
                    Usage::default(),
                    request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    None,
                )
                .await
                .map_err(to_rpc)?;
                permit.take();
            }
            match streamed {
                Ok(stream) => break (stream, permit, request_started),
                Err(moc_llm::LlmError::Guard(violation)) => {
                    guard.stop(violation);
                    final_reason = guard.reason();
                    crate::service::plugin_events::after_model_call(
                        &plugins,
                        &plugin_context,
                        &prompt_id,
                        attempt,
                        Some(&final_reason),
                        None,
                        None,
                        u64::try_from(request_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        &[],
                        Some(&final_reason.to_string()),
                    )
                    .await;
                    break 'turns;
                }
                Err(error)
                    if llm_config.server_compaction_threshold.is_some()
                        && error.api_error().is_some_and(|api| api.status == Some(400))
                        && error.to_string().contains("context_management") =>
                {
                    record.state.lock().await.server_compaction_disabled = true;
                    inputs.server_compaction_disabled = true;
                    tracing::warn!(session_id = %params.session_id, "上游不支持 context_management，本会话关闭服务端压缩");
                }
                Err(error)
                    if overflow_attempts < 2
                        && error.api_error().is_some_and(|api| {
                            api.kind == moc_llm::ApiErrorKind::ContextLength
                        }) =>
                {
                    overflow_attempts += 1;
                    match compact::compact(
                        &inner,
                        ctx,
                        &params.session_id,
                        compact::CompactReason::Overflow,
                        None,
                        compact::ContextBudget {
                            additional_tokens: compact::messages_tokens(&reminder_messages),
                        },
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(report) if report.created_headroom => {}
                        Ok(_) => {
                            let message = format!(
                                "{error}; overflow recovery did not create enough context space"
                            );
                            emit_fatal(&update, &params.session_id, &prompt_id, &message).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), message));
                        }
                        Err(compact_error) => {
                            if cancel.is_cancelled() {
                                final_reason = FinishReason::Cancelled;
                                break 'turns;
                            }
                            let message =
                                format!("{error}; overflow recovery failed: {compact_error}");
                            emit_fatal(&update, &params.session_id, &prompt_id, &message).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), message));
                        }
                    }
                    if !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'turns;
                    }
                    inputs = {
                        let state = record.state.lock().await;
                        turn_inputs(
                            &state,
                            &system.messages,
                            &model,
                            &tools_available,
                            compact::messages_tokens(&reminder_messages),
                        )
                    };
                    let mut retry_messages = inputs.messages;
                    retry_messages.extend(reminder_messages.iter().cloned());
                    request = build_request(
                        &model,
                        &project_config,
                        retry_messages,
                        tools_available.clone(),
                        inputs.native_input.clone(),
                        &params.session_id,
                        CacheAnchors {
                            instruction_static: &system.static_prefix,
                            trailing_reminders: reminder_messages.len(),
                        },
                    );
                }
                Err(error) => {
                    crate::service::plugin_events::after_model_call(
                        &plugins,
                        &plugin_context,
                        &prompt_id,
                        attempt,
                        None,
                        None,
                        first_token_ms,
                        u64::try_from(request_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        &[],
                        Some(&error.to_string()),
                    )
                    .await;
                    emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                    return Err(RpcError::new(
                        RpcErrorCode::InternalError.code(),
                        error.to_string(),
                    ));
                }
            }
        };
        update
            .emit(
                params.session_id.clone(),
                SessionUpdate::GenerationStarted {
                    prompt_id: prompt_id.clone(),
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
        let mut interrupted = false;
        let mut connection_error = None;
        let response = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    interrupted = true;
                    break Some(if guard.violation().is_some() { stream.discard(guard.reason()) } else { stream.interrupt() });
                }
                event = stream.next_event() => {
                    if guard.violation().is_some() {
                        interrupted = true;
                        break Some(stream.discard(guard.reason()));
                    }
                    match event {
                        Some(Ok(event)) => {
                            if first_token_ms.is_none() && event.starts_output() {
                                first_token_ms = Some(
                                    request_started.elapsed().as_millis().min(u64::MAX as u128)
                                        as u64,
                                );
                            }
                            if let Err(error) = forward_event(&update, &params.session_id, &prompt_id, event).await {
                                interrupted = true;
                                connection_error = Some(protocol_to_rpc(error));
                                break Some(stream.interrupt());
                            }
                        }
                        Some(Err(moc_llm::LlmError::Guard(violation))) => {
                            guard.stop(violation);
                            interrupted = true;
                            break Some(stream.discard(guard.reason()));
                        }
                        Some(Err(error)) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength)
                            && project_config.compaction.enabled && stream_overflows < 2 => {
                            let _ = stream.interrupt();
                            break None;
                        }
                        Some(Err(error)) => {
                            crate::service::plugin_events::after_model_call(
                                &plugins,
                                &plugin_context,
                                &prompt_id,
                                attempt,
                                None,
                                None,
                                first_token_ms,
                                u64::try_from(request_started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                                &[],
                                Some(&error.to_string()),
                            )
                            .await;
                            emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                        }
                        None => match stream.finish().await {
                            Ok(response) => break Some(response),
                            Err(error) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength) && project_config.compaction.enabled && stream_overflows < 2 => break None,
                            Err(error) => {
                                crate::service::plugin_events::after_model_call(
                                    &plugins,
                                    &plugin_context,
                                    &prompt_id,
                                    attempt,
                                    None,
                                    None,
                                    first_token_ms,
                                    u64::try_from(request_started.elapsed().as_millis())
                                        .unwrap_or(u64::MAX),
                                    &[],
                                    Some(&error.to_string()),
                                )
                                .await;
                                emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                                return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                            }
                        },
                }
                }
            }
        };
        drop(request_warning);
        drop(request_permit);
        let Some(response) = response else {
            stream_overflows += 1;
            let compact_report = compact::compact(
                &inner,
                ctx,
                &params.session_id,
                compact::CompactReason::Overflow,
                None,
                compact::ContextBudget {
                    additional_tokens: compact::messages_tokens(&reminder_messages),
                },
                None,
                None,
            )
            .await
            .map_err(to_rpc)?;
            if !compact_report.created_headroom {
                final_reason = FinishReason::Length;
                break 'turns;
            }
            if !project_config.compaction.auto_continue {
                final_reason = FinishReason::Length;
                break 'turns;
            }
            continue 'turns;
        };
        final_reason = if interrupted {
            FinishReason::Cancelled
        } else {
            response.finish_reason.clone()
        };
        last_message = response.message.clone();
        if guard.violation().is_some() {
            final_reason = guard.reason();
            crate::service::plugin_events::after_model_call(
                &plugins,
                &plugin_context,
                &prompt_id,
                attempt,
                Some(&final_reason),
                Some(response.usage),
                first_token_ms,
                u64::try_from(request_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                &[],
                Some(&final_reason.to_string()),
            )
            .await;
            break 'turns;
        }
        // 完整响应先落库，再发通知和记用量：辅助步骤失败不能吞掉已经完整收到的模型响应。
        inner
            .sessions
            .append_message(
                &params.session_id,
                response.message.clone(),
                None,
                Some(final_reason.clone()),
            )
            .await
            .map_err(to_rpc)?;
        let mismatches = upstream_mismatches(&model, &request, &response);
        if !mismatches.is_empty() {
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::UpstreamMismatch {
                        prompt_id: prompt_id.clone(),
                        fields: mismatches.clone(),
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
        let active_ms = request_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        crate::service::record_model_usage(
            &inner,
            &params.session_id,
            &model.selector,
            response.usage,
            active_ms,
            first_token_ms,
        )
        .await
        .map_err(to_rpc)?;
        crate::service::plugin_events::after_model_call(
            &plugins,
            &plugin_context,
            &prompt_id,
            attempt,
            Some(&final_reason),
            Some(response.usage),
            first_token_ms,
            active_ms,
            &mismatches,
            None,
        )
        .await;
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        usage.total_tokens = Some(usage.input_tokens.saturating_add(usage.output_tokens));
        if mode == moc_protocol::methods::SessionMode::ClmExperimental {
            crate::tools::clm::append_message(
                &inner.sessions,
                &record.state,
                &params.session_id,
                &response.message,
            )
            .await
            .map_err(to_rpc)?;
        }
        {
            let mut state = record.state.lock().await;
            state.usage = response.usage;
            state.usage_at = state.transcript.len();
        }

        let server_compacted = response.compaction.is_some();
        if let Some(item) = response.compaction {
            let compact_state = record.state.lock().await.clone();
            let (first_kept_id, retained_tokens) = compact::remote::retained_boundary(
                &compact_state,
                project_config.compaction.remote.retained_message_tokens as u64,
            );
            let native = Value::Array(vec![*item]);
            let native_tokens = serde_json::to_string(&native)
                .map_or(0, |serialized| compact::estimate_tokens(&serialized));
            let tokens_after = compact::messages_tokens(&system.messages)
                .saturating_add(compact::tools_tokens(&tools_available))
                .saturating_add(compact::messages_tokens(&reminder_messages))
                .saturating_add(retained_tokens)
                .saturating_add(native_tokens);
            let tokens_before = response.usage.input_tokens;
            compact::record_compaction(
                &inner,
                &params.session_id,
                compact::Checkpoint {
                    method: "remote".to_owned(),
                    model: Some(model.selector.clone()),
                    summary: compact::remote::checkpoint_summary(&compact_state),
                    reminder: None,
                    first_kept_id,
                    tokens_before,
                    tokens_after,
                    full: true,
                    instructions: None,
                    native: Some(Box::new(native)),
                    archive: compact::HistoryArchive::Write,
                },
            )
            .await
            .map_err(to_rpc)?;
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::ContextCompacted {
                        method: "remote".to_owned(),
                        tokens_before,
                        tokens_after,
                        full: true,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }

        let calls: Vec<_> = response.message.tool_calls().cloned().collect();
        if calls.is_empty() || interrupted {
            if interrupted {
                for call in &calls {
                    persist_tool_result(
                        &inner,
                        &params.session_id,
                        call.id.clone(),
                        "Prompt cancelled; this call was not executed.".into(),
                        true,
                        ToolStatus::Cancelled,
                        0,
                    )
                    .await
                    .map_err(to_rpc)?;
                }
                if let Some(error) = connection_error {
                    return Err(error);
                }
            } else if final_reason == FinishReason::Length
                && length_continues < project_config.limits.max_length_continues
                && (!server_compacted || project_config.compaction.auto_continue)
            {
                length_continues += 1;
                inner
                    .sessions
                    .append_message(
                        &params.session_id,
                        Message::user_text(
                            "Output hit the length limit. Continue from the cut without repeating text.",
                        ),
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
                continue;
            } else if final_reason == FinishReason::Length {
                tracing::warn!(session_id = %params.session_id, "模型连续输出达到长度上限");
            }
            if !interrupted {
                // 子代理必须用 `yield` 交付结果；如果只用普通文本结束回合，父代理拿到的
                // 只是一段正文而不是交付结果。所以先提醒几次，仍然没有再退回用最后一条消息。
                if subagent
                    && yield_reminders < crate::agents::MAX_YIELD_REMINDERS
                    && !crate::agents::has_yielded(&inner, &params.session_id).await
                {
                    yield_reminders += 1;
                    let reminder = format!(
                        "Reminder {yield_reminders}/{}: call `yield` with your complete report; do not end the turn with a plain message.",
                        crate::agents::MAX_YIELD_REMINDERS
                    );
                    inner
                        .sessions
                        .append_message(
                            &params.session_id,
                            Message::user_text(reminder),
                            None,
                            None,
                        )
                        .await
                        .map_err(to_rpc)?;
                    continue;
                }
                let mut steering = runtime.steering.lock().await;
                if !steering.messages.is_empty() {
                    continue;
                }
                steering.accepting = false;
            }
            break;
        }
        let tool_ctx = ToolCtx {
            session_id: params.session_id.clone(),
            prompt_id: prompt_id.clone(),
            cwd: cwd.clone(),
            paths: record.paths.clone(),
            config: Arc::clone(&project_config),
            state: Arc::clone(&record.state),
            anchors: Arc::clone(&runtime.anchors),
            request: ctx.clone(),
            cancel: cancel.clone(),
            guard: Arc::clone(&guard),
            sessions: Arc::clone(&inner.sessions),
            core: Arc::clone(&inner),
            catalog: Arc::clone(&catalog),
            mode,
            permission,
            allow_agents: !subagent,
            subagent,
            nested: false,
            jobs: Arc::clone(&runtime.jobs),
            mcp: Arc::clone(&mcp),
            skills: Arc::clone(&skills),
            plugins: Arc::clone(&plugins),
            model: model.selector.clone(),
            tool_call_id: String::new(),
            loaded_skills: Arc::clone(&runtime.loaded_skills),
            sandbox_support: inner.sandbox_support.clone(),
            sandbox_grants: Arc::clone(&runtime.sandbox_grants),
            call_grants: crate::sandbox::SandboxGrants::default(),
            sandbox_scratch: runtime.sandbox_scratch.clone(),
        };
        let batch = execute_tool_batch(&inner, &tool_ctx, &runtime, &record, mode, &calls).await?;
        let control_end = batch.control_end;
        if !control_end && !cancel.is_cancelled() && project_config.limits.loop_detection {
            match tool_cycles.record(&batch.observations) {
                CycleAction::Continue => {}
                CycleAction::Redirect(detail) => {
                    update
                        .emit(
                            params.session_id.clone(),
                            SessionUpdate::GuardNotice {
                                prompt_id: prompt_id.clone(),
                                message: format!(
                                    "检测到无进展的工具循环，已提醒模型纠正一次：{detail}"
                                ),
                                terminal: false,
                            },
                        )
                        .await
                        .map_err(protocol_to_rpc)?;
                    cycle_redirect = Some(detail);
                }
                CycleAction::Stop(detail) => guard.stop(moc_llm::LoopViolation {
                    channel: "工具",
                    detail,
                }),
            }
        }
        if control_end || cancel.is_cancelled() {
            final_reason = if control_end {
                FinishReason::Stop
            } else {
                FinishReason::Cancelled
            };
            break;
        }
        if server_compacted && !project_config.compaction.auto_continue {
            final_reason = FinishReason::Other("compacted".into());
            break;
        }
    }

    check_workspace(
        &update,
        &runtime,
        &record,
        WorkspaceScope {
            session_id: &params.session_id,
            cwd: &cwd,
            config: &project_config,
            mode,
            before_tools: None,
        },
        &plugins,
        &plugin_context,
    )
    .await?;
    Ok(PromptResult {
        session_id: params.session_id,
        prompt_id,
        message: last_message,
        finish_reason: final_reason,
        usage,
    })
}

async fn emit_fatal(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    prompt_id: &str,
    message: &str,
) -> Result<(), RpcError> {
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::Error {
                prompt_id: prompt_id.to_owned(),
                message: message.to_owned(),
                retryable: false,
            },
        )
        .await
        .map_err(protocol_to_rpc)
}

/// 请求耗时警告：到时间只推一条通知，请求结束时取消计时器。
struct RequestWarning(tokio::task::JoinHandle<()>);

impl RequestWarning {
    fn spawn(
        update: &moc_protocol::UpdateSink,
        session_id: &str,
        prompt_id: &str,
        warn_secs: u64,
    ) -> Self {
        let update = update.clone();
        let session_id = session_id.to_owned();
        let prompt_id = prompt_id.to_owned();
        Self(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(warn_secs)).await;
            let _ = update
                .emit(
                    session_id,
                    SessionUpdate::GuardNotice {
                        prompt_id,
                        message: format!(
                            "本次请求已超过 {warn_secs} 秒，仍在等待模型响应，不会自动终止"
                        ),
                        terminal: false,
                    },
                )
                .await;
        }))
    }
}

impl Drop for RequestWarning {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 解析用户输入里的 `/skill:<name> 参数` 显式调用。
///
/// skill 正文以 user 消息注入（用户指令的优先级高于工具结果）。设置了
/// `disable-model-invocation` 的 skill 只能通过这条路径使用。输入不以 `/skill:` 开头时原样返回。
async fn apply_skill_invocation(
    inner: &CoreInner,
    message: Message,
    cwd: &str,
) -> Result<Message, RpcError> {
    let text = message.text();
    let Some(rest) = text.strip_prefix("/skill:") else {
        return Ok(message);
    };
    let (name, arguments) = match rest.split_once([' ', '\n']) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest.trim_end(), ""),
    };
    if name.is_empty() {
        return Ok(message);
    }
    let skills = inner
        .project_skills(std::path::Path::new(cwd))
        .map_err(to_rpc)?;
    let mut loaded = std::collections::HashSet::new();
    let content = crate::skills::tool::render_for_invocation(&skills, name, &mut loaded)
        .map_err(crate::service::invalid_params)?;
    let mut injected = format!(
        "<system-reminder>The user injected this via /skill:{name}; follow it now.</system-reminder>\n{content}"
    );
    if !arguments.is_empty() {
        injected.push_str(&format!("\n\nUser instructions: {arguments}"));
    }
    Ok(Message::user_text(injected))
}
