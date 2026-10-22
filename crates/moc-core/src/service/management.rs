//! 保存配置、与运行中提示的配置切换，以及在安全的工具边界投递补充消息。

use super::CoreInner;
use crate::{
    CoreError,
    config::{discovery, editor},
};
use moc_protocol::{
    ConfigAction, ConfigReply, RequestContext, SteerParams, SteerResult, UnsteerParams,
    UnsteerResult,
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

pub(super) async fn manage(
    inner: &CoreInner,
    ctx: RequestContext,
    action: ConfigAction,
) -> Result<ConfigReply, CoreError> {
    match action {
        ConfigAction::Read { scope, cwd } => {
            let boot = inner.boot.clone();
            tokio::task::spawn_blocking(move || editor::inspect(&boot, scope, Path::new(&cwd)))
                .await
                .map_err(|error| CoreError::Config(error.to_string()))?
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
            let boot = inner.boot.clone();
            let path = cwd.clone();
            let resolved = tokio::task::spawn_blocking(move || {
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
            // 指令文件按会话缓存：保存配置后必须让缓存失效，否则已有会话还用旧开关。
            let runtimes: Vec<_> = inner.runtimes.lock().await.values().cloned().collect();
            for runtime in runtimes {
                *runtime.context_files.lock().await = None;
            }
            Ok(ConfigReply::Settings(editor::inspect(
                &inner.boot,
                scope,
                Path::new(&cwd),
            )?))
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
