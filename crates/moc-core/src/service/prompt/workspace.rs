//! 工作区检查：扫描范围、外部改动，以及只读模式下的违规通知。

use crate::{
    service::{SessionRuntime, protocol_to_rpc, snapshot_workspace, to_rpc},
    workspace::watch::WorkspaceSnapshot,
};
use moc_protocol::{RpcError, methods::SessionUpdate};

/// 一次工作区检查中随提示变化的输入。
///
/// `contract` 是**本次提示实际生效**的执行契约：`/ask` 这类只对单次提示生效的覆盖
/// 不会写入会话状态，所以判断只读违规必须用这里的值，不能读 `record.state`。
pub(super) struct WorkspaceScope<'a> {
    pub session_id: &'a str,
    pub cwd: &'a std::path::Path,
    pub config: &'a crate::ResolvedConfig,
    pub contract: crate::service::mode::ModeContract,
    /// 工具批次执行前的工作区快照；`None` 表示不排除工具自己造成的改动。
    pub before_tools: Option<&'a WorkspaceSnapshot>,
}

pub(super) async fn check_workspace(
    update: &moc_protocol::UpdateSink,
    runtime: &SessionRuntime,
    record: &crate::session::SessionRecord,
    scope: WorkspaceScope<'_>,
    plugins: &moc_plugin::PluginHost,
    plugin_context: &moc_plugin::CallContext,
) -> Result<(), RpcError> {
    let WorkspaceScope {
        session_id,
        cwd,
        config,
        contract,
        before_tools,
    } = scope;
    if !config.workspace.watch {
        return Ok(());
    }
    let after = snapshot_workspace(cwd.to_owned(), config.workspace.max_scan_entries)
        .await
        .map_err(to_rpc)?;
    if after.truncated {
        let notice = format!(
            "Workspace scan hit its {}-entry limit; unscanned paths are not treated as deleted.",
            config.workspace.max_scan_entries
        );
        let mut state = record.state.lock().await;
        if !state.notices.contains(&notice) {
            state.notices.push(notice);
        }
    }
    let mut previous = runtime.workspace.lock().await;
    let mut changes = previous
        .as_ref()
        .map(|before| crate::workspace::watch::diff(before, &after))
        .unwrap_or_default();
    let mut progressed = !changes.is_empty();
    let violation = contract.read_only();
    if !violation && let Some(before_tools) = before_tools {
        let tool_changes = crate::workspace::watch::diff(before_tools, &after);
        progressed |= !tool_changes.is_empty();
        changes.retain(|change| !tool_changes.iter().any(|owned| owned.path == change.path));
    }
    *previous = Some(after);
    drop(previous);
    if progressed && let Some(guard) = runtime.guard.lock().clone() {
        guard.record_change();
    }
    if changes.is_empty() {
        return Ok(());
    }
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::WorkspaceChanged {
                violation,
                changes: changes.clone(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    crate::service::plugin_events::workspace_changed(plugins, plugin_context, violation, &changes)
        .await;
    let paths = changes
        .iter()
        .map(|change| {
            let status = match change.kind.as_str() {
                "added" => "??",
                "removed" => "D",
                _ => "M",
            };
            format!("{status} {}", change.path)
        })
        .collect::<Vec<_>>()
        .join(", ");
    record.state.lock().await.notices.push(if violation {
        format!("You modified the workspace in read-only mode: {paths}. Stop and explain why to the user.")
    } else {
        format!("The workspace changed outside this prompt: {paths}.")
    });
    Ok(())
}
