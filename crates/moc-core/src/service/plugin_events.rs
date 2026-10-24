//! 插件生命周期事件的载荷与派发。
//!
//! 事件载荷的形状只在这里定义，调用点保持一行；`moc.d.ts` 与
//! `docs/plugins.md` 是同一份契约的另外两处投影。返回值契约（`beforeToolCall` /
//! `beforePrompt` / `messageReceived` / `afterToolCall` / `sessionStart` /
//! `permissionRequested` / `agentStarted`）不在这里实现，由各自的提交点直接消费。

use std::path::Path;
use std::sync::Arc;

use moc_plugin::{CallContext, HookKind, PluginHost};
use moc_protocol::methods::{
    AgentInfo, AgentMessageMeta, PathChange, PermissionOption, SessionInfo, SessionMode,
    SessionPermission,
};
use serde_json::{json, Value};

use super::CoreInner;

/// 该工作目录的插件宿主；不可用时只记日志，事件静默跳过。
pub(crate) async fn host_for(inner: &CoreInner, cwd: &Path) -> Option<Arc<PluginHost>> {
    match inner.project_plugins(cwd).await {
        Ok(host) => Some(host),
        Err(error) => {
            tracing::warn!(%error, cwd = %cwd.display(), "插件宿主不可用，跳过事件");
            None
        }
    }
}

/// 会话状态对应的插件上下文；模式与权限用本次实际生效的值。
pub(crate) fn context(
    session_id: &str,
    cwd: &str,
    mode: SessionMode,
    permission: SessionPermission,
    model: &str,
    is_subagent: bool,
) -> CallContext {
    CallContext {
        session_id: session_id.to_owned(),
        cwd: cwd.to_owned(),
        mode: mode_name(mode),
        permission: permission.as_str().to_owned(),
        model: model.to_owned(),
        is_subagent,
        writable: true,
    }
}

/// 会话创建瞬间的插件上下文。
pub(crate) fn context_from_info(info: &SessionInfo) -> CallContext {
    context(
        &info.id,
        &info.cwd,
        info.mode,
        info.permission,
        &info.model,
        info.is_subagent(),
    )
}

fn mode_name(mode: SessionMode) -> String {
    serde_json::to_value(mode)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn usage_json(usage: moc_llm::Usage) -> Value {
    json!({
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
        "totalTokens": usage.total_tokens(),
        "cachedInputTokens": usage.cached_input_tokens,
        "cacheCreationInputTokens": usage.cache_creation_input_tokens,
        "reasoningTokens": usage.reasoning_tokens,
    })
}

/// 用插件改写后的正文替换消息文本；图片块原样保留并排在新正文之后。
pub(crate) fn rewrite_message_text(message: moc_llm::Message, text: String) -> moc_llm::Message {
    let mut content = Vec::with_capacity(message.content.len().max(1));
    content.push(moc_llm::ContentPart::Text(text));
    content.extend(
        message
            .content
            .into_iter()
            .filter(|part| !matches!(part, moc_llm::ContentPart::Text(_))),
    );
    moc_llm::Message {
        role: message.role,
        content,
    }
}

/// 统一入口：没有插件声明该钩子时不构造载荷、不做任何调用。
async fn emit(host: &PluginHost, kind: HookKind, context: &CallContext, payload: Value) {
    if !host.has_hook(kind) {
        return;
    }
    host.emit(kind, payload, context).await;
}

/// `sessionStart`：观察 + 可注入初始消息（在第一条用户消息之前落库）。
pub(crate) async fn session_start(
    host: &PluginHost,
    context: &CallContext,
    info: &SessionInfo,
    origin: &str,
    parent: Option<&str>,
) -> Vec<moc_plugin::InjectedMessage> {
    if !host.has_hook(HookKind::SessionStart) {
        return Vec::new();
    }
    host.session_start(
        &json!({
            "atMs": crate::session::store::now_ms(),
            "sessionId": info.id,
            "cwd": info.cwd,
            "model": info.model,
            "mode": info.mode,
            "permission": info.permission,
            "title": info.title,
            "origin": origin,
            "parentSessionId": parent,
            "isSubagent": info.is_subagent(),
        }),
        context,
    )
    .await
}

/// `beforePrompt`：纯观察；消息此时已经落库，改写在 `messageReceived` 处完成。
pub(crate) async fn before_prompt(host: &PluginHost, context: &CallContext, payload: Value) {
    emit(host, HookKind::BeforePrompt, context, payload).await;
}

pub(crate) async fn session_end(host: &PluginHost, context: &CallContext, reason: &str) {
    emit(
        host,
        HookKind::SessionEnd,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "sessionId": context.session_id,
            "reason": reason,
        }),
    )
    .await;
}

