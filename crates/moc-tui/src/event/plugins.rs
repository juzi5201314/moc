//! 插件目录刷新与状态展示。

use moc_i18n::t;

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
                .map_err(|error| {
                    t!("tui.plugins.reload_failed_fmt", err = error.to_string()).into_owned()
                })
        } else {
            client
                .call::<PluginList>(PluginListParams { session_id })
                .await
                .map(|result| result.plugins)
                .map_err(|error| {
                    t!("tui.plugins.list_failed_fmt", err = error.to_string()).into_owned()
                })
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
        return t!("tui.plugins.empty_hint").into_owned();
    }
    t!(
        "tui.plugins.loaded_fmt",
        n = plugins.len(),
        lines = format_plugin_lines(plugins)
    )
    .into_owned()
}

/// `/plugins reload` 的正文：重载结果的可用计数，后接每个插件的明细。
pub(super) fn reload_plugins_note(plugins: &[moc_protocol::methods::PluginInfo]) -> String {
    if plugins.is_empty() {
        return t!("tui.plugins.reloaded_empty").into_owned();
    }
    let disabled = plugins
        .iter()
        .filter(|plugin| plugin.error.is_some())
        .count();
    t!(
        "tui.plugins.reloaded_fmt",
        ok = plugins.len() - disabled,
        disabled = disabled,
        lines = format_plugin_lines(plugins)
    )
    .into_owned()
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
        lines.push(
            t!(
                "tui.plugins.line_fmt",
                name = plugin.name.as_str(),
                version = version.as_str(),
                source = plugin.source.as_str(),
                desc = plugin.description.as_deref().unwrap_or_default()
            )
            .into_owned(),
        );
        lines.push(format!("  {}", plugin.dir));
        if let Some(error) = &plugin.error {
            lines.push(t!("tui.plugins.disabled_fmt", err = error.as_str()).into_owned());
            continue;
        }
        for tool in &plugin.tools {
            lines.push(
                t!(
                    "tui.plugins.tool_fmt",
                    name = tool.name.as_str(),
                    modes = tool.modes.join(",")
                )
                .into_owned(),
            );
        }
        for command in &plugin.commands {
            lines.push(t!("tui.plugins.cmd_fmt", name = command.name.as_str()).into_owned());
        }
    }
    lines.join("\n")
}

/// 插件停用提示：保留失败原因（可能是多行 JS 调用栈）与恢复方式。
pub(super) fn plugin_notice_note(plugin: &str, code: &str, message: &str) -> String {
    t!(
        "tui.plugins.notice_fmt",
        plugin = plugin,
        code = code,
        message = message
    )
    .into_owned()
}
