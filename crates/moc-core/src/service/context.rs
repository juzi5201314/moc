//! 当前上下文的只读组装；正文视图复用实际发送的回放、预算与协议编码。
mod view;
use super::{CoreInner, to_rpc};
use crate::{
    compact,
    system_prompt::{PromptPiece, SystemPrompt},
};
use moc_llm::Message;
use moc_protocol::methods::{
    ContextContentResult, ContextUsageResult, ContextUsageSection, SessionContextContentParams,
    SessionContextParams,
};
use moc_protocol::{RequestContext, RpcError};
use std::path::PathBuf;
use std::sync::Arc;
pub(crate) use view::Views;
pub(crate) use view::{close, read};

pub(crate) async fn tools(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::SessionToolsParams,
) -> Result<Vec<moc_llm::ToolDef>, RpcError> {
    let record = inner.sessions.get(&params.session_id).await.map_err(to_rpc)?;
    let state = record.state.lock().await.metadata();
    let cwd = PathBuf::from(&state.session.cwd);
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    let plugins = inner.project_plugins(&cwd).await.map_err(to_rpc)?;
    let plugin_tools = crate::service::plugin_tools_of(&plugins);
    Ok(inner.tools.definitions(
        state.engine,
        inner.ask_available(ctx.connection_id(), state.session.autonomy),
        &config,
        crate::tools::ToolScope::for_metadata(&state, &config),
        &plugin_tools,
    ))
}

struct Snapshot {
    model: crate::ResolvedModel,
    config: Arc<crate::ResolvedConfig>,
    pieces: Vec<PromptPiece>,
    system: SystemPrompt,
    tools: Vec<moc_llm::ToolDef>,
    messages: Vec<Message>,
    sources: Vec<String>,
    history_tokens: u64,
}
impl Snapshot {
    fn usage(&self) -> ContextUsageResult {
        let system_tokens = compact::messages_tokens(&self.system.messages);
        let piece_tokens = |title| {
            self.pieces
                .iter()
                .filter(|p| p.title == title)
                .map(|p| compact::estimate_tokens(&p.text))
                .sum::<u64>()
        };
        let project = piece_tokens("project");
        let skills = piece_tokens("skills");
        let directory = piece_tokens("tools");
        let tools = compact::tools_tokens(&self.tools);
        let history = self.history_tokens;
        ContextUsageResult {
            context_window: self.model.context_window,
            total_tokens: system_tokens.saturating_add(tools).saturating_add(history),
            sections: [
                (
                    "system",
                    system_tokens.saturating_sub(project + skills + directory),
                ),
                ("project", project),
                ("skills", skills),
                ("tools", tools + directory),
                ("messages", history),
            ]
            .into_iter()
            .map(|(name, tokens)| ContextUsageSection {
                name: name.into(),
                tokens,
            })
            .collect(),
        }
    }
}

pub(crate) async fn usage(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextParams,
) -> Result<ContextUsageResult, RpcError> {
    Ok(snapshot(inner, &ctx, &params.session_id).await?.usage())
}

pub(crate) async fn content(
    inner: &Arc<CoreInner>,
    ctx: RequestContext,
    params: SessionContextContentParams,
) -> Result<ContextContentResult, RpcError> {
    if let Some(id) = &params.view_id {
        return inner.context_views.lock().page(
            ctx.connection_id(),
            &params.session_id,
            id,
            params.cursor,
            params.limit,
        );
    }
    let snapshot = snapshot(inner, &ctx, &params.session_id).await?;
    let session_id = params.session_id.clone();
    let client = inner.client.clone();
    let view = tokio::task::spawn_blocking(move || build_view(snapshot, &session_id, &client))
        .await
        .map_err(|error| RpcError::new(moc_protocol::RpcErrorCode::InternalError.code(), error.to_string()))??;
    let result = view.page(params.cursor, params.limit)?;
    view::install(inner, &ctx, &params.session_id, view).await?;
    Ok(result)
}

