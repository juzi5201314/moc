//! 工作区覆盖范围、外部变化与只读模式违规通知。

use crate::{
    service::{SessionRuntime, protocol_to_rpc, snapshot_workspace, to_rpc},
    workspace::watch::WorkspaceSnapshot,
};
use moc_protocol::{RpcError, methods::SessionUpdate};

/// 一次工作区检查中随提示变化的输入。
///
/// `mode` 是**本次提示实际生效**的模式：`/ask` 这类按提示注入的覆盖不会写入会话状态，
/// 只读违规判定必须跟着本次提示走，不能回读 `record.state`。
pub(super) struct WorkspaceScope<'a> {
    pub session_id: &'a str,
    pub cwd: &'a std::path::Path,
    pub config: &'a crate::ResolvedConfig,
    pub mode: moc_protocol::methods::SessionMode,
    /// 工具批次执行前的工作区快照；`None` 表示本次检查不忽略工具自身改动。
    pub before_tools: Option<&'a WorkspaceSnapshot>,
}

pub(super) async fn check_workspace(
    update: &moc_protocol::UpdateSink,
    runtime: &SessionRuntime,
    record: &crate::session::SessionRecord,
    scope: WorkspaceScope<'_>,
) -> Result<(), RpcError> {
    let WorkspaceScope {
        session_id,
        cwd,
        config,
        mode,
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
            "工作区过大，检测仅覆盖部分路径（扫描上限 {}）；未扫描路径不会被判为删除。",
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
    let violation = matches!(
        mode,
        moc_protocol::methods::SessionMode::Ask | moc_protocol::methods::SessionMode::Plan
    );
    if !violation && let Some(before_tools) = before_tools {
        let tool_changes = crate::workspace::watch::diff(before_tools, &after);
        changes.retain(|change| !tool_changes.iter().any(|owned| owned.path == change.path));
    }
    *previous = Some(after);
    drop(previous);
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
        format!("你在只读模式下修改了工作区：{paths}。立即停止修改并向用户说明原因。")
    } else {
        format!("工作区在本次提示之外发生了变更：{paths}。")
    });
    Ok(())
}
