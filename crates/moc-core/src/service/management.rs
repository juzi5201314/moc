//! 保存配置、与运行中提示的配置切换，以及在安全的工具边界投递补充消息。

use super::CoreInner;
use crate::{
    CoreError,
    config::{discovery, editor},
};
use moc_protocol::{
    ConfigAction, ConfigReply, ConfigScope, ProtocolError, RequestContext, SteerParams,
    SteerResult, UnsteerParams, UnsteerResult, UpdateSink, WebStateDocument, WebStateSaveParams,
    WebStateUpdated,
};
use std::{collections::VecDeque, path::Path, sync::Arc};

/// 一条已接受、但还没在安全边界写入会话的补充消息。
pub(crate) struct PendingSteering {
    /// 本次提示内单调递增的 ID；前端用它撤回。
    pub id: u64,
    pub message: moc_llm::Message,
}

/// 一条已经写入会话的补充消息；前端据此把待发送项移入转写。
pub(crate) struct AppliedSteering {
    pub id: u64,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct Steering {
    pub accepting: bool,
    pub next_id: u64,
    pub messages: VecDeque<PendingSteering>,
    /// 补充消息入队时唤醒 `wait_agent`，让它立刻返回。
    pub notify: Arc<tokio::sync::Notify>,
}

pub(super) async fn steer(
    inner: &CoreInner,
    params: SteerParams,
) -> Result<SteerResult, CoreError> {
    let record = inner.sessions.get(&params.session_id).await?;
    let snapshot = record.state.lock().await.clone();
    let config = inner.project_config(Path::new(&snapshot.session.cwd))?;
    let model = config.resolve_model(&snapshot.model)?;
    let message = super::prompt::request::project_input(
        params.input,
        model.capabilities.image,
        Path::new(&snapshot.session.cwd),
    )
    .await?;
    if message.content.is_empty() {
        return Err(CoreError::Config("补充消息不能为空".into()));
    }
    let runtime = inner.runtime(&params.session_id).await;
    if !runtime.steering.lock().await.accepting {
        return Ok(SteerResult {
            accepted: false,
            id: None,
        });
    }
    // 补充消息也走 `messageReceived`：与普通输入共享同一套改写语义。
    let mut message = message;
    if let Some(host) =
        crate::service::plugin_events::host_for(inner, Path::new(&snapshot.session.cwd)).await
    {
        let context = crate::service::plugin_events::context(
            &params.session_id,
            &snapshot.session.cwd,
            snapshot.engine,
            crate::service::mode::Mode::effective(snapshot.plan_active, None),
            snapshot.permission,
            &snapshot.model,
            snapshot.session.is_subagent(),
        );
        let images = message
            .content
            .iter()
            .filter(|part| matches!(part, moc_llm::ContentPart::Image { .. }))
            .count();
        let rewrite = crate::service::plugin_events::message_received(
            &host,
            &context,
            "steer",
            &message.text(),
            images,
            None,
        )
        .await;
        if let Some(text) = rewrite.text {
            if text.trim().is_empty() {
                return Err(CoreError::Config("补充消息不能为空".into()));
            }
            message = crate::service::plugin_events::rewrite_message_text(message, text);
        }
    }
    let mut steering = runtime.steering.lock().await;
    if !steering.accepting {
        return Ok(SteerResult {
            accepted: false,
            id: None,
        });
    }
    steering.next_id += 1;
    let id = steering.next_id;
    steering.messages.push_back(PendingSteering { id, message });
    steering.notify.notify_waiters();
    Ok(SteerResult {
        accepted: true,
        id: Some(id),
    })
}

/// 撤回一条尚未写入会话的补充消息；写入之后返回 false。
pub(super) async fn unsteer(
    inner: &CoreInner,
    params: UnsteerParams,
) -> Result<UnsteerResult, CoreError> {
    let runtime = inner.runtime(&params.session_id).await;
    let mut steering = runtime.steering.lock().await;
    let Some(index) = steering
        .messages
        .iter()
        .position(|entry| entry.id == params.id)
    else {
        return Ok(UnsteerResult { removed: false });
    };
    steering.messages.remove(index);
    // 等待中的 `wait_agent` 需要重新看到「已无待投递消息」。
    steering.notify.notify_waiters();
    Ok(UnsteerResult { removed: true })
}

pub(super) async fn drain_steering(
    inner: &CoreInner,
    session: &str,
    closing: bool,
) -> Result<Vec<AppliedSteering>, CoreError> {
    let runtime = inner.runtime(session).await;
    let pending = {
        let mut steering = runtime.steering.lock().await;
        if closing {
            steering.accepting = false;
        }
        std::mem::take(&mut steering.messages)
    };
    let mut applied = Vec::with_capacity(pending.len());
    for entry in pending {
        let text = entry.message.text();
        inner
            .sessions
            .append_message(session, entry.message, None, None)
            .await?;
        applied.push(AppliedSteering { id: entry.id, text });
    }
    Ok(applied)
}

/// `web/state`：读取 `config_home()/web.toml` 前端状态文档。
///
/// 顺手把这条连接登记进 `web/state_updated` 扇出表：只有读过状态的前端
/// 才关心后续变更；TUI 这类不读的内嵌连接自然不会收到通知。
pub(super) async fn web_state(
    inner: &CoreInner,
    ctx: &RequestContext,
) -> Result<WebStateDocument, CoreError> {
    inner
        .web_state_sinks
        .lock()
        .insert(ctx.connection_id(), ctx.updates());
    let home = inner.config().home.clone();
    tokio::task::spawn_blocking(move || crate::config::web::load(&home))
        .await
        .map_err(|error| CoreError::Config(error.to_string()))?
}

/// `web/state_save`：版本戳乐观锁整文档写回，成功后向其余连接扇出新文档。
pub(super) async fn web_state_save(
    inner: &Arc<CoreInner>,
    ctx: &RequestContext,
    params: WebStateSaveParams,
) -> Result<WebStateDocument, CoreError> {
    let home = inner.config().home.clone();
    let document = tokio::task::spawn_blocking(move || crate::config::web::save(&home, params))
        .await
        .map_err(|error| CoreError::Config(error.to_string()))??;
    broadcast_web_state(inner, ctx.connection_id(), &document);
    Ok(document)
}

/// 把保存结果推给其余已登记的连接；发起方不回推（它的乐观更新已经领先）。
///
/// 在独立任务里发：出站队列满时发送会背压，不能拖慢保存方的响应。
/// 发送失败按连接已死处理，顺手从扇出表剔除。
fn broadcast_web_state(inner: &Arc<CoreInner>, origin: u64, document: &WebStateDocument) {
    let targets: Vec<(u64, UpdateSink)> = inner
        .web_state_sinks
        .lock()
        .iter()
        .filter(|(id, _)| **id != origin)
        .map(|(id, sink)| (*id, sink.clone()))
        .collect();
    if targets.is_empty() {
        return;
    }
    let inner = Arc::clone(inner);
    let document = document.clone();
    tokio::spawn(async move {
        for (id, sink) in targets {
            if matches!(
                sink.notify::<WebStateUpdated>(document.clone()).await,
                Err(ProtocolError::ConnectionClosed)
            ) {
                inner.web_state_sinks.lock().remove(&id);
            }
        }
    });
}

pub(super) async fn manage(
    inner: &CoreInner,
    ctx: RequestContext,
    action: ConfigAction,
) -> Result<ConfigReply, CoreError> {
    match action {
        ConfigAction::Read { scope, cwd } => {
            // 插件 activate 可能调用 config.set；先完成首次加载，再读取 revision 快照。
            let _ = inner.project_plugins(Path::new(&cwd)).await;
            let boot = inner.boot.clone();
            let path = cwd.clone();
            let view = tokio::task::spawn_blocking(move || {
                editor::inspect(&boot, scope, Path::new(&path))
            })
            .await
            .map_err(|error| CoreError::Config(error.to_string()))??;
            super::plugin_persistence::settings(inner, Path::new(&cwd), view)
                .await
                .map(ConfigReply::Settings)
        }
        ConfigAction::Save {
            scope,
            cwd,
            document,
            revision,
        } => {
            let cancel = ctx.cancel();
            let _write = inner.config_write.lock().await;
            // 运行中的提示在用同一份配置：保存要等它们结束，不能中途断开 MCP 连接。
            let _epoch = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                epoch = inner.config_epoch.write() => epoch,
            };
            // 仅校验修改过的插件条目，已有坏配置不能阻挡普通 moc 设置的保存。
            let plugin_host = inner.project_plugins(Path::new(&cwd)).await.ok();
            let boot = inner.boot.clone();
            let path = cwd.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                if let Some(host) = plugin_host {
                    let previous = editor::inspect(&boot, scope, Path::new(&path))?;
                    let target = match scope {
                        ConfigScope::Global => boot.home.join("config.toml"),
                        ConfigScope::Project => Path::new(&path).join(".moc/config.toml"),
                    };
                    super::plugin_persistence::validate_changes(
                        &host,
                        &target,
                        &previous.document,
                        &document,
                    )?;
                }
                editor::save(&boot, scope, Path::new(&path), document, &revision)
            })
            .await
            .map_err(|error| CoreError::Config(error.to_string()))??;
            let managers = std::mem::take(&mut *inner.mcp_managers.lock().await);
            for manager in managers.into_values() {
                manager.shutdown().await;
            }
            *inner.config.write() = Arc::new(resolved);
            inner.project_configs.write().clear();
            inner.skill_indexes.write().clear();
            // 插件目录没变时指纹察觉不到配置改动（`disable`、超时、内存上限），必须显式丢弃宿主。
            inner.plugin_hosts.lock().await.clear();
            // 指令文件按会话缓存：保存配置后必须让缓存失效，否则已有会话还用旧开关。
            let runtimes: Vec<_> = inner.runtimes.lock().await.values().cloned().collect();
            for runtime in runtimes {
                *runtime.context_files.lock().await = None;
            }
            // 清缓存后插件会重新激活并可能写入配置；激活完成后再取最终 revision。
            let _ = inner.project_plugins(Path::new(&cwd)).await;
            let view = editor::inspect(&inner.boot, scope, Path::new(&cwd))?;
            super::plugin_persistence::settings(inner, Path::new(&cwd), view)
                .await
                .map(ConfigReply::Settings)
        }
        ConfigAction::Models { id, mut provider } => {
            editor::remove_nulls(&mut provider);
            let cancel = ctx.cancel();
            tokio::select! {
                _ = cancel.cancelled() => Err(CoreError::Cancelled),
                models = discovery::models(&id, provider) => models.map(ConfigReply::Models),
            }
        }
        ConfigAction::Maintenance { apply } => {
            let cancel = ctx.cancel();
            let _epoch = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                epoch = inner.config_epoch.write() => epoch,
            };
            inner
                .sessions
                .maintenance(&inner.config().session.retention, apply)
                .await
                .map(ConfigReply::Maintenance)
        }
    }
}
