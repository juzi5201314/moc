//! `moc web`：把一个 core 进程的 JSON-RPC 服务桥到浏览器 WebSocket。
//!
//! 这里没有 daemon，也没有多路复用：`moc web` 自己就是一个 core 进程，和
//! `moc`（TUI）平级。多个浏览器标签共享同一个 core，同一会话的写互斥由 core
//! 内的提示锁负责；另一个 `moc` 进程同时打开同一会话时，由跨进程写租约裁决。
//!
//! 传输格式是 NDJSON（一行一个 JSON-RPC 报文），与 TUI 内嵌连接一致。前端由
//! pnpm 构建（`crates/moc-web/web`），产物经 `build.rs` 嵌入二进制，部署仍只需一个可执行文件。

mod assets;
mod bridge;
mod browser;
mod commands;

use std::net::Ipv4Addr;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use moc_core::Core;
use moc_protocol::Server;
use serde::Deserialize;
use tokio::net::TcpListener;

/// 未指定 `--port` 时优先尝试的端口。
pub const DEFAULT_PORT: u16 = 7432;

/// `moc web` 的启动参数。
#[derive(Debug, Clone)]
pub struct WebOptions {
    /// 优先监听端口；只绑定回环地址。`0` 表示由系统分配。
    pub port: u16,
    /// 端口被占用时是否退回系统分配的随机端口。
    /// 显式 `--port` 为 `false`：调用方指定了端口，换了就不算兑现。
    pub fallback: bool,
    /// 访问令牌：浏览器必须用 `?token=` 带上它才能建立 WebSocket。
    pub token: String,
    /// 启动后用系统默认浏览器打开 WebUI。
    pub open_browser: bool,
}

/// 启动 Web 前端，直到进程退出。
pub async fn run(core: Arc<Core>, options: WebOptions) -> anyhow::Result<()> {
    let (listener, fell_back) = listen(options.port, options.fallback)
        .await
        .with_context(|| format!("无法监听 127.0.0.1:{}；端口被占用或权限不足", options.port))?;
    let addr = listener.local_addr()?;
    if fell_back {
        println!("端口 {} 已被占用，改用随机端口 {addr}", options.port);
    }
    tracing::info!(%addr, "moc web 已启动");

    let server = Server::with_options((*core).clone(), bridge::options());
    let state = AppState {
        token: Arc::new(options.token.clone()),
        server,
    };

    let url = format!("http://{addr}/?token={}", options.token);
    println!("moc web 已启动：{url}");
    if options.open_browser {
        match browser::open(&url) {
            Ok(true) => println!("已在浏览器中打开；如未弹出，请手动访问上面的地址"),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "自动打开浏览器失败"),
        }
    }
    println!("按 Ctrl+C 停止服务");
    axum::serve(listener, routes(state)).await?;
    Ok(())
}

/// 绑定回环地址：先试 `port`，被占用且允许回退时改由系统分配。
/// 第二个返回值表示是否回退过，供调用方把实际端口告诉用户。
async fn listen(port: u16, fallback: bool) -> std::io::Result<(TcpListener, bool)> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Err(error) if fallback && error.kind() == std::io::ErrorKind::AddrInUse => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
            Ok((listener, true))
        }
        result => result.map(|listener| (listener, false)),
    }
}

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    server: Arc<Server<Core>>,
}

impl AppState {
    fn authorized(&self, token: Option<&str>) -> bool {
        token.is_some_and(|token| constant_time_eq(token.as_bytes(), self.token.as_bytes()))
    }
}

