use rust_i18n::t;
use super::MAX_THINKING_REDIRECTS;
use super::RequestWarning;
use super::flush_reminders;
use super::guard::CycleAction;
use super::guard::ToolCycles;
use super::input;
use super::reminders;
use super::request::CacheAnchors;
use super::request::build_request;
use super::request::upstream_mismatches;
use super::stream::forward_event;
use super::tools::NOT_EXECUTED;
use super::tools::execute_tool_batch;
use super::tools::persist_tool_result;
use super::turn_inputs;
use super::warnings::emit_fatal;
use crate::compact;
use crate::compact::replay::model_messages;
use crate::service::CoreInner;
use crate::service::flush_plugin_notices;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use crate::session::store::now_ms;
use crate::system_prompt::build;
use crate::system_prompt::reminder;
use crate::tools::ToolCtx;
use moc_llm::FinishReason;
use moc_llm::Message;
use moc_llm::Usage;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::PromptResult;
use moc_protocol::methods::SessionUpdate;
use moc_protocol::methods::ToolStatus;
use std::sync::Arc;
use std::time::Instant;

pub(super) async fn run_prompt_locked(
    inner: Arc<CoreInner>,
    ctx: &RequestContext,
    mut params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
    retry: bool,
) -> Result<PromptResult, RpcError> {
    let update = ctx.updates();
    let input::Prepared {
        state_snapshot,
        project_config,
        model,
        mut replay_calls,
        user_message,
        plugins,
        engine,
        mode,
        permission,
        plugin_context,
        agent_meta,
        goal_meta,
        source,
        images,
        first_prompt,
    } = input::prepare(&inner, ctx, &mut params, &record, retry).await?;
    let runtime = inner.runtime(&params.session_id).await;
    let guard = runtime
        .guard
        .lock()
        .clone()
        .expect("运行中的提示一定持有共享预算");
    let mut tool_cycles = ToolCycles::default();
    let mut state = record.state.lock().await;
    state.todo_reminders_sent = 0;
    if retry {
        // 重试不落库用户消息，`prompt_sequence` 不会随 append 自增；这里显式推进一位，
        // 保证本次 `prompt_id` 与上一条提示不撞号（该计数由 transcript 推导、不落盘，
        // 协议层也不解释它，重启后重新计数即可）。
        state.prompt_sequence = state.prompt_sequence.saturating_add(1);
    }
    let prompt_id = crate::ids::prompt_id(state.prompt_sequence);
    let clm_active = engine == moc_protocol::methods::SessionEngine::ClmExperimental;
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
        } else if let Some(user_message) = &user_message {
            crate::tools::clm::append_message(
                &inner.sessions,
                &record.state,
                &params.session_id,
                user_message,
            )
            .await
            .map_err(to_rpc)?;
        }
    }

    // 重试没有新的用户正文：`beforePrompt` 仍触发，但 `text` 为空、`images` 为 0。
    let prompt_text = user_message.as_ref().map(Message::text).unwrap_or_default();
    crate::service::plugin_events::before_prompt(
        &plugins,
        &plugin_context,
        crate::service::plugin_events::prompt_started_payload(
            &prompt_id,
            first_prompt,
            &model.selector,
            &plugin_context.mode,
            permission,
            source,
            &prompt_text,
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
    mcp.wait_ready(&cancel)
        .await
        .map_err(|_| to_rpc(crate::CoreError::Cancelled))?;
    for notice in mcp.drain_notices().await {
        record
            .state
            .lock()
            .await
            .notices
            .push(crate::session::state::SessionNotice::new(format!(
                "MCP `{}`：{}",
                notice.server, notice.message
            )));
    }
    let mcp_snapshot = mcp.snapshot().await;
    // 加载期失败的插件只有在宿主建好后才有通知：先把积压送达模型与前端。
    flush_plugin_notices(&plugins, &record.state, &update, &params.session_id).await?;
    let plugin_tools = crate::service::plugin_tools_of(&plugins);
    let tool_scope = crate::tools::ToolScope::for_metadata(
        &state_snapshot,
        &project_config,
        crate::agents::roles::descriptions(&inner, &project_config, &plugins),
    );
    let catalog = Arc::new(
        inner
            .tools
            .catalog(
                &mcp_snapshot,
                Arc::clone(&mcp),
                plugin_tools.clone(),
                tool_scope.clone(),
            )
            .map_err(|error| to_rpc(crate::CoreError::Tool(error.to_string())))?,
    );
    let skills = inner
        .session_skills(
            std::path::Path::new(&state_snapshot.session.cwd),
            state_snapshot.workflows_active(&project_config.workflow),
        )
        .map_err(to_rpc)?;
    // 会话身份只看会话头。系统提示、工具可见性和代理列表都必须以它为准，
    // 否则会出现「系统提示说我是子代理，工具表里却没有 yield」这类矛盾。
    let subagent = state_snapshot.session.is_subagent();
    let ask_supported = inner.ask_available(ctx.connection_id(), state_snapshot.session.autonomy);
    let tools_available = inner.tools.definitions(
        engine,
        ask_supported,
        &project_config,
        tool_scope,
        &plugin_tools,
    );
    let mut last_message = Message::default();
    // 只在 break 前赋值；循环没有别的出口，所以不需要初值。
    let mut final_reason: FinishReason;
    let mut usage = Usage::default();
    let mut length_continues = 0usize;
    let mut yield_reminders = 0usize;
    let mut stream_overflows = 0usize;
    let mut stream_signature_recoveries = 0usize;
    let mut retry_budget = moc_llm::retry::RetryBudget::default();
    let mut cycle_redirect = None;
    // 思维链重复时先纠偏再重试；只有反复命中才按循环终止。
    let mut thinking_redirect = None;
    let mut thinking_redirects = 0usize;
    let summary = catalog.summary();
    // 声明与工具表用同一份快照渲染，保证提示里的签名和实际可调用的集合一致。
    // 只有内置签名进系统提示；MCP 索引随服务器刷新，走下面的尾部提醒。
    let tool_declarations = crate::tools::declarations::render_signatures(
        &catalog,
        engine,
        ask_supported,
        state_snapshot.workflows_active(&project_config.workflow),
        &project_config,
    )
    .unwrap_or_default();
    // 目录、模式和窗口在本次提示内固定；压缩后重用同一正文补回提醒。
    let mcp_index = crate::tools::declarations::render_mcp_index(
        &catalog,
        engine,
        mode,
        ask_supported,
        &project_config,
        model.context_window,
    );
    let (context_files, context_warning) =
        inner.prompt_context_files(&params.session_id, &cwd).await;
    if let Some(message) = context_warning {
        update
            .emit(
                params.session_id.clone(),
                SessionUpdate::GuardNotice {
                    prompt_id: prompt_id.clone(),
                    message,
                    terminal: false,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
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
        if cancel.is_cancelled() {
            final_reason = FinishReason::Cancelled;
            break 'turns;
        }
        let calls: Vec<moc_llm::ToolCall> = match replay_calls.take() {
            // `/retry` 剪掉的批次整批都没跑：直接重放这批调用，不花模型请求，也不注入新的
            // 尾部提醒；`is_first_turn` 留给紧接着那次真实请求。
            Some(calls) => calls,
            None => {
                // 本地提醒只在首轮注入；同一提示内因 continue 进入的轮次不算首轮。
                let first_turn = std::mem::take(&mut is_first_turn);
                for entry in
                    super::super::management::drain_steering(&inner, &params.session_id, false)
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
                let candidates = reminders::prepare(reminders::Scope {
                    inner: &inner,
                    record: &record,
                    runtime: &runtime,
                    update: &update,
                    session_id: &params.session_id,
                    prompt_id: &prompt_id,
                    project_config: &project_config,
                    tools_available: &tools_available,
                    engine,
                    mode,
                    permission,
                    roster: &roster,
                    mcp_index: &mcp_index,
                    cycle_redirect: cycle_redirect.take(),
                    thinking_redirect: thinking_redirect.take(),
                    first_turn,
                })
                .await?;
                let mut inputs = {
                    let state = record.state.lock().await;
                    turn_inputs(&state, &system.messages, &model, &tools_available)
                };
                let mut context_tokens = inputs.context_tokens;
                // 预火只改变 grok_build 压缩的输入来源；触发条件在函数内部判断。
                compact::grok_build::prefire::maybe_prefire(
                    &inner,
                    &params.session_id,
                    &project_config,
                    &model,
                    context_tokens,
                    compact::sample::PromptSkeleton {
                        system: &system.messages,
                        static_prefix: &system.static_prefix,
                        tools: &tools_available,
                    },
                    &ctx.cancel(),
                )
                .await;
                if engine != moc_protocol::methods::SessionEngine::ClmExperimental
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
                            if !report.created_headroom || !project_config.compaction.auto_continue
                            {
                                final_reason = FinishReason::Other("compacted".into());
                                break 'turns;
                            }
                            // 压缩重写了可见历史，先前写进去的提醒可能被摘要取代；
                            // 状态层已经清空去重表，这里补一份仍生效的条目。
                            flush_reminders(&inner, &params.session_id, &record, &candidates)
                                .await?;
                            inputs = {
                                let state = record.state.lock().await;
                                turn_inputs(&state, &system.messages, &model, &tools_available)
                            };
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
                    &params.session_id,
                    CacheAnchors {
                        instruction_static: &system.static_prefix,
                        trailing_reminders: 0,
                    },
                );
                let mut overflow_attempts = 0;
                let mut signature_recoveries = 0usize;
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
                        result = inner.client.stream_with_budget(&model.llm, &request, &mut retry_budget) => result,
                    };
                    // 错误恢复可能再次请求同一个供应商，必须先归还失败请求占用的并发名额。
                    if streamed.is_err() {
                        crate::service::record_model_usage(
                            &inner,
                            &params.session_id,
                            model.actual_selector(),
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
                                u64::try_from(request_started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                                &[],
                                Some(&final_reason.to_string()),
                            )
                            .await;
                            break 'turns;
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
                                    emit_fatal(&update, &params.session_id, &prompt_id, &message)
                                        .await?;
                                    return Err(RpcError::new(
                                        RpcErrorCode::InternalError.code(),
                                        message,
                                    ));
                                }
                                Err(compact_error) => {
                                    if cancel.is_cancelled() {
                                        final_reason = FinishReason::Cancelled;
                                        break 'turns;
                                    }
                                    let message = format!(
                                        "{error}; overflow recovery failed: {compact_error}"
                                    );
                                    emit_fatal(&update, &params.session_id, &prompt_id, &message)
                                        .await?;
                                    return Err(RpcError::new(
                                        RpcErrorCode::InternalError.code(),
                                        message,
                                    ));
                                }
                            }
                            if !project_config.compaction.auto_continue {
                                final_reason = FinishReason::Other("compacted".into());
                                break 'turns;
                            }
                            flush_reminders(&inner, &params.session_id, &record, &candidates)
                                .await?;
                            inputs = {
                                let state = record.state.lock().await;
                                turn_inputs(&state, &system.messages, &model, &tools_available)
                            };
                            context_tokens = inputs.context_tokens;
                            request = build_request(
                                &model,
                                &project_config,
                                std::mem::take(&mut inputs.messages),
                                tools_available.clone(),
                                &params.session_id,
                                CacheAnchors {
                                    instruction_static: &system.static_prefix,
                                    trailing_reminders: 0,
                                },
                            );
                        }
                        Err(error) => {
                            if signature_recoveries < 1
                                && error.api_error().is_some_and(|api| {
                                    api.kind == moc_llm::ApiErrorKind::ThinkingSignatureInvalid
                                })
                                && try_strip_old_reasoning(
                                    &inner,
                                    &update,
                                    &record,
                                    &params.session_id,
                                    &prompt_id,
                                )
                                .await?
                            {
                                signature_recoveries += 1;
                                inputs = {
                                    let state = record.state.lock().await;
                                    turn_inputs(&state, &system.messages, &model, &tools_available)
                                };
                                context_tokens = inputs.context_tokens;
                                request = build_request(
                                    &model,
                                    &project_config,
                                    std::mem::take(&mut inputs.messages),
                                    tools_available.clone(),
                                    &params.session_id,
                                    CacheAnchors {
                                        instruction_static: &system.static_prefix,
                                        trailing_reminders: 0,
                                    },
                                );
                                continue;
                            }
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
                            emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string())
                                .await?;
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
                let mut previews = super::preview::Previews::default();
                let mut preview_tick = tokio::time::interval(super::preview::INTERVAL);
                preview_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut preview_identities = Vec::new();
                let response: Result<_, StreamRecovery> = loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            interrupted = true;
                            break Ok(if guard.violation().is_some() { stream.discard(guard.reason()) } else { stream.interrupt() });
                        }
                        _ = preview_tick.tick() => {
                            if let Err(error) = previews.flush(&update, &params.session_id, &prompt_id).await {
                                interrupted = true;
                                connection_error = Some(protocol_to_rpc(error));
                                break Ok(stream.interrupt());
                            }
                        }
                        event = stream.next_event() => {
                            if guard.violation().is_some() {
                                interrupted = true;
                                break Ok(stream.discard(guard.reason()));
                            }
                            match event {
                                Some(Ok(event)) => {
                                    if first_token_ms.is_none() && event.starts_output() {
                                        first_token_ms = Some(
                                            request_started.elapsed().as_millis().min(u64::MAX as u128)
                                                as u64,
                                        );
                                    }
                                    let forwarded = async {
                                        if previews.apply(&event) {
                                            previews.flush(&update, &params.session_id, &prompt_id).await?;
                                        }
                                        forward_event(&update, &params.session_id, &prompt_id, event).await
                                    }.await;
                                    if let Err(error) = forwarded {
                                        interrupted = true;
                                        connection_error = Some(protocol_to_rpc(error));
                                        break Ok(stream.interrupt());
                                    }
                                }
                                Some(Err(moc_llm::LlmError::Guard(violation)))
                                    if violation.kind == moc_llm::LoopChannel::Reasoning
                                        && thinking_redirects < MAX_THINKING_REDIRECTS =>
                                {
                                    // 重复的思维链不会自己停下来：丢弃这次流、写一条纠偏提醒，
                                    // 然后重新请求。提醒落库后模型在新的请求里就能看到它。
                                    thinking_redirects += 1;
                                    thinking_redirect = Some(violation.detail);
                                    let _ = stream.discard(FinishReason::Cancelled);
                                    previews.resolve(&update, &params.session_id, &prompt_id, &[]).await.map_err(protocol_to_rpc)?;
                                    continue 'turns;
                                }
                                Some(Err(moc_llm::LlmError::Guard(violation))) => {
                                    guard.stop(violation);
                                    interrupted = true;
                                    break Ok(stream.discard(guard.reason()));
                                }
                                Some(Err(error)) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength)
                                    && project_config.compaction.enabled && stream_overflows < 2 => {
                                    let _ = stream.interrupt();
                                    break Err(StreamRecovery::Overflow);
                                }
                                Some(Err(error))
                                    if error.api_error().is_some_and(|api| {
                                        api.kind == moc_llm::ApiErrorKind::ThinkingSignatureInvalid
                                    }) && first_token_ms.is_none()
                                        && stream_signature_recoveries < 1 =>
                                {
                                    let _ = stream.interrupt();
                                    break Err(StreamRecovery::StripReasoning(error));
                                }
                                Some(Err(error)) => {
                                    let failed = stream.discard(FinishReason::Cancelled);
                                    break Err(StreamRecovery::Retry(error, failed.usage));
                                }
                                None => {
                                    let failed_usage = stream.usage();
                                    preview_identities = stream.tool_call_identities();
                                    match stream.finish().await {
                                    Ok(response) => break Ok(response),
                                    Err(error) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength) && project_config.compaction.enabled && stream_overflows < 2 => break Err(StreamRecovery::Overflow),
                                    Err(error)
                                        if error.api_error().is_some_and(|api| {
                                            api.kind == moc_llm::ApiErrorKind::ThinkingSignatureInvalid
                                        }) && first_token_ms.is_none()
                                            && stream_signature_recoveries < 1 =>
                                    {
                                        break Err(StreamRecovery::StripReasoning(error));
                                    }
                                    Err(error) => break Err(StreamRecovery::Retry(error, failed_usage)),
                                    }
                                },
                        }
                        }
                    }
                };
                if connection_error.is_some() {
                    previews = super::preview::Previews::default();
                } else if (response.is_err() || interrupted || guard.violation().is_some())
                    && let Err(error) = previews
                        .resolve(&update, &params.session_id, &prompt_id, &[])
                        .await
                {
                    connection_error = Some(protocol_to_rpc(error));
                    previews = super::preview::Previews::default();
                }
                drop(request_warning);
                drop(request_permit);
                let response = match response {
                    Ok(response) => response,
                    Err(StreamRecovery::Retry(error, failed_usage)) => {
                        usage.input_tokens =
                            usage.input_tokens.saturating_add(failed_usage.input_tokens);
                        usage.output_tokens = usage
                            .output_tokens
                            .saturating_add(failed_usage.output_tokens);
                        usage.total_tokens =
                            Some(usage.input_tokens.saturating_add(usage.output_tokens));
                        let active_ms = u64::try_from(request_started.elapsed().as_millis())
                            .unwrap_or(u64::MAX);
                        crate::service::record_model_usage(
                            &inner,
                            &params.session_id,
                            model.actual_selector(),
                            failed_usage,
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
                            None,
                            Some(failed_usage),
                            first_token_ms,
                            active_ms,
                            &[],
                            Some(&error.to_string()),
                        )
                        .await;
                        let message = error.to_string();
                        let delay = match retry_budget.next_delay(&model.llm.retry, error) {
                            Ok(delay) => delay,
                            Err(error) => {
                                emit_fatal(
                                    &update,
                                    &params.session_id,
                                    &prompt_id,
                                    &error.to_string(),
                                )
                                .await?;
                                return Err(RpcError::new(
                                    RpcErrorCode::InternalError.code(),
                                    error.to_string(),
                                ));
                            }
                        };
                        update.emit(params.session_id.clone(), SessionUpdate::GenerationRetry {
                            prompt_id: prompt_id.clone(),
                            // TUI 按本地化锚点定位秒数并替换成倒数；改措辞时同步 moc-tui 与 cli.render.retry。
                            message: t!(
                                "core.err.retry_notice",
                                secs = format!("{:.1}", delay.as_secs_f64()),
                                attempt = retry_budget.attempts() + 1,
                                max = model.llm.retry.max_attempts,
                                message = message
                            )
                            .into_owned(),
                            retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        }).await.map_err(protocol_to_rpc)?;
                        last_message = Message::default();
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                final_reason = FinishReason::Cancelled;
                                break 'turns;
                            }
                            _ = tokio::time::sleep(delay) => {}
                        }
                        continue 'turns;
                    }
                    Err(StreamRecovery::Overflow) => {
                        stream_overflows += 1;
                        let compact_report = compact::compact(
                            &inner,
                            ctx,
                            &params.session_id,
                            compact::CompactReason::Overflow,
                            None,
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
                    }
                    Err(StreamRecovery::StripReasoning(error)) => {
                        stream_signature_recoveries += 1;
                        if try_strip_old_reasoning(
                            &inner,
                            &update,
                            &record,
                            &params.session_id,
                            &prompt_id,
                        )
                        .await?
                        {
                            continue 'turns;
                        }
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
                        emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string())
                            .await?;
                        return Err(RpcError::new(
                            RpcErrorCode::InternalError.code(),
                            error.to_string(),
                        ));
                    }
                };
                retry_budget = moc_llm::retry::RetryBudget::default();
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
                    model.actual_selector(),
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
                if engine == moc_protocol::methods::SessionEngine::ClmExperimental {
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

                previews
                    .resolve(&update, &params.session_id, &prompt_id, &preview_identities)
                    .await
                    .map_err(protocol_to_rpc)?;
                let calls: Vec<_> = response.message.tool_calls().cloned().collect();
                if calls.is_empty() || interrupted {
                    if interrupted {
                        for call in &calls {
                            persist_tool_result(
                                &inner,
                                &params.session_id,
                                call.id.clone(),
                                NOT_EXECUTED.into(),
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
                    {
                        length_continues += 1;
                        inner
                    .sessions
                    .append_message(
                        &params.session_id,
                        Message::custom(reminder::wrap(
                            "Output hit the length limit. Continue from the cut without repeating text.",
                        )),
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
                        continue 'turns;
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
                            let text = format!(
                                "Reminder {yield_reminders}/{}: call `yield` with your complete report; do not end the turn with a plain message.",
                                crate::agents::MAX_YIELD_REMINDERS
                            );
                            inner
                                .sessions
                                .append_message(
                                    &params.session_id,
                                    Message::custom(reminder::wrap(&text)),
                                    None,
                                    None,
                                )
                                .await
                                .map_err(to_rpc)?;
                            continue 'turns;
                        }
                        let mut steering = runtime.steering.lock().await;
                        if !steering.messages.is_empty() {
                            continue 'turns;
                        }
                        // 收件箱里还压着投递就先别收：继续跑一轮把消息答掉。
                        // 持 steering 锁时再取 inbox 锁（锁序 steering→inbox）。
                        if !crate::agents::close_inbox_if_empty(&inner, &params.session_id).await {
                            continue 'turns;
                        }
                        steering.accepting = false;
                    }
                    break 'turns;
                }
                calls
            }
        };
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
            engine,
            mode,
            permission,
            allow_agents: state_snapshot.session.delegation_depth < project_config.agents.max_depth,
            subagent,
            user_initiated: agent_meta.is_none() && goal_meta.is_none(),
            goal_round: goal_meta.clone(),
            tool_policy: state_snapshot.tool_policy.clone(),
            nested: false,
            jobs: Arc::clone(&runtime.jobs),
            mcp: Arc::clone(&mcp),
            skills: Arc::clone(&skills),
            plugins: Arc::clone(&plugins),
            model: model.selector.clone(),
            accepts_image: model.capabilities.image,
            tool_call_id: String::new(),
            loaded_skills: Arc::clone(&runtime.loaded_skills),
            sandbox_support: inner.sandbox_support.clone(),
            sandbox_grants: Arc::clone(&runtime.sandbox_grants),
            call_grants: crate::sandbox::SandboxGrants::default(),
            sandbox_scratch: runtime.sandbox_scratch.clone(),
        };
        let batch = execute_tool_batch(&inner, &tool_ctx, &record, &calls).await?;
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
                CycleAction::Stop(detail) => guard.stop(moc_llm::LoopViolation::new(
                    moc_llm::LoopChannel::Tool,
                    detail,
                )),
            }
        }
        // 子代理 `yield` 后收件箱可能还压着投递：有消息就再跑一轮作答，
        // 后一轮的 yield 会覆盖前一份报告。
        if control_end
            && subagent
            && !cancel.is_cancelled()
            && !crate::agents::close_inbox_if_empty(&inner, &params.session_id).await
        {
            continue 'turns;
        }
        if control_end || cancel.is_cancelled() {
            final_reason = if control_end {
                FinishReason::Stop
            } else {
                FinishReason::Cancelled
            };
            break;
        }
    }

    Ok(PromptResult {
        session_id: params.session_id,
        prompt_id,
        message: last_message,
        finish_reason: final_reason,
        usage,
    })
}

enum StreamRecovery {
    Overflow,
    StripReasoning(moc_llm::LlmError),
    Retry(moc_llm::LlmError, Usage),
}

async fn try_strip_old_reasoning(
    inner: &Arc<CoreInner>,
    update: &moc_protocol::UpdateSink,
    record: &Arc<crate::session::SessionRecord>,
    session_id: &str,
    prompt_id: &str,
) -> Result<bool, RpcError> {
    let can_strip = {
        let state = record.state.lock().await;
        compact::replay::has_unstripped_reasoning(&state)
    };
    if !can_strip {
        return Ok(false);
    }
    inner
        .sessions
        .append(
            session_id,
            crate::session::entries::EntryPayload::ReasoningBoundary,
        )
        .await
        .map_err(to_rpc)?;
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::GuardNotice {
                prompt_id: prompt_id.to_owned(),
                message:
                    "上游拒绝历史加密思考内容（thinking_signature_invalid），已自动剥离旧思考块并原地重试"
                        .into(),
                terminal: false,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(true)
}
