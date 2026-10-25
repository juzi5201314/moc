//! 只读旁路问答：复制当前模型视图，单次生成，不进入主会话的工具与持久化流程。

use std::path::PathBuf;
use std::sync::Arc;

use moc_llm::{ContentPart, FinishReason, Message, Role, StreamEvent, Usage};
use moc_protocol::methods::{BtwExchange, BtwParams, BtwResult, BtwUpdate, MAX_BTW_HISTORY_TURNS};
use moc_protocol::{RequestContext, RpcError, RpcErrorCode};
use serde_json::Value;

use super::prompt::request::{CacheAnchors, build_request};
use super::{CoreInner, invalid_params, protocol_to_rpc, to_rpc};
use crate::compact::{self, replay::model_messages};

pub(crate) async fn run(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: BtwParams,
) -> Result<BtwResult, RpcError> {
    if params.question.trim().is_empty() {
        return Err(invalid_params("旁路问题不能为空"));
    }
    if params.history.len() > MAX_BTW_HISTORY_TURNS {
        return Err(invalid_params(format!(
            "旁路历史最多保留 {MAX_BTW_HISTORY_TURNS} 轮"
        )));
    }
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    // 不抢主提示锁、不申请写租约；锁内只复制当前模型视图，不复制整份日志或等待 I/O。
    let (cwd, selector, messages, mut native_input) = {
        let state = record.state.lock().await;
        (
            PathBuf::from(&state.session.cwd),
            state.model.clone(),
            model_messages(&state, None, None),
            state.native_input.clone(),
        )
    };
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    let model = config.resolve_model(&selector).map_err(to_rpc)?;
    if !model.capabilities.text {
        return Err(RpcError::new(
            RpcErrorCode::NotSupported.code(),
            "当前模型不支持旁路文本问答",
        ));
    }
    let files = inner.project_context_files(&params.session_id, &cwd).await;
    let system = crate::system_prompt::build_btw(&config, &model, &cwd, &files);
    let mut base = system.messages;
    base.extend(messages.into_iter().map(|message| {
        compact::replay::repair_tool_history(textualize_tools(message), Some(&model))
    }));
    textualize_native_tools(&mut native_input);
    base.push(Message::user_text(
        "The main-session reference ends here. The following messages form a separate side conversation.",
    ));
    let (messages, history_turns_used) = fit_history(
        base,
        native_input.as_deref(),
        &params.history,
        &params.question,
        &model,
    )?;
    let mut request = build_request(
        &model,
        &config,
        messages,
        Vec::new(),
        native_input,
        &params.session_id,
        CacheAnchors {
            instruction_static: &system.static_prefix,
            trailing_reminders: 0,
        },
    );
    // 不发送 `tool_choice`：部分供应商在没有 `tools` 时拒绝该字段。旁路本来就不
    // 提供工具，模型若仍返回调用会被显式拒绝。
    request.parallel_tool_calls = None;
    let mut llm = model.llm.clone();
    // extra_body 会直接进入线上请求；旁路不能借配置重新获得工具或服务端动作。
    for key in [
        "tools",
        "tool_choice",
        "functions",
        "function_call",
        "parallel_tool_calls",
        "context_management",
        "mcp_servers",
    ] {
        llm.extra_body.remove(key);
    }
    llm.server_compaction_threshold = None;
    let cancel = ctx.cancel();
    let mut result = BtwResult {
        session_id: params.session_id.clone(),
        request_id: params.request_id,
        model: model.selector.clone(),
        text: String::new(),
        finish_reason: FinishReason::Cancelled,
        usage: Usage::default(),
        history_turns_used,
    };
    let updates = ctx.updates();
    updates
        .emit_btw(
            params.session_id.clone(),
            params.request_id,
            BtwUpdate::Waiting,
        )
        .await
        .map_err(protocol_to_rpc)?;
    let _permit = match inner.provider_permit(&model, &cancel).await {
        Ok(permit) => permit,
        Err(crate::CoreError::Cancelled) => return Ok(result),
        Err(error) => return Err(to_rpc(error)),
    };
    let streamed = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Ok(result),
        response = inner.client.stream(&llm, &request) => response,
    };
    let mut stream = match streamed {
        Ok(stream) => stream,
        Err(moc_llm::LlmError::Guard(_)) => {
            result.finish_reason = FinishReason::LoopDetected;
            return Ok(result);
        }
        Err(error) => return Err(llm_error(error)),
    };
    updates
        .emit_btw(
            params.session_id.clone(),
            params.request_id,
            BtwUpdate::Generating,
        )
        .await
        .map_err(protocol_to_rpc)?;
    let response = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let mut response = stream.interrupt();
                response.finish_reason = FinishReason::Cancelled;
                break response;
            }
            event = stream.next_event() => match event {
                Some(Ok(event)) => {
                    let update = match event {
                        StreamEvent::TextDelta { text, .. } => Some(BtwUpdate::TextDelta { text }),
                        StreamEvent::ReasoningDelta { text, .. } => Some(BtwUpdate::ReasoningDelta { text }),
                        StreamEvent::ToolCallStart { .. }
                        | StreamEvent::ToolCallArgumentsDelta { .. }
                        | StreamEvent::Done { finish_reason: FinishReason::ToolCalls } => {
                            return Err(tools_unavailable());
                        }
                        _ => None,
                    };
                    if let Some(update) = update {
                        updates.emit_btw(params.session_id.clone(), params.request_id, update)
                            .await.map_err(protocol_to_rpc)?;
                    }
                }
                Some(Err(moc_llm::LlmError::Guard(_))) => {
                    break stream.discard(FinishReason::LoopDetected);
                }
                Some(Err(error)) => return Err(llm_error(error)),
                None => break stream.finish().await.map_err(llm_error)?,
            }
        }
    };
    if response.finish_reason != FinishReason::Cancelled
        && (response.message.tool_calls().next().is_some()
            || response.finish_reason == FinishReason::ToolCalls)
    {
        return Err(tools_unavailable());
    }
    // 累加器是权威结果；块顺序可能不同于增量到达顺序，不能在 core 再拼一份文本。
    result.text = response.message.text();
    result.finish_reason = response.finish_reason;
    result.usage = response.usage;
    Ok(result)
}

