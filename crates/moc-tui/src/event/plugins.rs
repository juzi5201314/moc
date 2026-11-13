//! 插件目录刷新与状态展示。

use crate::app::App;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use moc_protocol::PluginList;
use moc_protocol::PluginListParams;
use moc_protocol::PluginReload;
use moc_protocol::PluginReloadParams;

/// 刷新插件清单：`show` 为真时把清单写进转写（`/plugins`），否则只更新命令补全；
/// `reload` 为真时先丢弃插件宿主并重新加载（`/plugins reload`）。
pub(super) fn refresh_plugins(
    app: &App,
    connection: &EmbeddedConnection,
    show: bool,
    reload: bool,
) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = if reload {
            client
                .call::<PluginReload>(PluginReloadParams { session_id })
                .await
                .map(|result| result.plugins)
                .map_err(|error| format!("重载插件失败：{error}"))
        } else {
            client
                .call::<PluginList>(PluginListParams { session_id })
                .await
                .map(|result| result.plugins)
                .map_err(|error| format!("读取插件失败：{error}"))
        };
        match outcome {
            Ok(plugins) => {
                let text = show.then(|| {
                    if reload {
                        reload_plugins_note(&plugins)
                    } else {
                        format_plugins(&plugins)
                    }
                });
                let _ = events.send(AppEvent::Plugins(plugins));
                if let Some(text) = text {
                    let _ = events.send(AppEvent::PluginOutput {
                        name: "plugins".into(),
                        text,
                        error: false,
                    });
                }
            }
            Err(text) => {
                if show {
                    let _ = events.send(AppEvent::PluginOutput {
                        name: "plugins".into(),
                        text,
                        error: true,
                    });
                }
            }
        }
    });
}

/// `/plugins` 的正文：首行是总数，随后每个插件一行来源与目录，再列出它注册的工具和命令。
pub(super) fn format_plugins(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    if plugins.is_empty() {
        return "当前工作区没有插件。把插件放进 .moc/plugins/<name>/（或 $MOC_HOME/plugins）即可自动启用。"
            .into();
    }
    format!(
        "已加载 {} 个插件：\n{}",
        plugins.len(),
        format_plugin_lines(plugins)
    )
}

/// `/plugins reload` 的正文：重载结果的可用计数，后接每个插件的明细。
pub(super) fn reload_plugins_note(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    if plugins.is_empty() {
        return "插件已重载：当前工作区没有插件".into();
    }
    let disabled = plugins
        .iter()
        .filter(|plugin| plugin.error.is_some())
        .count();
    format!(
        "插件已重载：{} 个可用 / {disabled} 个仍不可用\n{}",
        plugins.len() - disabled,
        format_plugin_lines(plugins)
    )
}

/// 插件明细：来源、目录、不可用原因，以及它注册的工具与命令。
pub(super) fn format_plugin_lines(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    let mut lines = Vec::new();
    for plugin in plugins {
        let version = plugin
            .version
            .as_deref()
            .map(|version| format!(" v{version}"))
            .unwrap_or_default();
        lines.push(format!(
            "- {}{version}（{}）{}",
            plugin.name,
            plugin.source,
            plugin.description.as_deref().unwrap_or_default()
        ));
        lines.push(format!("  {}", plugin.dir));
        if let Some(error) = &plugin.error {
            lines.push(format!("  不可用：{error}"));
            continue;
        }
        for tool in &plugin.tools {
            lines.push(format!("  工具 {} [{}]", tool.name, tool.modes.join(",")));
        }
        for command in &plugin.commands {
            lines.push(format!("  命令 /{}", command.name));
        }
    }
    lines.join("\n")
}

/// 插件停用提示：保留失败原因（可能是多行 JS 调用栈）与恢复方式。
pub(super) fn plugin_notice_note(plugin: &str, code: &str, message: &str) -> String {
    format!("插件 `{plugin}` 已停用（{code}）：{message}\n修复后 /plugins reload 可重载")
}
