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

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use moc_core::Core;
use moc_protocol::Server;
use serde::Deserialize;

/// `moc web` 的启动参数。
#[derive(Debug, Clone)]
pub struct WebOptions {
    /// 监听地址；只应绑定回环地址。
    pub addr: SocketAddr,
    /// 访问令牌：浏览器必须用 `?token=` 带上它才能建立 WebSocket。
    pub token: String,
    /// 启动后用系统默认浏览器打开 WebUI。
    pub open_browser: bool,
}

/// 启动 Web 前端，直到进程退出。
pub async fn run(core: Arc<Core>, options: WebOptions) -> anyhow::Result<()> {
    if !options.addr.ip().is_loopback() {
        anyhow::bail!("moc web 只允许绑定回环地址，收到 {}", options.addr);
    }
    let listener = tokio::net::TcpListener::bind(options.addr).await?;
    let addr = listener.local_addr()?;
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
        .fallback(get(static_handler))
        .with_state(state)
}

/// 静态资源：精确命中嵌入表；`/assets/` 下的文件名带内容哈希，可永久缓存。
/// `/assets/` 下未命中回 404（旧版本的过期 chunk 不能拿到 HTML）；
/// 其余未命中回 `index.html`（SPA 回退），且不缓存，保证升级后拿到新入口。
async fn static_handler(uri: Uri) -> Response {
    match assets::lookup(uri.path()) {
        Some(asset) => {
            let cache = if asset.path.starts_with("/assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, asset.mime),
                    (header::CACHE_CONTROL, cache),
                ],
                asset.body,
            )
                .into_response()
        }
        None if uri.path().starts_with("/assets/") => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "资源不存在",
        )
            .into_response(),
        None => {
            let index = assets::index();
            (
                [
                    (header::CONTENT_TYPE, index.mime),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                index.body,
            )
                .into_response()
        }
    }
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
