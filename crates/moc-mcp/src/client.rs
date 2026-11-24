//! rmcp 连接封装：transport 构建、客户端握手与 `tools/list_changed` 桥接。
//!
//! [`Connector`] 把「建一条连接」抽象为可重入闭包，监督任务每次重连都重新调用它；
//! 测试由此可以注入内存 transport 而无需拉起真实子进程。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
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
/// stdio server 最近的 stderr 尾部缓冲（有界环形队列，供状态诊断展示）。
pub(crate) type StderrTail = Arc<Mutex<VecDeque<String>>>;

const MAX_STDERR_TAIL_LINES: usize = 8;
const MAX_STDERR_TAIL_CHARS: usize = 240;

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
///
/// HTTP 传输在连接器级别复用同一个带 `connect_timeout` 的 `reqwest::Client`，
/// 避免每次重连重建 TLS 根证书库和连接池。
pub(crate) fn default_connector(
    server: &str,
    transport: &Transport,
    connect_timeout: Duration,
    stderr_tail: StderrTail,
) -> Connector {
    let server = server.to_owned();
    let transport = transport.clone();
    let http_client = matches!(transport, Transport::Http { .. }).then(|| {
        reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });
    Arc::new(move |tool_list_changed| {
        let server = server.clone();
        let transport = transport.clone();
        let http_client = http_client.clone();
        let stderr_tail = Arc::clone(&stderr_tail);
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
                        stderr_tail,
                    )
                    .await
                }
                Transport::Http { url, headers } => {
                    connect_http(
                        &server,
                        url,
                        headers,
                        http_client.unwrap_or_else(reqwest::Client::new),
                        tool_list_changed,
                    )
                    .await
                }
            }
        })
    })
}

#[cfg(unix)]
struct StdioGroupTransport {
    inner: TokioChildProcess,
    pgid: Option<libc::pid_t>,
}

#[cfg(unix)]
impl StdioGroupTransport {
    fn new(inner: TokioChildProcess) -> Self {
        let pgid = inner
            .id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            .filter(|pid| *pid > 1);
        Self { inner, pgid }
    }
}

#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, sig: libc::c_int) -> bool {
    // SAFETY: 子进程经 `process_group(0)` 独占进程组且 `pgid > 1`；
    // 负 PID 只作用于该 MCP 子进程及其孙进程（如 `npx`/`uvx` 拉起的真实服务）。
    unsafe { libc::kill(-pgid, sig) == 0 }
}

#[cfg(unix)]
impl Drop for StdioGroupTransport {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            // 超时或取消丢弃时立刻回收整组进程，防止孙进程泄漏为孤儿。
            let _ = signal_group(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
impl rmcp::transport::Transport<RoleClient> for StdioGroupTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<RoleClient>>> + Send
    {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        async move {
            let result = self.inner.graceful_shutdown().await;
            if let Some(pgid) = self.pgid.take()
                && signal_group(pgid, 0)
            {
                let _ = signal_group(pgid, libc::SIGTERM);
                let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                while tokio::time::Instant::now() < deadline && signal_group(pgid, 0) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                if signal_group(pgid, 0) {
                    let _ = signal_group(pgid, libc::SIGKILL);
                }
            }
            result
        }
    }
}

async fn connect_stdio(
    server: &str,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
    tool_list_changed: mpsc::Sender<()>,
    stderr_tail: StderrTail,
) -> Result<Connection, McpError> {
    let mut child = tokio::process::Command::new(command);
    child.args(args);
    for (key, value) in env {
        child.env(key, value);
    }
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    // Unix 下让子进程独占进程组：超时取消或关闭时可一并清理 `npx`/`uvx` 等派生孙进程，
    // 同时也避免终端作业控制信号把无控制台的 stdio server 挂起。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.as_std_mut().process_group(0);
    }
    // stderr 默认继承会污染前端终端，必须接管后转入日志。
    let (transport, stderr) = TokioChildProcess::builder(child)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| McpError::Transport(format!("failed to start `{command}`: {error}")))?;
    if let Some(stderr) = stderr {
        let server = server.to_owned();
        tokio::spawn(async move {
            drain_stderr(
                tokio::io::BufReader::new(stderr),
                &server,
                Some(stderr_tail),
            )
            .await;
        });
    }
    let handler = ClientHandlerBridge::new(tool_list_changed);
    #[cfg(unix)]
    {
        serve_bridge(server, handler, StdioGroupTransport::new(transport)).await
    }
    #[cfg(not(unix))]
    {
        serve_bridge(server, handler, transport).await
    }
}

