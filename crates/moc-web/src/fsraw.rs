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
        let disposition = match HeaderValue::from_str(&content_disposition(&path)) {
            Ok(value) => value,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("文件名无法用于下载：{error}"),
                )
                    .into_response();
            }
        };
        headers.insert(header::CONTENT_DISPOSITION, disposition);
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
    upload_to(
        &path,
        query.overwrite == Some(1),
        request.into_body(),
        UPLOAD_MAX_BYTES,
    )
    .await
}

async fn upload_to(
    path: &std::path::Path,
    overwrite: bool,
    body: Body,
    max_bytes: u64,
) -> Response {
    let exists = path.symlink_metadata().is_ok();
    if exists && !overwrite {
        return (StatusCode::CONFLICT, "目标已存在").into_response();
    }
    // 覆盖已有链接时沿用原接口语义，替换链接指向的普通文件。
    let target = if exists {
        match tokio::fs::canonicalize(path).await {
            Ok(target) => target,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        }
    } else {
        path.to_owned()
    };
    let Some(parent) = target.parent().map(std::path::Path::to_owned) else {
        return (StatusCode::BAD_REQUEST, "不是普通文件路径").into_response();
    };
    let staged = tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(parent)).await;
    let temporary = match staged {
        Ok(Ok(file)) => file,
        Ok(Err(error)) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("无法创建暂存文件：{error}"),
            )
                .into_response();
        }
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    };
    let (handle, temporary) = temporary.into_parts();
    // TempPath 保持到发布完成；请求失败或取消时自动删除半成品。
    let mut file = tokio::fs::File::from_std(handle);
    match tokio::fs::metadata(&target).await {
        Ok(metadata) => {
            if !metadata.is_file() {
                return (StatusCode::BAD_REQUEST, "不是普通文件").into_response();
            }
            if let Err(error) = file.set_permissions(metadata.permissions()).await {
                return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
    let mut received = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                return (StatusCode::BAD_REQUEST, format!("读取请求体失败：{error}"))
                    .into_response();
            }
        };
        received += bytes.len() as u64;
        if received > max_bytes {
            return (StatusCode::PAYLOAD_TOO_LARGE, "文件超过上传上限").into_response();
        }
        if let Err(error) = file.write_all(&bytes).await {
            return (StatusCode::BAD_REQUEST, format!("写入失败：{error}")).into_response();
        }
    }
    if let Err(error) = file.sync_all().await {
        return (StatusCode::BAD_REQUEST, format!("写入失败：{error}")).into_response();
    }
    drop(file);
    // 不覆盖发布不能使用 exists + rename；persist_noclobber 在发布时原子拒绝竞争者。
    let published = tokio::task::spawn_blocking(move || {
        if overwrite {
            temporary.persist(&target)
        } else {
            temporary.persist_noclobber(&target)
        }
    })
    .await;
    match published {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return (StatusCode::CONFLICT, "目标已存在").into_response();
        }
        Ok(Err(error)) => {
            return (StatusCode::BAD_REQUEST, format!("发布文件失败：{error}")).into_response();
        }
        Err(error) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
        }
    }
    axum::Json(serde_json::json!({"path": path.to_string_lossy()})).into_response()
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
    if name
        .bytes()
        .all(|byte| (0x20..=0x7e).contains(&byte) && !b"\"\\".contains(&byte))
    {
        return format!("attachment; filename=\"{name}\"");
    }
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(byte) {
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

    async fn upload(path: &std::path::Path, overwrite: bool, body: Body) -> Response {
        write(
            Query(WriteQuery {
                path: path.to_string_lossy().into_owned(),
                overwrite: overwrite.then_some(1),
            }),
            HeaderMap::new(),
            Request::new(body),
        )
        .await
    }

    #[tokio::test]
    async fn interrupted_upload_preserves_original_and_cleans_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"original").unwrap();
        let body = Body::from_stream(futures_util::stream::iter([
            Ok(bytes::Bytes::from_static(b"partial")),
            Err(std::io::Error::other("连接中断")),
        ]));
        assert_eq!(
            upload(&path, true, body).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn oversized_and_cancelled_uploads_preserve_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"original").unwrap();
        let response = upload_to(&path, true, Body::from("oversized"), 4).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let (started, waiting) = tokio::sync::oneshot::channel();
        let body = Body::from_stream(futures_util::stream::once(async move {
            started.send(()).unwrap();
            std::future::pending::<Result<bytes::Bytes, std::io::Error>>().await
        }));
        let mut request = Box::pin(upload(&path, true, body));
        tokio::select! {
            _ = &mut request => panic!("请求体尚未完成"),
            _ = waiting => {}
        }
        drop(request);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_overwrite_preserves_permissions_and_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&path, b"original").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&path, &link).unwrap();
        assert_eq!(
            upload(&link, true, Body::from("complete")).await.status(),
            StatusCode::OK
        );
        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn concurrent_uploads_do_not_overwrite_without_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        // 在消费请求体时同步，确保两个请求都已经通过存在性预检。
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let body = |text: &'static [u8]| {
            let barrier = barrier.clone();
            Body::from_stream(futures_util::stream::once(async move {
                barrier.wait().await;
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(text))
            }))
        };
        let (left, right) = tokio::join!(
            upload(&path, false, body(b"left")),
            upload(&path, false, body(b"right")),
        );
        let statuses = [left.status(), right.status()];
        assert!(statuses.contains(&StatusCode::OK));
        assert!(statuses.contains(&StatusCode::CONFLICT));
        let expected = if left.status() == StatusCode::OK {
            b"left".as_slice()
        } else {
            b"right".as_slice()
        };
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_encodes_control_characters_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("line\n\r\x7f'break.txt");
        std::fs::write(&path, b"data").unwrap();
        let response = read(Query(RawQuery {
            path: path.to_string_lossy().into_owned(),
            download: Some(1),
        }))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename*=UTF-8''line%0A%0D%7F%27break.txt"
        );
    }

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
