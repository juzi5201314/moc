//! 提示输入准备：重试尾部、插件改写与用户消息持久化，顺序与原执行入口一致。

use super::PromptRun;
use super::RetryTrim;
use super::request::project_input;
use super::retry::trim_failed_tail;
use super::skills::apply_skill_invocation;
use crate::service::CoreInner;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use moc_llm::Message;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::SessionUpdate;
use std::sync::Arc;

pub(super) struct Prepared {
    pub(super) state_snapshot: crate::session::state::SessionMetadata,
    pub(super) project_config: Arc<crate::ResolvedConfig>,
    pub(super) model: crate::ResolvedModel,
    pub(super) replay_calls: Option<Vec<moc_llm::ToolCall>>,
    pub(super) user_message: Option<Message>,
    pub(super) plugins: Arc<moc_plugin::PluginHost>,
    pub(super) engine: moc_protocol::SessionEngine,
    pub(super) mode: crate::service::mode::Mode,
    pub(super) permission: moc_protocol::SessionPermission,
    pub(super) plugin_context: moc_plugin::CallContext,
    pub(super) agent_meta: Option<moc_protocol::methods::AgentMessageMeta>,
    pub(super) goal_meta: Option<moc_protocol::methods::GoalRoundMeta>,
    pub(super) source: &'static str,
    pub(super) images: usize,
    pub(super) first_prompt: bool,
}

pub(super) async fn prepare(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    params: &mut moc_protocol::methods::PromptParams,
    record: &Arc<crate::session::SessionRecord>,
    run: PromptRun,
) -> Result<Prepared, RpcError> {
    let update = ctx.updates();
    let state_snapshot = record.state.lock().await.metadata();
    let project_config = inner
        .project_config(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    let model = project_config
        .resolve_model(&state_snapshot.model)
        .map_err(to_rpc)?
        .with_fast_mode(state_snapshot.session.fast_mode);
    // `/retry` 剪掉的失败回合如果整批工具都没跑，就直接重放这批调用，省掉一次模型请求。
    let mut replay_calls: Option<Vec<moc_llm::ToolCall>> = None;
    let user_message = match run {
        PromptRun::Normal => {
            let runtime = inner.runtime(&params.session_id).await;
            let message = project_input(
                std::mem::take(&mut params.input),
                model.capabilities.image,
                std::path::Path::new(&state_snapshot.session.cwd),
                &runtime.anchors,
                &project_config,
            )
            .await
            .map_err(to_rpc)?;
            if message.content.is_empty() {
                return Err(RpcError::of(RpcErrorCode::InvalidParams));
            }
            Some(message)
        }
        PromptRun::Retry => {
            if !state_snapshot.has_history {
                return Err(crate::service::invalid_params(
                    moc_i18n::t!("core.err.retry_no_context").into_owned(),
                ));
            }
            match trim_failed_tail(inner, &params.session_id).await? {
                RetryTrim::Nothing => {}
                RetryTrim::Dropped => {
                    update
                        .emit(
                            params.session_id.clone(),
                            SessionUpdate::PromptRewound {
                                replayed_tools: false,
                            },
                        )
                        .await
                        .map_err(protocol_to_rpc)?;
                }
                RetryTrim::Replay(calls) => {
                    update
                        .emit(
                            params.session_id.clone(),
                            SessionUpdate::PromptRewound {
                                replayed_tools: true,
                            },
                        )
                        .await
                        .map_err(protocol_to_rpc)?;
                    replay_calls = Some(calls);
                }
            }
            None
        }
        PromptRun::Wake => None,
    };
    // 插件宿主与事件上下文在落库前就要就绪：`messageReceived` 是正文改写的提交点。
    let plugins = inner
        .project_plugins(std::path::Path::new(&state_snapshot.session.cwd))
        .await
        .map_err(to_rpc)?;
    // 引擎是会话级不可变量；有效模式 = plan 模式 ∪ 本次提示的 `/ask` 声明。
    let engine = state_snapshot.engine;
    let mode = crate::service::mode::Mode::effective(state_snapshot.plan_active, params.mode);
    let permission = inner.effective_permission(state_snapshot.permission);
    let plugin_context = crate::service::plugin_events::context(
        &params.session_id,
        &state_snapshot.session.cwd,
        engine,
        mode,
        permission,
        &model.selector,
        state_snapshot.session.is_subagent(),
    );
    let agent_meta = params.agent_message.clone();
    let goal_meta = params.goal_round.clone();
    let source = if agent_meta.is_some() {
        "agent"
    } else if goal_meta.is_some() {
        "goal"
    } else {
        "user"
    };
    let images = user_message.as_ref().map_or(0, |message| {
        message
            .content
            .iter()
            .filter(|part| matches!(part, moc_llm::ContentPart::Image { .. }))
            .count()
    });
    // 重试与后台唤醒没有新消息：不触发 `messageReceived`，也不做 `/skill:` 改写。
    let user_message = match user_message {
        Some(message) => {
            let rewrite = crate::service::plugin_events::message_received(
                &plugins,
                &plugin_context,
                source,
                &message.text(),
                images,
                agent_meta.as_ref(),
            )
            .await;
            let message = match rewrite.text {
                Some(text) => {
                    if text.trim().is_empty() {
                        return Err(RpcError::of(RpcErrorCode::InvalidParams));
                    }
                    crate::service::plugin_events::rewrite_message_text(message, text)
                }
                None => message,
            };
            // 代理与 goal 续跑消息原样写入会话：skill 调用对用户输入的改写不适用。
            let message = if agent_meta.is_none() && goal_meta.is_none() {
                apply_skill_invocation(
                    inner,
                    message,
                    &state_snapshot.session.cwd,
                    state_snapshot.workflows_active(
                        &inner
                            .project_config(std::path::Path::new(&state_snapshot.session.cwd))
                            .map_err(to_rpc)?
                            .workflow,
                    ),
                )
                .await?
            } else {
                message
            };
            Some(message)
        }
        None => None,
    };
    let first_prompt = run == PromptRun::Normal && state_snapshot.prompt_sequence == 0;
    if let Some(user_message) = &user_message {
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
            None => match &goal_meta {
                Some(meta) => {
                    inner
                        .sessions
                        .append_goal_message(&params.session_id, user_message.clone(), meta.clone())
                        .await
                        .map_err(to_rpc)?;
                    update
                        .emit(
                            params.session_id.clone(),
                            SessionUpdate::GoalRoundMessage {
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
            },
        }
        if matches!(
            state_snapshot.session.title.as_str(),
            "New session" | "新会话"
        ) && goal_meta.is_none()
        {
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
    }
    Ok(Prepared {
        state_snapshot,
        project_config,
        model,
        replay_calls,
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
    })
}
