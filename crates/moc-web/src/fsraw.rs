//! `/fs/raw`：文件管理器的原始字节通道——预览下载与流式上传。

use axum::body::Body;
use axum::extract::{Query, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::origin_allowed;

/// 上传请求的 body 上限；超出即拒绝并清掉半成品文件。
const UPLOAD_MAX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct RawQuery {
    path: String,
    download: Option<u8>,
}

/// `GET /fs/raw`：流式返回文件字节；`download=1` 时按附件下发。
pub async fn read(Query(query): Query<RawQuery>) -> Response {
    let path = match expand(&query.path) {
        Ok(path) => path,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            return (
                StatusCode::NOT_FOUND,
                format!("无法打开文件：{}：{error}", path.display()),
            )
                .into_response();
        }
    };
    let Ok(metadata) = file.metadata().await else {
        return (StatusCode::NOT_FOUND, "无法读取文件信息").into_response();
    };
    if !metadata.is_file() {
        return (StatusCode::BAD_REQUEST, "不是普通文件").into_response();
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&path)),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string()).expect("长度是数字"),
    );
    if query.download == Some(1) {
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&content_disposition(&path)).expect("附件头不含非法字节"),
        );
    }
    let stream = tokio_util::io::ReaderStream::new(file);
    (headers, Body::from_stream(stream)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct WriteQuery {
    path: String,
    overwrite: Option<u8>,
}

/// `PUT /fs/raw`：请求体流式写入文件；已存在且未带 `overwrite=1` 报 409。
///
/// 跨站写必须带本机 Origin（token 在 query 里，浏览器可跨站发带 query 的 PUT）。
pub async fn write(
    Query(query): Query<WriteQuery>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "跨站写入被拒绝").into_response();
    }
    let path = match expand(&query.path) {
        Ok(path) => path,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let exists =
        tokio::fs::try_exists(&path).await.unwrap_or(false) || path.symlink_metadata().is_ok();
    if exists && query.overwrite != Some(1) {
        return (StatusCode::CONFLICT, "目标已存在").into_response();
    }
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(file) => file,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("无法创建文件：{}：{error}", path.display()),
            )
                .into_response();
        }
    };
    let mut received = 0u64;
    let mut stream = request.into_body().into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = tokio::fs::remove_file(&path).await;
                return (StatusCode::BAD_REQUEST, format!("读取请求体失败：{error}"))
                    .into_response();
            }
        };
        received += bytes.len() as u64;
        if received > UPLOAD_MAX_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&path).await;
            return (StatusCode::PAYLOAD_TOO_LARGE, "文件超过 512 MiB 上限").into_response();
        }
        if let Err(error) = file.write_all(&bytes).await {
            let _ = tokio::fs::remove_file(&path).await;
            return (
                StatusCode::BAD_REQUEST,
                format!("写入失败：{}：{error}", path.display()),
            )
                .into_response();
        }
    }
    if let Err(error) = file.flush().await {
        return (StatusCode::BAD_REQUEST, format!("写入失败：{error}")).into_response();
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            "{{\"path\":{}}}",
            serde_json::to_string(&path.to_string_lossy()).unwrap()
        ),
    )
        .into_response()
}

/// 路径校验：复用 core 的 `~` 展开与绝对路径要求。
fn expand(raw: &str) -> Result<std::path::PathBuf, moc_core::fs::PathError> {
    moc_core::fs::expand(raw)
}

/// 按扩展名的 Content-Type 小表；其余一律 octet-stream。
fn content_type(path: &std::path::Path) -> &'static str {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `attachment` 下载名：ASCII 直接给 `filename=`，非 ASCII 走 `filename*=`。
fn content_disposition(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_owned());
    if name.is_ascii() && !name.contains(['"', '\\']) {
        return format!("attachment; filename=\"{name}\"");
    }
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~'".contains(byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename*=UTF-8''{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_covers_the_small_table() {
        for (name, expected) in [
            ("a.png", "image/png"),
            ("a.JPG", "image/jpeg"),
            ("a.svg", "image/svg+xml"),
            ("a.pdf", "application/pdf"),
            ("a.mp4", "video/mp4"),
            ("a.txt", "text/plain; charset=utf-8"),
            ("a.bin", "application/octet-stream"),
            ("noext", "application/octet-stream"),
        ] {
            assert_eq!(content_type(std::path::Path::new(name)), expected, "{name}");
        }
    }

    #[test]
    fn disposition_keeps_ascii_and_encodes_utf8_names() {
        assert_eq!(
            content_disposition(std::path::Path::new("/a/report.pdf")),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            content_disposition(std::path::Path::new("/a/报告.pdf")),
            "attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf"
        );
    }
}