// 限制单条 stderr 日志的内存占用，同时仍完整排空超长行，避免阻塞子进程。
// 正文最多 MAX 字节，行终止符最多额外保留 CRLF 两字节。
const MAX_LOG_LINE_BYTES: usize = 8 * 1024;

async fn read_log_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    buf: &mut Vec<u8>,
    truncated: &mut bool,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;

    buf.clear();
    *truncated = false;
    let mut bytes_read = 0usize;
    let mut pending_cr = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if pending_cr {
                if buf.len() < MAX_LOG_LINE_BYTES {
                    buf.push(b'\r');
                } else {
                    *truncated = true;
                }
            }
            return Ok(bytes_read);
        }

        if pending_cr {
            if available[0] == b'\n' {
                // 跨块 CRLF 的 CR 属于终止符，不占正文上限。
                buf.push(b'\r');
            } else if buf.len() < MAX_LOG_LINE_BYTES {
                // 后续字节不是 LF，待定 CR 属于正文。
                buf.push(b'\r');
            } else {
                *truncated = true;
            }
            pending_cr = false;
        }

        let line_ending_index = available.iter().position(|byte| *byte == b'\n');
        let chunk_len = line_ending_index.map_or(available.len(), |index| index + 1);
        let trailing_cr = line_ending_index.is_none() && available.ends_with(b"\r");
        let terminator_len = match line_ending_index {
            Some(index) if index > 0 && available[index - 1] == b'\r' => 2,
            Some(_) => 1,
            None if trailing_cr => 1,
            None => 0,
        };
        let content_len = chunk_len - terminator_len;
        let remaining_capacity = MAX_LOG_LINE_BYTES.saturating_sub(buf.len());
        let content_to_keep = content_len.min(remaining_capacity);
        buf.extend_from_slice(&available[..content_to_keep]);
        *truncated |= content_to_keep < content_len;
        if content_len <= remaining_capacity {
            if trailing_cr {
                pending_cr = true;
            } else {
                buf.extend_from_slice(&available[content_len..chunk_len]);
            }
        }
        let line_ended = line_ending_index.is_some();
        reader.consume(chunk_len);
        bytes_read = bytes_read.saturating_add(chunk_len);

        if line_ended {
            return Ok(bytes_read);
        }
    }
}

fn render_log_line(buf: &[u8], truncated: bool) -> Cow<'_, str> {
    let line = String::from_utf8_lossy(buf);
    if truncated {
        let mut owned = line.into_owned();
        owned.push_str("…[truncated]");
        Cow::Owned(owned)
    } else {
        line
    }
}

fn push_stderr_tail(tail: &StderrTail, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut chars = trimmed.chars();
    let mut clipped: String = chars.by_ref().take(MAX_STDERR_TAIL_CHARS).collect();
    if chars.next().is_some() {
        clipped.push('…');
    }
    let mut guard = tail.lock();
    if guard.len() >= MAX_STDERR_TAIL_LINES {
        guard.pop_front();
    }
    guard.push_back(clipped);
}

