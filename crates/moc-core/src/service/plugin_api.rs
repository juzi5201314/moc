//! 插件管理协议操作。

use super::Core;
use super::plugin_support::plugin_infos;
use super::plugin_support::plugin_notice_prefix;
use crate::service::flush_plugin_notices;
use crate::service::plugin_context;
use crate::service::to_rpc;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::PluginListParams;
use moc_protocol::methods::PluginListResult;
use moc_protocol::methods::PluginReloadParams;
use moc_protocol::methods::PluginReloadResult;
use moc_protocol::methods::PluginRunCommandParams;
use moc_protocol::methods::PluginRunCommandResult;
use std::path::PathBuf;

pub(super) async fn plugin_list(
    core: &Core,
    _ctx: RequestContext,
    params: PluginListParams,
) -> Result<PluginListResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await;
    let cwd = PathBuf::from(&state.session.cwd);
    let host = core.inner.project_plugins(&cwd).await.map_err(to_rpc)?;
    Ok(PluginListResult {
        plugins: plugin_infos(&host),
    })
}

pub(super) async fn plugin_reload(
    core: &Core,
    _ctx: RequestContext,
    params: PluginReloadParams,
) -> Result<PluginReloadResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
    let host = core.inner.reload_plugins(&cwd).await.map_err(to_rpc)?;
    // 重载是用户主动发起的：新宿主若仍有加载失败，返回清单的 error 已经写明，
    // 这里丢掉通知，免得下一轮把刚展示过的失败再报一次。
    host.drain_notices();
    let plugins = plugin_infos(&host);
    // 重载的目的就是把插件修好：撤掉本次会话里对已恢复插件的「已停用」提醒，
    // 否则下一轮模型还会被告知一个其实已经可用的插件坏了。仍失败的保留原因。
    {
        let mut state = record.state.lock().await;
        for plugin in plugins.iter().filter(|plugin| plugin.error.is_none()) {
            let prefix = plugin_notice_prefix(&plugin.name);
            state
                .notices
                .retain(|notice| !notice.text.starts_with(&prefix));
        }
    }
    Ok(PluginReloadResult { plugins })
}

pub(super) async fn plugin_run_command(
    core: &Core,
    ctx: RequestContext,
    params: PluginRunCommandParams,
) -> Result<PluginRunCommandResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _epoch = core.inner.config_epoch.read().await;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, "moc-plugin")
        .await
        .map_err(to_rpc)?;
    let (cwd, context) = {
        let state = record.state.lock().await;
        (
            PathBuf::from(&state.session.cwd),
            plugin_context(&state, &state.model),
        )
    };
    // JS 可同步访问持久化 state；等待插件返回时不能占用会话状态锁。
    let host = core.inner.project_plugins(&cwd).await.map_err(to_rpc)?;
    let text = match host.run_command(&params.name, &params.args, &context).await {
        Ok(text) => text,
        Err(error) => {
            // 命令失败通常意味着插件刚被停用：先把失败通知送到前端与下一轮提醒再报错。
            flush_plugin_notices(&host, &record.state, &ctx.updates(), &params.session_id).await?;
            return Err(RpcError::new(
                RpcErrorCode::InvalidParams.code(),
                error.to_string(),
            ));
        }
    };
    Ok(PluginRunCommandResult { text })
}