pub(crate) async fn session_config_changed(
    host: &PluginHost,
    context: &CallContext,
    field: &str,
    from: &str,
    to: &str,
) {
    emit(
        host,
        HookKind::SessionConfigChanged,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "field": field,
            "from": from,
            "to": to,
        }),
    )
    .await;
}

/// `messageReceived`：观察 + 可在落库前改写正文。
pub(crate) async fn message_received(
    host: &PluginHost,
    context: &CallContext,
    source: &str,
    text: &str,
    images: usize,
    agent: Option<&AgentMessageMeta>,
) -> moc_plugin::MessageRewrite {
    if !host.has_hook(HookKind::MessageReceived) {
        return moc_plugin::MessageRewrite::default();
    }
    host.message_received(
        &json!({
            "atMs": crate::session::store::now_ms(),
            "source": source,
            "text": text,
            "images": images,
            "agent": agent,
        }),
        context,
    )
    .await
}

pub(crate) fn prompt_started_payload(
    prompt_id: &str,
    first_prompt: bool,
    model: &str,
    mode: SessionMode,
    permission: SessionPermission,
    source: &str,
    text: &str,
    images: usize,
) -> Value {
    json!({
        "atMs": crate::session::store::now_ms(),
        "promptId": prompt_id,
        "firstPrompt": first_prompt,
        "model": model,
        "mode": mode,
        "permission": permission,
        "source": source,
        "text": text,
        "images": images,
    })
}

pub(crate) async fn after_prompt(
    host: &PluginHost,
    context: &CallContext,
    prompt_id: &str,
    finish_reason: Option<&moc_llm::FinishReason>,
    usage: moc_llm::Usage,
    duration_ms: u64,
    loop_detected: bool,
    error: Option<&str>,
) {
    emit(
        host,
        HookKind::AfterPrompt,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "promptId": prompt_id,
            "finishReason": finish_reason.map(ToString::to_string),
            "usage": usage_json(usage),
            "durationMs": duration_ms,
            "loopDetected": loop_detected,
            "error": error,
        }),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn before_model_call(
    host: &PluginHost,
    context: &CallContext,
    prompt_id: &str,
    attempt: u32,
    provider: &str,
    model: &str,
    effort: Option<&str>,
    messages: usize,
    tools: usize,
    context_tokens: u64,
    max_output_tokens: u32,
) {
    emit(
        host,
        HookKind::BeforeModelCall,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "promptId": prompt_id,
            "attempt": attempt,
            "provider": provider,
            "model": model,
            "effort": effort,
            "messages": messages,
            "tools": tools,
            "contextTokens": context_tokens,
            "maxOutputTokens": max_output_tokens,
        }),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn after_model_call(
    host: &PluginHost,
    context: &CallContext,
    prompt_id: &str,
    attempt: u32,
    finish_reason: Option<&moc_llm::FinishReason>,
    usage: Option<moc_llm::Usage>,
    first_token_ms: Option<u64>,
    duration_ms: u64,
    mismatch: &[moc_protocol::methods::UpstreamFieldMismatch],
    error: Option<&str>,
) {
    emit(
        host,
        HookKind::AfterModelCall,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "promptId": prompt_id,
            "attempt": attempt,
            "finishReason": finish_reason.map(ToString::to_string),
            "usage": usage.map(usage_json),
            "firstTokenMs": first_token_ms,
            "durationMs": duration_ms,
            "mismatch": mismatch,
            "error": error,
        }),
    )
    .await;
}

