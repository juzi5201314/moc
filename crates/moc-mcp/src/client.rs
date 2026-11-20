//! rmcp 连接封装：transport 构建、客户端握手与 `tools/list_changed` 桥接。
//!
//! [`Connector`] 把「建一条连接」抽象为可重入闭包，监督任务每次重连都重新调用它；
//! 测试由此可以注入内存 transport 而无需拉起真实子进程。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use rmcp::ClientServiceExt;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitationCapability, FormElicitationCapability,
    Implementation, ProtocolVersion, UrlElicitationCapability,
};
use rmcp::service::{ClientLifecycleMode, NotificationContext, RoleClient, RunningService};
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
pub(crate) type Connector = Arc<dyn Fn(mpsc::Sender<()>) -> ConnectFuture + Send + Sync>;

/// 客户端 handler：只做两件事——自报身份、把 `tools/list_changed` 转发出去。
pub(crate) struct ClientHandlerBridge {
    tool_list_changed: mpsc::Sender<()>,
}

/// 启动握手：先 `server/discover` 协商（2026-07-28 的 inline 生命周期），
/// 对端不支持 discover 时回退 legacy `initialize`。MRTR（SEP-2322）只在
/// 协商到 2026-07-28 的连接上启用，legacy 路径行为与此前一致。
pub(crate) async fn serve_bridge<T, E, A>(
    server: &str,
    handler: ClientHandlerBridge,
    transport: T,
) -> Result<Connection, McpError>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    handler
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                // 新→旧全量列出：discover-capable 的旧版服务端也能选中共同版本，
                // 而不是因仅支持旧版而直接失败。
                preferred_versions: ProtocolVersion::KNOWN_VERSIONS
                    .iter()
                    .rev()
                    .cloned()
                    .collect(),
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        )
        .await
        .map_err(|error| McpError::Transport(format!("handshake with `{server}` failed: {error}")))
}

impl ClientHandlerBridge {
    pub(crate) fn new(tool_list_changed: mpsc::Sender<()>) -> Self {
        Self { tool_list_changed }
    }
}

impl ClientHandler for ClientHandlerBridge {
    fn get_info(&self) -> ClientConfig {
        let mut info = Implementation::from_build_env();
        info.name = "moc".into();
        // 只声明能完整兑现的能力：elicitation 表单/URL 两种模式都由
        // `McpManager::call` 注入的 fulfiller 经 `session/request_ask` 落到用户；
        // sampling 与 roots 不声明（sampling 需要嵌套模型调用，roots 已 deprecated）。
        // 注意旧协议版本下的独立 `elicitation/create` 请求仍会进来，由 rmcp
        // 默认 handler 回 decline——合法降级。
        let mut capabilities = ClientCapabilities::default();
        capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new())
                .with_url(UrlElicitationCapability::new()),
        );
        ClientConfig::new(capabilities, info)
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // 队列只保留一次待刷新标记；刷新期间的新通知仍会触发下一次列举。
        let _ = self.tool_list_changed.try_send(());
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
    tool_list_changed: mpsc::Sender<()>,
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
            drain_stderr(tokio::io::BufReader::new(stderr), &server).await;
        });
    }
    let handler = ClientHandlerBridge::new(tool_list_changed);
    serve_bridge(server, handler, transport).await
}

async fn drain_stderr(mut reader: impl tokio::io::AsyncBufRead + Unpin, server: &str) {
    use tokio::io::AsyncBufReadExt;

    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                if buf.ends_with(b"\n") {
                    buf.pop();
                    if buf.ends_with(b"\r") {
                        buf.pop();
                    }
                }
                // 非 UTF-8 日志也必须持续读取，避免提前关闭管道导致子进程写入失败。
                let line = String::from_utf8_lossy(&buf);
                tracing::debug!(target: "moc_mcp", server, "{line}");
            }
            Err(error) => {
                tracing::debug!(target: "moc_mcp", server, %error, "读取 MCP stderr 失败");
                break;
            }
        }
    }
}

async fn connect_http(
    server: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    tool_list_changed: mpsc::Sender<()>,
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
    serve_bridge(server, handler, transport).await
}

#[cfg(test)]
mod tests {
    use super::drain_stderr;

    #[tokio::test]
    async fn stderr_drain_continues_after_invalid_utf8() {
        let input = b"before\ninvalid \xff\r\nafter\nlast \xff".to_vec();
        let mut reader = std::io::Cursor::new(input);

        drain_stderr(&mut reader, "test").await;

        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }
}
