//! MCP 运行态查询与手动重连协议操作。

use std::path::PathBuf;

use moc_protocol::methods::{McpListParams, McpReconnectParams, McpStatusSnapshot};
use moc_protocol::{RequestContext, RpcError};

use super::Core;
use crate::service::to_rpc;

async fn resolve_cwd(
    core: &Core,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Result<PathBuf, RpcError> {
    if let Some(session_id) = session_id {
        let record = core.inner.sessions.get(session_id).await.map_err(to_rpc)?;
        let cwd = record.state.lock().await.session.cwd.clone();
        return Ok(PathBuf::from(cwd));
    }
    if let Some(cwd) = cwd {
        return Ok(PathBuf::from(cwd));
    }
    Ok(core.inner.config().cwd.clone())
}

pub(super) async fn mcp_list(
    core: &Core,
    ctx: RequestContext,
    params: McpListParams,
) -> Result<McpStatusSnapshot, RpcError> {
    core.inner
        .register_mcp_sink(ctx.connection_id(), ctx.updates());
    let cwd = resolve_cwd(core, params.session_id.as_deref(), params.cwd.as_deref()).await?;
    core.inner
        .mcp_status_snapshot(&cwd, true)
        .await
        .map_err(to_rpc)
}

pub(super) async fn mcp_reconnect(
    core: &Core,
    ctx: RequestContext,
    params: McpReconnectParams,
) -> Result<McpStatusSnapshot, RpcError> {
    core.inner
        .register_mcp_sink(ctx.connection_id(), ctx.updates());
    let cwd = resolve_cwd(core, params.session_id.as_deref(), params.cwd.as_deref()).await?;
    core.inner
        .mcp_reconnect(&cwd, params.server.as_deref())
        .await
        .map_err(to_rpc)
}

#[cfg(test)]
mod tests {
    use crate::{Core, CoreConfig};
    use moc_protocol::methods::{
        McpList, McpListParams, McpReconnect, McpReconnectParams, McpStatusSnapshot,
        McpStatusUpdated,
    };
    use moc_protocol::{ClientBuilder, ClientInfo, Server, transport};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mcp_list_and_reconnect_report_config_errors_and_recover() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        // 写入损坏的 mcp.json：必须向前端暴露 config_error，且修复后经 mcp/reconnect 清除。
        let mcp_json = home.join("mcp.json");
        std::fs::write(&mcp_json, "{ not valid json").unwrap();

        let boot = CoreConfig {
            home,
            cwd: work.clone(),
            user_home: Some(root.path().join("userhome")),
            session_dir: Some(root.path().join("sessions")),
            ..Default::default()
        };
        let core =
            Core::new_with_sandbox_support(boot, crate::sandbox::SandboxSupport::full_only())
                .unwrap();
        let server = Server::new((*core).clone());
        let (server_side, client_side) = transport::duplex(65536);
        let connection = server.spawn(server_side);

        let (tx, mut rx) = mpsc::unbounded_channel::<McpStatusSnapshot>();
        let (client, _) = ClientBuilder::new(ClientInfo {
            name: "mcp-api-test".into(),
            version: "1".into(),
            frontend: "test".into(),
        })
        .on_notification::<McpStatusUpdated, _, _>(move |snapshot, _ctx| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(snapshot);
            }
        })
        .connect(client_side)
        .await
        .unwrap();

        let initial = client
            .call::<McpList>(McpListParams::default())
            .await
            .unwrap();
        assert!(
            initial.config_error.is_some(),
            "损坏的 mcp.json 必须返回 config_error：{initial:?}"
        );
        // initialize 预热也应主动推送包含 config_error 的状态通知。
        let pushed = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("应收到初始 MCP 状态推送")
            .unwrap();
        assert!(pushed.config_error.is_some());

        // 修复 mcp.json 并触发 mcp/reconnect 后 config_error 清除。
        std::fs::write(&mcp_json, r#"{"mcpServers":{}}"#).unwrap();
        let recovered = client
            .call::<McpReconnect>(McpReconnectParams::default())
            .await
            .unwrap();
        assert!(
            recovered.config_error.is_none(),
            "修复后 mcp/reconnect 应清除 config_error：{recovered:?}"
        );
        assert!(recovered.servers.is_empty());

        client.close().await;
        connection.close();
        connection.wait().await;
        Arc::clone(&core).shutdown().await;
    }
}
