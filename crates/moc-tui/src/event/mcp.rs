//! MCP 运行态查询、重连与诊断格式化。

use std::time::{SystemTime, UNIX_EPOCH};

use moc_protocol::methods::{
    McpList, McpListParams, McpReconnect, McpReconnectParams, McpServerInfo, McpServerPhase,
    McpStatusSnapshot,
};
use rust_i18n::t;

use crate::app::App;
use crate::protocol::{AppEvent, EmbeddedConnection};

/// 刷新当前工作区的 MCP 状态：`show` 为真时将详情写入转写（`/mcp`）。
pub(super) fn refresh_mcp(app: &App, connection: &EmbeddedConnection, show: bool) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let cwd = if session_id.is_none() && !app.startup_cwd.is_empty() {
        Some(app.startup_cwd.clone())
    } else {
        None
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = client
            .call::<McpList>(McpListParams { session_id, cwd })
            .await;
        match outcome {
            Ok(snapshot) => {
                let text = show.then(|| format_mcp_status(&snapshot, false));
                let _ = events.send(AppEvent::McpStatus(snapshot));
                if let Some(text) = text {
                    let _ = events.send(AppEvent::McpReport { text, error: false });
                }
            }
            Err(error) => {
                if show {
                    let _ = events.send(AppEvent::McpReport {
                        text: t!("tui.mcp.list_failed_fmt", err = error.to_string()).into_owned(),
                        error: true,
                    });
                }
            }
        }
    });
}

/// 手动重连指定或全部已启用的 MCP server（`/mcp reconnect [server]`）。
pub(super) fn reconnect_mcp(app: &App, connection: &EmbeddedConnection, server: Option<String>) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let cwd = if session_id.is_none() && !app.startup_cwd.is_empty() {
        Some(app.startup_cwd.clone())
    } else {
        None
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let outcome = client
            .call::<McpReconnect>(McpReconnectParams {
                session_id,
                cwd,
                server,
            })
            .await;
        match outcome {
            Ok(snapshot) => {
                let text = format_mcp_status(&snapshot, true);
                let _ = events.send(AppEvent::McpStatus(snapshot));
                let _ = events.send(AppEvent::McpReport { text, error: false });
            }
            Err(error) => {
                let _ = events.send(AppEvent::McpReport {
                    text: t!("tui.mcp.reconnect_failed_fmt", err = error.to_string()).into_owned(),
                    error: true,
                });
            }
        }
    });
}

pub(crate) fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub(crate) fn elapsed_secs(started_at_ms: Option<u64>, now_ms: u64) -> Option<u64> {
    started_at_ms.map(|started| now_ms.saturating_sub(started) / 1000)
}

/// 格式化 `/mcp` 与 `/mcp reconnect` 的只读诊断输出。
pub(super) fn format_mcp_status(snapshot: &McpStatusSnapshot, reconnected: bool) -> String {
    let now_ms = now_unix_ms();
    let mut lines = Vec::new();
    if let Some(error) = &snapshot.config_error {
        lines.push(t!("tui.mcp.config_error_fmt", err = error.as_str()).into_owned());
    }
    if snapshot.servers.is_empty() {
        if lines.is_empty() {
            return t!("tui.mcp.empty_hint").into_owned();
        }
        return lines.join("\n");
    }
    let connected = snapshot
        .servers
        .iter()
        .filter(|server| server.phase == McpServerPhase::Connected)
        .count();
    let connecting = snapshot
        .servers
        .iter()
        .filter(|server| {
            matches!(
                server.phase,
                McpServerPhase::Connecting | McpServerPhase::Reconnecting
            )
        })
        .count();
    let failed = snapshot
        .servers
        .iter()
        .filter(|server| server.phase == McpServerPhase::Failed)
        .count();
    let header_key = if reconnected {
        "tui.mcp.reconnected_header_fmt"
    } else {
        "tui.mcp.header_fmt"
    };
    lines.push(
        t!(
            header_key,
            total = snapshot.servers.len(),
            connected = connected,
            connecting = connecting,
            failed = failed
        )
        .into_owned(),
    );
    for server in &snapshot.servers {
        lines.push(format_server_line(server, now_ms));
        if server.phase != McpServerPhase::Connected && !server.recent_stderr.is_empty() {
            let tail_start = server.recent_stderr.len().saturating_sub(3);
            for stderr_line in &server.recent_stderr[tail_start..] {
                lines.push(format!("    stderr: {stderr_line}"));
            }
        }
    }
    if failed > 0 || connecting > 0 {
        lines.push(t!("tui.mcp.reconnect_hint").into_owned());
    }
    lines.join("\n")
}

fn format_server_line(server: &McpServerInfo, now_ms: u64) -> String {
    let elapsed = elapsed_secs(server.started_at_ms, now_ms)
        .map(|secs| format!(" ({secs}s)"))
        .unwrap_or_default();
    let detail = match server.phase {
        McpServerPhase::Connected => {
            t!("tui.mcp.phase_connected_fmt", tools = server.tool_count).into_owned()
        }
        McpServerPhase::Connecting => {
            t!("tui.mcp.phase_connecting_fmt", elapsed = elapsed.as_str()).into_owned()
        }
        McpServerPhase::Reconnecting => {
            let err = server.error.as_deref().unwrap_or("");
            t!(
                "tui.mcp.phase_reconnecting_fmt",
                attempts = server.attempts,
                elapsed = elapsed.as_str(),
                err = err
            )
            .into_owned()
        }
        McpServerPhase::Failed => {
            let err = server.error.as_deref().unwrap_or("");
            t!("tui.mcp.phase_failed_fmt", err = err).into_owned()
        }
        McpServerPhase::Disabled => t!("tui.mcp.phase_disabled").into_owned(),
    };
    let glyph = match server.phase {
        McpServerPhase::Connected => "●",
        McpServerPhase::Connecting => "◐",
        McpServerPhase::Reconnecting => "↻",
        McpServerPhase::Failed => "✕",
        McpServerPhase::Disabled => "○",
    };
    format!(
        "  {glyph} {} [{}] {} — {}",
        server.name, server.transport, server.target, detail
    )
}
