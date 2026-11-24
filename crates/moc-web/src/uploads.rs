//! `/uploads`：输入框「上传文件」的落点——写进会话 sidecar 的 `uploads/<sha256>/<文件名>`。
//!
//! 按内容寻址去重：前端先算 SHA-256 用 `GET` 探测，已有同内容文件时直接复用、不再传输；
//! `PUT` 边收边算摘要，与声明不符即拒绝，避免错误内容占住某个摘要槽位。

use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use moc_i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{AppState, origin_allowed};

/// 单个上传文件的体积上限，与 `/fs/raw` 一致。
pub const UPLOAD_MAX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    session_id: String,
    sha256: String,
    /// 仅 `PUT` 使用：落盘文件名（取最后一段并去掉控制字符）。
    name: Option<String>,
}

/// 上传结果：core 所在机器上的绝对路径与字节数。
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Stored {
    path: String,
    size: u64,
}

type Failure = (StatusCode, String);

/// `GET /uploads`：同内容文件已在本会话上传过时返回它，否则 404。
pub async fn lookup(State(state): State<AppState>, Query(query): Query<UploadQuery>) -> Response {
    let result = async {
        let slot = slot(&state, &query).await?;
        existing(&slot)
            .await
            .ok_or_else(|| (StatusCode::NOT_FOUND, t!("web.err.not_found").into_owned()))
    }
    .await;
    respond(result)
}

/// `PUT /uploads`：请求体流式写入并校验摘要；同内容已存在时丢弃本次数据、复用已有文件。
pub async fn store(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if !origin_allowed(&headers) {
        return (
            StatusCode::FORBIDDEN,
            t!("web.err.write_denied").into_owned(),
        )
            .into_response();
    }
    let result = async {
        let slot = slot(&state, &query).await?;
        let name = file_name(query.name.as_deref().unwrap_or_default());
        save(
            &slot,
            &query.sha256,
            &name,
            request.into_body(),
            UPLOAD_MAX_BYTES,
        )
        .await
    }
    .await;
    respond(result)
}

fn respond(result: Result<Stored, Failure>) -> Response {
    match result {
        Ok(stored) => Json(stored).into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// 摘要对应的槽位目录 `<uploads>/<sha256>`；摘要必须是 64 位小写十六进制。
async fn slot(state: &AppState, query: &UploadQuery) -> Result<PathBuf, Failure> {
    let digest = &query.sha256;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            t!("web.err.upload_digest_invalid").into_owned(),
        ));
    }
    let uploads = state
        .core
        .session_uploads_dir(&query.session_id)
        .await
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                t!("web.err.session_required").into_owned(),
            )
        })?;
    Ok(uploads.join(digest))
}

/// 槽位里已发布的文件；目录里只会有一个普通文件。
async fn existing(slot: &Path) -> Option<Stored> {
    let mut entries = tokio::fs::read_dir(slot).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let metadata = entry.metadata().await.ok()?;
        if metadata.is_file() {
            return Some(Stored {
                path: entry.path().to_string_lossy().into_owned(),
                size: metadata.len(),
            });
        }
    }
    None
}

/// 落盘文件名：只取最后一段，去掉控制字符；取不出可用名字时用 `file`。
fn file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().filter(|char| !char.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file".to_owned()
    } else {
        cleaned.to_owned()
    }
}

async fn save(
    slot: &Path,
    digest: &str,
    name: &str,
    body: Body,
    max_bytes: u64,
) -> Result<Stored, Failure> {
    let bad = |key: &str, error: String| -> Failure {
        (StatusCode::BAD_REQUEST, t!(key, error = error).into_owned())
    };
    if let Some(stored) = existing(slot).await {
        return Ok(stored);
    }
    let uploads = slot.parent().expect("槽位在 uploads 目录下").to_owned();
    tokio::fs::create_dir_all(&uploads)
        .await
        .map_err(|error| bad("web.err.staging_create", error.to_string()))?;
    let staged = tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(uploads))
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| bad("web.err.staging_create", error.to_string()))?;
    // TempPath 保持到发布完成；请求失败或取消时自动删除半成品。
    let (handle, temporary) = staged.into_parts();
    let mut file = tokio::fs::File::from_std(handle);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|error| bad("web.err.body_read", error.to_string()))?;
        size += bytes.len() as u64;
        if size > max_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                t!("web.err.upload_too_large").into_owned(),
            ));
        }
        hasher.update(&bytes);
        file.write_all(&bytes)
            .await
            .map_err(|error| bad("web.err.write_failed", error.to_string()))?;
    }
    file.sync_all()
        .await
        .map_err(|error| bad("web.err.write_failed", error.to_string()))?;
    drop(file);
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != digest {
        return Err((
            StatusCode::BAD_REQUEST,
            t!("web.err.upload_digest_mismatch").into_owned(),
        ));
    }
    tokio::fs::create_dir_all(slot)
        .await
        .map_err(|error| bad("web.err.publish_failed", error.to_string()))?;
    let target = slot.join(name);
    // 并发上传同一内容时后发布者覆盖同名文件，内容相同，无需互斥。
    tokio::task::spawn_blocking(move || temporary.persist(&target).map(|()| target))
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map_err(|error| bad("web.err.publish_failed", error.to_string()))?;
    existing(slot).await.ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            t!("web.err.not_found").into_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 同内容第二次上传复用首个文件（不论文件名）；摘要不符时拒绝且不留下文件。
    #[tokio::test]
    async fn uploads_dedupe_by_content_and_reject_digest_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let digest = sha(b"hello");
        let slot = dir.path().join("uploads").join(&digest);

        let first = save(&slot, &digest, "a.txt", Body::from("hello"), 1024)
            .await
            .unwrap();
        assert!(first.path.ends_with("a.txt"), "{}", first.path);
        assert_eq!(first.size, 5);
        let again = save(&slot, &digest, "b.txt", Body::from("hello"), 1024)
            .await
            .unwrap();
        assert_eq!(again, first);

        let wrong = sha(b"other");
        let wrong_slot = dir.path().join("uploads").join(&wrong);
        let error = save(&wrong_slot, &wrong, "c.txt", Body::from("hello"), 1024)
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(!wrong_slot.exists());
        let leftovers = std::fs::read_dir(dir.path().join("uploads"))
            .unwrap()
            .count();
        assert_eq!(leftovers, 1, "暂存文件必须被清理");
    }

    #[test]
    fn file_names_keep_only_the_last_segment() {
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name(r"C:\Users\a\report.pdf"), "report.pdf");
        assert_eq!(file_name(".."), "file");
        assert_eq!(file_name(""), "file");
    }
}