pub(crate) async fn before_compaction(
    host: &PluginHost,
    context: &CallContext,
    reason: &str,
    instructions: Option<&str>,
    model: Option<&str>,
) {
    emit(
        host,
        HookKind::BeforeCompaction,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "reason": reason,
            "instructions": instructions,
            "model": model,
        }),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn after_compaction(
    host: &PluginHost,
    context: &CallContext,
    reason: &str,
    method: Option<&str>,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    full: Option<bool>,
    duration_ms: u64,
    error: Option<&str>,
) {
    emit(
        host,
        HookKind::AfterCompaction,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "reason": reason,
            "ok": error.is_none(),
            "method": method,
            "tokensBefore": tokens_before,
            "tokensAfter": tokens_after,
            "full": full,
            "durationMs": duration_ms,
            "error": error,
        }),
    )
    .await;
}

pub(crate) fn permission_request_payload(
    prompt_id: &str,
    request_id: &str,
    tool_call_id: &str,
    tool: &str,
    title: &str,
    body: Option<&str>,
    options: &[PermissionOption],
) -> Value {
    json!({
        "atMs": crate::session::store::now_ms(),
        "promptId": prompt_id,
        "requestId": request_id,
        "toolCallId": tool_call_id,
        "tool": tool,
        "title": title,
        "body": body,
        "options": options,
    })
}

pub(crate) async fn permission_resolved(
    host: &PluginHost,
    context: &CallContext,
    prompt_id: &str,
    request_id: &str,
    option: Option<&str>,
) {
    emit(
        host,
        HookKind::PermissionResolved,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "promptId": prompt_id,
            "requestId": request_id,
            "option": option,
        }),
    )
    .await;
}

pub(crate) async fn workspace_changed(
    host: &PluginHost,
    context: &CallContext,
    violation: bool,
    changes: &[PathChange],
) {
    emit(
        host,
        HookKind::WorkspaceChanged,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "violation": violation,
            "changes": changes,
        }),
    )
    .await;
}

pub(crate) async fn plan_decided(host: &PluginHost, context: &CallContext, plan_id: &str, action: &str) {
    emit(
        host,
        HookKind::PlanDecided,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "planId": plan_id,
            "action": action,
        }),
    )
    .await;
}

/// `agentStarted`：观察 + 可在子代理首轮提示前改写任务文本。
pub(crate) async fn agent_started(
    host: &PluginHost,
    context: &CallContext,
    agent: &AgentInfo,
    task: &str,
) -> Option<String> {
    if !host.has_hook(HookKind::AgentStarted) {
        return None;
    }
    host.agent_started(
        &json!({
            "atMs": crate::session::store::now_ms(),
            "agentId": agent.id,
            "parentSessionId": agent.parent_id,
            "title": agent.title,
            "model": agent.model,
            "task": task,
        }),
        context,
    )
    .await
}

pub(crate) async fn agent_finished(
    host: &PluginHost,
    context: &CallContext,
    agent: &AgentInfo,
    finish_reason: Option<&moc_llm::FinishReason>,
) {
    emit(
        host,
        HookKind::AgentFinished,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "agentId": agent.id,
            "parentSessionId": agent.parent_id,
            "status": agent.status,
            "finishReason": finish_reason.map(ToString::to_string),
            "lastMessage": agent.last_message,
        }),
    )
    .await;
}

pub(crate) async fn job_finished(
    host: &PluginHost,
    context: &CallContext,
    job_id: &str,
    command: &str,
    exit_code: Option<i32>,
    log_path: &str,
) {
    emit(
        host,
        HookKind::JobFinished,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "jobId": job_id,
            "command": command,
            "exitCode": exit_code,
            "logPath": log_path,
        }),
    )
    .await;
}

pub(crate) async fn user_command(
    host: &PluginHost,
    context: &CallContext,
    command: &str,
    duration_ms: u64,
    is_error: bool,
    status: &str,
    content: &str,
) {
    emit(
        host,
        HookKind::UserCommand,
        context,
        json!({
            "atMs": crate::session::store::now_ms(),
            "command": command,
            "durationMs": duration_ms,
            "isError": is_error,
            "status": status,
            "content": content,
        }),
    )
    .await;
}