fn build_view(snapshot: Snapshot, session_id: &str, client: &moc_llm::Client) -> Result<view::View, RpcError> {
    let usage = snapshot.usage();
    // compose 按角色分组；来源也按同一规则排列，不能沿原 pieces 顺序错配。
    let mut instruction_sources = Vec::new();
    for keep_system in [true, false] {
        let labels: Vec<String> = snapshot
            .pieces
            .iter()
            .filter(|piece| snapshot.model.piece_keeps_system(piece.name) == keep_system)
            .map(|piece| format!("系统提示 · {}", piece.name))
            .collect();
        if !labels.is_empty() {
            instruction_sources.push(labels);
        }
    }
    let request = super::prompt::request::build_request(
        &snapshot.model,
        &snapshot.config,
        snapshot.messages,
        snapshot.tools,
        session_id,
        super::prompt::request::CacheAnchors {
            instruction_static: &snapshot.system.static_prefix,
            trailing_reminders: 0,
        },
    );
    let inspection = client.inspect_request(&snapshot.model.llm, &request, true);
    let protocol = serde_json::to_value(snapshot.model.llm.protocol)
        .map_err(|e| {
            RpcError::new(
                moc_protocol::RpcErrorCode::InternalError.code(),
                e.to_string(),
            )
        })?
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut view = view::View::new(
        usage,
        snapshot.model.selector,
        protocol,
        inspection,
        &snapshot.sources,
        &instruction_sources,
    );
    view.bound_text(view::TEXT_CACHE_BYTES).map_err(|error| RpcError::new(
        moc_protocol::RpcErrorCode::InternalError.code(), format!("保存上下文快照失败：{error}")
    ))?;
    Ok(view)
}

/// 异步资源准备在会话锁外，冻结消息只在锁内读取，不消费通知或刷新提醒。
async fn snapshot(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    session_id: &str,
) -> Result<Snapshot, RpcError> {
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let (cwd, model_id, engine, fast_mode) = {
        let state = record.state.lock().await;
        (
            PathBuf::from(&state.session.cwd),
            state.model.clone(),
            state.engine,
            state.session.fast_mode,
        )
    };
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    let model = config
        .resolve_model(&model_id)
        .map_err(to_rpc)?
        .with_fast_mode(fast_mode);
    let skills = {
        let state = record.state.lock().await;
        inner
            .session_skills(
                &cwd,
                crate::workflow::workflows_active(&state, &config.workflow),
            )
            .map_err(to_rpc)?
    };
    let context_files = inner.project_context_files(session_id, &cwd).await;
    let mcp_summary = inner.mcp_summary(&config).await.map_err(to_rpc)?;
    let subagent = crate::agents::subagent_prompt(inner, session_id).await;
    let plugins = inner.project_plugins(&cwd).await.map_err(to_rpc)?;
    let plugin_tools = crate::service::plugin_tools_of(&plugins);
    let mut plugin_context = {
        let state = record.state.lock().await;
        crate::service::plugin_context(&state, &model.selector)
    };
    plugin_context.writable = false;
    let plugin_prompt = plugins.prompt_text(&plugin_context).await;
    let (autonomy, workflows_active) = {
        let state = record.state.lock().await;
        (
            state.session.autonomy,
            crate::workflow::workflows_active(&state, &config.workflow),
        )
    };
    let tool_declarations = inner
        .tool_declarations(
            engine,
            &config,
            inner.ask_available(ctx.connection_id(), autonomy),
            workflows_active,
        )
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await;
    let pieces = crate::system_prompt::pieces(
        &config,
        &model,
        &cwd,
        &context_files,
        &skills,
        &mcp_summary,
        &tool_declarations,
        &plugin_prompt,
        subagent.as_ref().map(|p| p.block.as_str()),
    );
    let system = crate::system_prompt::compose(pieces.clone(), &model);
    let tools = inner.tools.definitions(
        state.engine,
        inner.ask_available(ctx.connection_id(), state.session.autonomy),
        &config,
        crate::tools::ToolScope::for_session(&state, &config),
        &plugin_tools,
    );
    let mut sources = Vec::new();
    let messages = compact::replay::model_messages_traced(
        &state,
        Some(&system.messages),
        Some(&model),
        Some(&mut sources),
    );
    let history_tokens = compact::cached_messages_tokens(&state, &messages[system.messages.len()..]);
    Ok(Snapshot {
        history_tokens,
        model,
        config,
        pieces,
        system,
        tools,
        messages,
        sources,
    })
}