fn tools_unavailable() -> RpcError {
    RpcError::new(
        RpcErrorCode::NotSupported.code(),
        "旁路问答不能执行工具；模型返回了工具调用，请改为无需操作的文本问题",
    )
}

fn llm_error(error: moc_llm::LlmError) -> RpcError {
    if error
        .api_error()
        .is_some_and(|api| api.kind == moc_llm::ApiErrorKind::ContextLength)
    {
        return to_rpc(crate::CoreError::ContextOverflow(
            "旁路上下文超出模型窗口；不会压缩或修改主会话，请缩短问题或在主会话中处理上下文".into(),
        ));
    }
    to_rpc(error.into())
}

/// 只把历史工具变成参考文本，避免快照恰好位于工具执行中间时形成孤立调用。
fn textualize_tools(mut message: Message) -> Message {
    if message.role == Role::Tool {
        message.role = Role::User;
    }
    for part in &mut message.content {
        let text = match part {
            ContentPart::ToolCall(call) => format!(
                "<historical-tool-call name=\"{}\" id=\"{}\">\n{}\n</historical-tool-call>",
                call.name, call.id, call.arguments
            ),
            ContentPart::ToolResult(result) => format!(
                "<historical-tool-result id=\"{}\" error=\"{}\">\n{}\n[{} historical images]\n</historical-tool-result>",
                result.tool_call_id,
                result.is_error,
                result.content,
                result.images.len()
            ),
            _ => continue,
        };
        *part = ContentPart::Text(text);
    }
    message
}

/// 保留原生压缩载荷，只把可见工具 item 转成普通消息；不丢失加密 checkpoint。
fn textualize_native_tools(input: &mut Option<Vec<Value>>) {
    for item in input.iter_mut().flatten() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind.ends_with("_call") || kind.ends_with("_call_output") {
            *item = serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("<historical-tool-record>\n{item}\n</historical-tool-record>")
                }]
            });
        }
    }
}

/// 历史最多 20 轮，线性估算一次，再按完整问答从旧到新淘汰；主上下文始终不动。
fn fit_history(
    mut base: Vec<Message>,
    native_input: Option<&[Value]>,
    history: &[BtwExchange],
    question: &str,
    model: &crate::ResolvedModel,
) -> Result<(Vec<Message>, usize), RpcError> {
    debug_assert!(history.len() <= MAX_BTW_HISTORY_TURNS);
    let question = Message::user_text(question);
    let native_tokens = native_input.map_or(0, |items| {
        compact::estimate_tokens(&serde_json::to_string(items).expect("原生上下文是 JSON"))
    });
    let fixed = compact::messages_tokens(&base)
        .saturating_add(native_tokens)
        .saturating_add(compact::message_tokens(&question))
        .saturating_add(u64::from(model.max_output_tokens));
    if fixed > model.context_window {
        return Err(to_rpc(crate::CoreError::ContextOverflow(format!(
            "主上下文与当前旁路问题需要约 {fixed} tokens，超过窗口 {}；旁路不会压缩主会话",
            model.context_window
        ))));
    }
    let turns: Vec<_> = history
        .iter()
        .map(|exchange| {
            let pair = [
                Message::user_text(&exchange.question),
                compact::replay::repair_tool_history(
                    Message::assistant_text(&exchange.answer),
                    Some(model),
                ),
            ];
            let tokens = compact::messages_tokens(&pair);
            (pair, tokens)
        })
        .collect();
    let mut total = turns
        .iter()
        .fold(fixed, |sum, (_, tokens)| sum.saturating_add(*tokens));
    let mut removed = 0;
    while total > model.context_window {
        total = total.saturating_sub(turns[removed].1);
        removed += 1;
    }
    let used = turns.len() - removed;
    base.reserve(used * 2 + 1);
    for (pair, _) in turns.into_iter().skip(removed) {
        base.extend(pair);
    }
    base.push(question);
    Ok((base, used))
}

#[cfg(test)]
mod tests;
