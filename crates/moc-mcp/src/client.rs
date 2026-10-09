//! rmcp 连接封装：transport 构建、客户端握手与 `tools/list_changed` 桥接。
//!
//! [`Connector`] 把「建一条连接」抽象为可重入闭包，监督任务每次重连都重新调用它；
//! 测试由此可以注入内存 transport 而无需拉起真实子进程。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use rmcp::ServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use tokio::sync::mpsc;

use crate::config::Transport;
use crate::error::McpError;

/// 一条已握手的客户端连接。
pub(crate) type Connection = RunningService<RoleClient, ClientHandlerBridge>;
/// 建立一条新连接的异步闭包；监督任务在每次（重）连接时调用。
pub(crate) type ConnectFuture = BoxFuture<'static, Result<Connection, McpError>>;
pub(crate) type Connector = Arc<dyn Fn(mpsc::UnboundedSender<()>) -> ConnectFuture + Send + Sync>;

/// 客户端 handler：只做两件事——自报身份、把 `tools/list_changed` 转发出去。
pub(crate) struct ClientHandlerBridge {
    tool_list_changed: mpsc::UnboundedSender<()>,
}

impl ClientHandlerBridge {
    pub(crate) fn new(tool_list_changed: mpsc::UnboundedSender<()>) -> Self {
        Self { tool_list_changed }
    }
}

impl ClientHandler for ClientHandlerBridge {
    fn get_info(&self) -> ClientConfig {
        let mut info = Implementation::from_build_env();
        info.name = "moc".into();
        ClientConfig::new(ClientCapabilities::default(), info)
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.tool_list_changed.send(());
    }
}

/// 按配置构建真实的 stdio / streamable HTTP 连接器。
pub(crate) fn default_connector(server: &str, transport: &Transport) -> Connector {
    let server = server.to_owned();
    let transport = transport.clone();
    Arc::new(move |tool_list_changed| {
        let server = server.clone();
        let transport = transport.clone();
        Box::pin(async move {
            match &transport {
                Transport::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                } => {
                    connect_stdio(
                        &server,
                        command,
                        args,
                        env,
                        cwd.as_deref(),
                        tool_list_changed,
                    )
                    .await
                }
                Transport::Http { url, headers } => {
                    connect_http(&server, url, headers, tool_list_changed).await
                }
            }
        })
    })
}

async fn connect_stdio(
    server: &str,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    tool_list_changed: mpsc::UnboundedSender<()>,
) -> Result<Connection, McpError> {
    let mut child = tokio::process::Command::new(command);
    child.args(args);
    for (key, value) in env {
        child.env(key, value);
    }
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    // stderr 默认继承会污染前端终端，必须接管后转入日志。
    let (transport, stderr) = TokioChildProcess::builder(child)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| McpError::Transport(format!("failed to start `{command}`: {error}")))?;
    if let Some(stderr) = stderr {
        let server = server.to_owned();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "moc_mcp", server, "{line}");
            }
        });
    }
    let handler = ClientHandlerBridge::new(tool_list_changed);
    handler
        .serve(transport)
        .await
        .map_err(|error| McpError::Transport(format!("handshake with `{server}` failed: {error}")))
}

async fn connect_http(
    server: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    tool_list_changed: mpsc::UnboundedSender<()>,
) -> Result<Connection, McpError> {
    let mut custom_headers = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        let parsed_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            McpError::Config(format!(
                "server `{server}` header name `{name}` is invalid: {error}"
            ))
        })?;
        let parsed_value = http::HeaderValue::from_str(value).map_err(|error| {
            McpError::Config(format!(
                "server `{server}` header `{name}` has an invalid value: {error}"
            ))
        })?;
        custom_headers.insert(parsed_name, parsed_value);
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_owned());
    config.custom_headers = custom_headers;
    let transport = StreamableHttpClientTransport::with_client(reqwest::Client::new(), config);
    let handler = ClientHandlerBridge::new(tool_list_changed);
    handler
        .serve(transport)
        .await
        .map_err(|error| McpError::Transport(format!("handshake with `{server}` failed: {error}")))
}
