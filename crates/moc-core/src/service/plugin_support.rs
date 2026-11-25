use super::CoreInner;
use super::plugin_events;
use crate::service::protocol_to_rpc;
use moc_i18n::t;
use moc_protocol::RpcError;
use moc_protocol::commands::{CommandParamMeta, ParamChoiceMeta, ParamKind};
use moc_protocol::methods::PluginCommandInfo;
use moc_protocol::methods::PluginInfo;
use moc_protocol::methods::PluginToolInfo;
use moc_protocol::methods::SessionUpdate;
use std::borrow::Cow;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 会话状态里的插件调用上下文；构建提示词时还没有 `ToolCtx`，先按会话头取一份。
pub(crate) fn plugin_context(
    state: &crate::session::state::SessionState,
    model: &str,
) -> moc_plugin::CallContext {
    plugin_events::context(
        &state.session.id,
        &state.session.cwd,
        state.engine,
        crate::service::mode::Mode::effective(state.plan_active, None),
        state.permission,
        model,
        state.session.is_subagent(),
    )
}

/// 会话配置变更事件：引擎 / 自主程度 / 模型 / 权限切换成功后广播给插件。
pub(crate) async fn emit_session_config_changed(
    inner: &CoreInner,
    state: &crate::session::state::SessionMetadata,
    field: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    if let Some(host) =
        plugin_events::host_for(inner, std::path::Path::new(&state.session.cwd)).await
    {
        let context = plugin_events::context(
            &state.session.id,
            &state.session.cwd,
            state.engine,
            crate::service::mode::Mode::effective(state.plan_active, None),
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        plugin_events::session_config_changed(&host, &context, field, from, to).await;
    }
}

/// 把插件声明适配成工具实例；调用方已经拿到宿主时用它，免得重复查询缓存。
pub(crate) fn plugin_tools_of(host: &moc_plugin::PluginHost) -> Vec<Arc<dyn crate::tools::Tool>> {
    host.tools()
        .into_iter()
        .map(|(plugin, declaration)| {
            Arc::new(crate::tools::plugin::PluginTool::new(plugin, &declaration))
                as Arc<dyn crate::tools::Tool>
        })
        .collect()
}

fn project_command_param(param: moc_plugin::CommandParamDecl) -> CommandParamMeta {
    let kind = match param.kind {
        moc_plugin::CommandParamKind::Text => ParamKind::Text,
        moc_plugin::CommandParamKind::Path => ParamKind::Path,
        moc_plugin::CommandParamKind::Choice => ParamKind::Choice,
        moc_plugin::CommandParamKind::Number => ParamKind::Number,
        moc_plugin::CommandParamKind::Token => ParamKind::Token,
    };
    CommandParamMeta {
        name: Cow::Owned(param.name),
        kind,
        required: param.required,
        accepts_text: param.accepts_text,
        description: Cow::Owned(param.description),
        placeholder: param.placeholder.map(Cow::Owned),
        default_behavior: param.default_behavior.map(Cow::Owned),
        choices: param
            .choices
            .into_iter()
            .map(|choice| ParamChoiceMeta {
                value: Cow::Owned(choice.value),
                label: choice.label.map(Cow::Owned),
                description: Cow::Owned(choice.description),
                params: choice
                    .params
                    .into_iter()
                    .map(project_command_param)
                    .collect(),
            })
            .collect(),
        dynamic: None,
    }
}

/// 插件清单的协议投影；`plugin/list` 与 `plugin/reload` 用同一份映射。
pub(super) fn plugin_infos(host: &moc_plugin::PluginHost) -> Vec<PluginInfo> {
    host.info()
        .into_iter()
        .map(|info| PluginInfo {
            name: info.name,
            version: info.version,
            description: info.description,
            source: info.source,
            dir: info.dir.to_string_lossy().into_owned(),
            tools: info
                .tools
                .into_iter()
                .map(|tool| PluginToolInfo {
                    name: tool.name,
                    description: tool.description,
                    modes: tool.modes.names().map(str::to_owned).collect(),
                })
                .collect(),
            commands: info
                .commands
                .into_iter()
                .map(|command| PluginCommandInfo {
                    name: command.name,
                    description: command.description,
                    usage: command.usage,
                    params: command
                        .params
                        .into_iter()
                        .map(project_command_param)
                        .collect(),
                })
                .collect(),
            error: info.error,
        })
        .collect()
}

/// 插件失败提醒的前缀；`plugin_reload` 按同一前缀撤掉过时的提醒。
pub(super) fn plugin_notice_prefix(plugin: &str) -> String {
    t!("core.err.plugin_prefix", plugin = plugin).into_owned()
}

/// 把宿主积压的插件失败通知送进会话提醒与前端。
///
/// 调用方在加载、工具批次与命令失败这些边界上主动抽干通道：通知攒到下一轮再报，
/// 模型可能已经在用一把刚失去插件的工具表。
pub(crate) async fn flush_plugin_notices(
    host: &moc_plugin::PluginHost,
    state: &Mutex<crate::session::state::SessionState>,
    updates: &moc_protocol::UpdateSink,
    session_id: &str,
) -> Result<(), RpcError> {
    let notices = host.drain_notices();
    if notices.is_empty() {
        return Ok(());
    }
    {
        // 锁只圈住 push：通知里可能带多行 JS 栈，不能让格式化或 emit 拖住会话状态。
        let mut state = state.lock().await;
        for notice in &notices {
            state
                .notices
                .push(crate::session::state::SessionNotice::new(format!(
                    "{}{}（本进程内已停用；`plugin/reload` 或重启后恢复）",
                    plugin_notice_prefix(&notice.plugin),
                    notice.message
                )));
        }
    }
    for notice in notices {
        updates
            .emit(
                session_id.to_owned(),
                SessionUpdate::PluginNotice {
                    plugin: notice.plugin,
                    code: notice.code,
                    message: notice.message,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(())
}
