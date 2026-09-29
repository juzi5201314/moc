//! 配置保存与运行轮次的原子切换，以及安全工具边界的补充消息投递。

use super::CoreInner;
use crate::{
    CoreError,
    config::{discovery, editor},
    workspace::registry,
};
use moc_protocol::{ConfigAction, ConfigReply, RequestContext, SteerParams, SteerResult};
use std::{collections::VecDeque, path::Path, sync::Arc};

#[derive(Default)]
pub(crate) struct Steering {
    pub accepting: bool,
    pub messages: VecDeque<moc_llm::Message>,
}

pub(super) async fn steer(
    inner: &CoreInner,
    params: SteerParams,
) -> Result<SteerResult, CoreError> {
    let record = inner.sessions.get(&params.session_id).await?;
    let snapshot = record.state.lock().await.clone();
    let config = inner.project_config(Path::new(&snapshot.session.cwd))?;
    let model = config.resolve_model(&snapshot.model)?;
    let message = super::turn::request::project_input(
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
        return Ok(SteerResult { accepted: false });
    }
    steering.messages.push_back(message);
    Ok(SteerResult { accepted: true })
}

pub(super) async fn drain_steering(
    inner: &CoreInner,
    session: &str,
    closing: bool,
) -> Result<usize, CoreError> {
    let runtime = inner.runtime(session).await;
    let pending = {
        let mut steering = runtime.steering.lock().await;
        if closing {
            steering.accepting = false;
        }
        std::mem::take(&mut steering.messages)
    };
    let count = pending.len();
    for message in pending {
        inner
            .sessions
            .append_message(session, message, None, None)
            .await?;
    }
    Ok(count)
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
            // 正在运行的轮次使用同一份配置；保存等待其收尾，不中途撤销 MCP 连接。
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
            inner.openviking_clients.lock().await.clear();
            inner.remote_unavailable.write().clear();
            Ok(ConfigReply::Settings(editor::inspect(
                &inner.boot,
                scope,
                Path::new(&cwd),
            )?))
        }
        ConfigAction::Models { mut provider } => {
            editor::remove_nulls(&mut provider);
            let cancel = ctx.cancel();
            tokio::select! {
                _ = cancel.cancelled() => Err(CoreError::Cancelled),
                models = discovery::models(provider) => models.map(ConfigReply::Models),
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
        ConfigAction::Workspaces => {
            // 已存在会话的可访问目录补登记；消失目录没有可靠 peer 信息时不猜测身份。
            let sessions = inner.sessions.list().await?;
            for session in sessions {
                if Path::new(&session.cwd).is_dir() {
                    inner.workspace_identity(Path::new(&session.cwd))?;
                }
            }
            registry::list(&inner.config().home).map(ConfigReply::Workspaces)
        }
        ConfigAction::RebindWorkspace {
            url,
            path,
            new_path,
        } => {
            let cancel = ctx.cancel();
            let _epoch = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                epoch = inner.config_epoch.write() => epoch,
            };
            registry::rebind(&inner.config().home, &url, &path, &new_path)?;
            registry::list(&inner.config().home).map(ConfigReply::Workspaces)
        }
        ConfigAction::DeleteWorkspace {
            url,
            path,
            confirmed_peer,
        } => {
            let cancel = ctx.cancel();
            let _epoch = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                epoch = inner.config_epoch.write() => epoch,
            };
            registry::delete(&inner.config(), &url, &path, &confirmed_peer)
                .await
                .map(ConfigReply::Updated)
        }
    }
}