async fn drain_stderr(
    mut reader: impl tokio::io::AsyncBufRead + Unpin,
    server: &str,
    tail: Option<StderrTail>,
) {
    let mut buf = Vec::new();
    let mut truncated = false;
    loop {
        match read_log_line(&mut reader, &mut buf, &mut truncated).await {
            Ok(0) => break,
            Ok(_) => {
                if buf.ends_with(b"\n") {
                    buf.pop();
                    if buf.ends_with(b"\r") {
                        buf.pop();
                    }
                }
                // 非 UTF-8 日志也必须持续读取，避免提前关闭管道导致子进程写入失败。
                let line = render_log_line(&buf, truncated);
                if let Some(tail) = &tail {
                    push_stderr_tail(tail, &line);
                }
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
    client: reqwest::Client,
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
    let transport = StreamableHttpClientTransport::with_client(client, config);
    let handler = ClientHandlerBridge::new(tool_list_changed);
    serve_bridge(server, handler, transport).await
}

#[cfg(test)]
mod tests {
    use super::{MAX_LOG_LINE_BYTES, drain_stderr, read_log_line, render_log_line};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn stderr_long_line_is_bounded_and_next_line_is_read() {
        let mut input = vec![b'x'; MAX_LOG_LINE_BYTES + 1024];
        input.extend_from_slice(b"\nnext line\n");
        let mut reader = BufReader::with_capacity(64, std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut truncated = false;

        let first_len = read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();
        assert_eq!(first_len, MAX_LOG_LINE_BYTES + 1025);
        assert_eq!(buf.len(), MAX_LOG_LINE_BYTES + 1);
        assert!(buf.ends_with(b"\n"));
        assert!(truncated);
        assert!(render_log_line(&buf, truncated).ends_with("…[truncated]"));

        let second_len = read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();
        assert_eq!(second_len, b"next line\n".len());
        assert_eq!(buf, b"next line\n");
        assert!(!truncated);
    }

    #[tokio::test]
    async fn stderr_body_limit_excludes_lf_and_crlf_terminators() {
        for (input, expected_suffix) in [
            (vec![b'x'; MAX_LOG_LINE_BYTES], b"\n".as_slice()),
            (vec![b'x'; MAX_LOG_LINE_BYTES - 1], b"\r\n".as_slice()),
        ] {
            let mut input = input;
            input.extend_from_slice(expected_suffix);
            let mut reader = BufReader::with_capacity(64, std::io::Cursor::new(input));
            let mut buf = Vec::new();
            let mut truncated = false;

            read_log_line(&mut reader, &mut buf, &mut truncated)
                .await
                .unwrap();

            assert!(!truncated);
            assert!(buf.ends_with(expected_suffix));
        }
    }

    #[tokio::test]
    async fn stderr_crlf_at_chunk_boundary_preserves_maximum_body() {
        let mut input = vec![b'x'; MAX_LOG_LINE_BYTES];
        input.extend_from_slice(b"\r\n");
        let mut reader =
            BufReader::with_capacity(MAX_LOG_LINE_BYTES + 1, std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut truncated = false;

        read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert!(!truncated);
        assert_eq!(buf.len(), MAX_LOG_LINE_BYTES + 2);
        assert!(buf[..MAX_LOG_LINE_BYTES].iter().all(|byte| *byte == b'x'));
        assert!(buf.ends_with(b"\r\n"));
    }

    #[tokio::test]
    async fn stderr_eof_after_maximum_body_and_trailing_carriage_return_is_truncated() {
        let mut input = vec![b'x'; MAX_LOG_LINE_BYTES];
        input.push(b'\r');
        let mut reader = BufReader::with_capacity(MAX_LOG_LINE_BYTES, std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut truncated = false;

        read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert!(truncated);
        assert_eq!(buf.len(), MAX_LOG_LINE_BYTES);
        assert!(buf[..MAX_LOG_LINE_BYTES].iter().all(|byte| *byte == b'x'));
    }

    #[tokio::test]
    async fn stderr_maximum_body_pending_cr_followed_by_body_is_truncated_and_drained() {
        let mut input = vec![b'x'; MAX_LOG_LINE_BYTES];
        input.extend_from_slice(b"\rbody\nnext\n");
        let mut reader =
            BufReader::with_capacity(MAX_LOG_LINE_BYTES + 1, std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut truncated = false;

        let first_len = read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert_eq!(first_len, MAX_LOG_LINE_BYTES + b"\rbody\n".len());
        assert!(truncated);
        assert_eq!(buf.len(), MAX_LOG_LINE_BYTES);
        assert!(buf.iter().all(|byte| *byte == b'x'));

        let second_len = read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert_eq!(second_len, b"next\n".len());
        assert_eq!(buf, b"next\n");
        assert!(!truncated);
    }

    #[tokio::test]
    async fn stderr_pending_cr_followed_by_body_is_kept_when_under_limit() {
        let mut reader = BufReader::with_capacity(2, std::io::Cursor::new(b"a\rb\n"));
        let mut buf = Vec::new();
        let mut truncated = false;

        read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert!(!truncated);
        assert_eq!(buf, b"a\rb\n");
    }

    #[tokio::test]
    async fn stderr_truncated_line_preserves_trailing_carriage_return() {
        let mut input = vec![b'x'; MAX_LOG_LINE_BYTES - 1];
        input.extend_from_slice(b"\rmore\n");
        let mut reader = BufReader::with_capacity(64, std::io::Cursor::new(input));
        let mut buf = Vec::new();
        let mut truncated = false;

        read_log_line(&mut reader, &mut buf, &mut truncated)
            .await
            .unwrap();

        assert!(truncated);
        assert_eq!(buf.len(), MAX_LOG_LINE_BYTES);
        assert!(buf.ends_with(b"\r"));
        let rendered = render_log_line(&buf, truncated);
        let rendered_body = rendered.strip_suffix("…[truncated]").unwrap();
        assert!(rendered_body.ends_with('\r'));
    }

    #[tokio::test]
    async fn stderr_drain_continues_after_invalid_utf8() {
        let input = b"before\ninvalid \xff\r\nafter\nlast \xff".to_vec();
        let mut reader = std::io::Cursor::new(input);

        drain_stderr(&mut reader, "test", None).await;

        assert_eq!(reader.position(), reader.get_ref().len() as u64);
    }
}