fn routes(state: AppState) -> Router {
    let ws_route = get(ws_handler).route_layer(axum::middleware::from_fn_with_state(
        state.clone(),
        require_token,
    ));
    Router::new()
        .route("/ws", ws_route)
        .route("/commands.json", get(commands_handler))
        .fallback(get(static_handler))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

/// 页面只加载自身资源；连接层放行回环 WebSocket（`ws` 直连在 'self' 之外的兜底）。
/// `frame-ancestors`/`frame-src` 双断 iframe 嵌套：本应用自己也不内嵌任何页面。
const CSP: &str = concat!(
    "default-src 'self'",
    "; script-src 'self'",
    "; style-src 'self' 'unsafe-inline'",
    "; img-src 'self' data: blob: https: http:",
    "; font-src 'self' data:",
    "; connect-src 'self' ws://127.0.0.1:* ws://localhost:* ws://[::1]:* wss://127.0.0.1:* wss://localhost:* wss://[::1]:*",
    "; object-src 'none'",
    "; frame-src 'none'",
    "; base-uri 'self'",
    "; form-action 'self'",
    "; frame-ancestors 'none'",
);

/// 所有响应统一的安全头：HTML、静态资源、JSON 与错误页一视同仁，新路由自动继承。
const SECURITY_HEADERS: [(header::HeaderName, &str); 5] = [
    (header::CONTENT_SECURITY_POLICY, CSP),
    // frame-ancestors 的旧式兜底；现代浏览器以 CSP 为准。
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    // 页面会打开图片等外部链接；Referer 只泄本机源，不带任何路径信息。
    (header::REFERRER_POLICY, "same-origin"),
    (
        header::HeaderName::from_static("permissions-policy"),
        "camera=(), microphone=(), geolocation=()",
    ),
];

fn apply_security_headers(headers: &mut HeaderMap) {
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

/// 挂在整棵路由树上的中间件；/ws 的拒绝响应、404 与 /commands.json 同样带头。
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    apply_security_headers(response.headers_mut());
    response
}

/// 内置斜杠命令元数据（注册表的可序列化视图）：与静态资源一样是公开元数据，
/// 不需要令牌；不缓存，升级后立刻生效。
async fn commands_handler() -> Response {
    let body = serde_json::to_vec(&commands::command_metas()).expect("命令元数据可序列化");
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// 静态资源：精确命中嵌入表；`/assets/` 下的文件名带内容哈希，可永久缓存。
/// `/assets/` 下未命中回 404（旧版本的过期 chunk 不能拿到 HTML）；
/// 其余未命中回 `index.html`（SPA 回退），且不缓存，保证升级后拿到新入口。
async fn static_handler(uri: Uri, headers: HeaderMap) -> Response {
    let accept_zstd = accepts_zstd(&headers);
    match assets::lookup(uri.path()) {
        Some(asset) => render(asset, cache_control(asset.path()), accept_zstd),
        None if uri.path().starts_with("/assets/") => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "资源不存在",
        )
            .into_response(),
        None => render(assets::index(), "no-cache", accept_zstd),
    }
}

fn cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// 压缩响应直接引用构建期的独立帧；普通响应只解压当前文件。
/// `Vary` 让中间缓存按编码分开存。
fn render(asset: &'static assets::Asset, cache: &'static str, accept_zstd: bool) -> Response {
    let encoded = if accept_zstd { asset.encoded() } else { None };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime()));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    if encoded.is_some() {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    }
    let body = match encoded {
        Some(bytes) => axum::body::Body::from(bytes),
        None => match asset.body() {
            std::borrow::Cow::Borrowed(bytes) => axum::body::Body::from(bytes),
            std::borrow::Cow::Owned(bytes) => axum::body::Body::from(bytes),
        },
    };
    (headers, body).into_response()
}

/// `Accept-Encoding` 是可重复头，`zstd;q=0` 表示明确拒绝。
fn accepts_zstd(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| {
            let mut parts = token.split(';');
            let name = parts.next().unwrap_or_default().trim();
            let weight = parts.find_map(|part| {
                part.trim()
                    .strip_prefix("q=")
                    .and_then(|weight| weight.parse::<f32>().ok())
            });
            name.eq_ignore_ascii_case("zstd") && weight != Some(0.0)
        })
}

#[derive(Debug, Deserialize)]
struct AuthQuery {
    token: Option<String>,
}

/// 令牌校验放在 `WebSocketUpgrade` 提取之前：否则缺 Upgrade 头的未授权请求
/// 会先被判成 400，掩盖真正的 401。
async fn require_token(
    State(state): State<AppState>,
    Query(query): Query<AuthQuery>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.authorized(query.token.as_deref()) {
        return (StatusCode::UNAUTHORIZED, "缺少或错误的访问令牌").into_response();
    }
    next.run(request).await
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "跨站 WebSocket 被拒绝").into_response();
    }
    let server = Arc::clone(&state.server);
    ws.on_upgrade(move |socket| bridge::serve(socket, server))
}

/// 浏览器会带 `Origin`；只接受本机来源，防 DNS rebinding 与跨站页面连入。
///
/// 非浏览器客户端通常不带这个头，放行交给 token 校验。
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next());
    let Some(authority) = authority else {
        return false;
    };
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => host,
        _ => authority,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// 定长比较，避免按字节提前返回泄露令牌前缀。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_encoding(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, value.parse().unwrap());
        headers
    }

    #[test]
    fn zstd_is_sent_only_when_the_client_accepts_it() {
        assert!(!accepts_zstd(&HeaderMap::new()));
        assert!(!accepts_zstd(&accept_encoding("gzip, br")));
        assert!(accepts_zstd(&accept_encoding("gzip, zstd")));
        assert!(accepts_zstd(&accept_encoding("ZSTD;q=1")));
        assert!(!accepts_zstd(&accept_encoding("zstd;q=0")));
    }

    #[test]
    fn security_headers_close_framing_and_sniffing() {
        let mut headers = HeaderMap::new();
        apply_security_headers(&mut headers);
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        for directive in [
            "default-src 'self'",
            "script-src 'self'",
            "frame-ancestors 'none'",
            "frame-src 'none'",
            "object-src 'none'",
        ] {
            assert!(csp.contains(directive), "CSP 缺少 {directive}");
        }
    }

    #[tokio::test]
    async fn occupied_port_falls_back_to_an_ephemeral_one_only_when_allowed() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = occupied.local_addr().unwrap().port();

        let (listener, fell_back) = listen(port, true).await.unwrap();
        assert!(fell_back);
        assert_ne!(listener.local_addr().unwrap().port(), port);

        let error = listen(port, false).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }
}
