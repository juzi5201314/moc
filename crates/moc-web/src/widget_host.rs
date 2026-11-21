//! WebUI 嵌入式交互组件的专用宿主页（`GET /widget-host`）。
//!
//! 主页面严格限制 `script-src 'self'`，若直接用 `<iframe srcdoc>` 会继承主页面 CSP
//! 导致组件脚本被拦截。本路由提供极简引导页：
//! - 响应头只保留 `frame-ancestors 'self'`（以及 `SAMEORIGIN` / `nosniff` / `no-referrer`），
//!   不限制外部 CDN、样式、图片或网络请求；
//! - 前端在 `<iframe sandbox="...">` 中不授予 `allow-same-origin`，将组件隔离在 opaque
//!   (`null`) origin 中，使其无法读取主页面的 `moc-token`、DOM 或调用 `/fs/raw` / `/pty`。

use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};

const BOOTSTRAP_HTML: &str = r#"<!doctype html>
<html>
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"></head>
<body>
<script>
(function () {
  var id = location.hash.slice(1);
  var rendered = false;
  window.addEventListener("message", function (event) {
    if (rendered || event.source !== window.parent) return;
    var data = event.data;
    if (!data || data.channel !== "moc-widget" || data.id !== id || data.type !== "render" || typeof data.html !== "string") return;
    rendered = true;
    document.open();
    document.write(data.html);
    document.close();
  });
  if (window.parent && window.parent !== window) {
    window.parent.postMessage({ channel: "moc-widget", id: id, type: "ready" }, "*");
  }
})();
</script>
</body>
</html>"#;

/// 返回组件引导宿主页及其专属响应头。
pub(crate) async fn handler() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'self'"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    (headers, BOOTSTRAP_HTML).into_response()
}
