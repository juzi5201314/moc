//! Image Playground HTTP API 路由处理器。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::Json;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use moc_i18n::t;
use moc_llm::{ImageGenParams, ImageInputFile};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{AppState, origin_allowed};

use super::codec;
use super::config::{
    self, AnimatedOutputFormat, OutputCompressionConfig, OutputCompressionFormat, PlaygroundConfig,
};
use super::reveal;
use super::store::{
    self, FavoriteFolder, FavoritePreset, ImageOrigin, PlaygroundSession, PlaygroundSessionSummary,
    PlaygroundTurn, StoredImage, TurnError, TurnInputRef,
};

// 同时最多读取两个上传请求，结合单请求体积限制约束进程内上传缓冲区。
const MAX_CONCURRENT_UPLOAD_READS: usize = 2;
static UPLOAD_READ_SEMAPHORE: Semaphore = Semaphore::const_new(MAX_CONCURRENT_UPLOAD_READS);

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_body: Option<String>,
}

fn err_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
            status: None,
            raw_body: None,
        }),
    )
        .into_response()
}

fn check_origin(headers: &HeaderMap) -> Result<(), Response> {
    if !origin_allowed(headers) {
        return Err(err_response(StatusCode::FORBIDDEN, t!("web.err.write_denied")));
    }
    Ok(())
}

// ---------- 模型列表 ----------

#[derive(Serialize)]
pub struct ImageModelRow {
    pub id: String,
    pub provider: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ImageModelRow>,
}

pub async fn list_models(State(state): State<AppState>) -> Json<ModelsResponse> {
    let cfg = state.core.config_snapshot();
    let models = cfg
        .available_image_models()
        .into_iter()
        .map(|m| ImageModelRow {
            id: m.id,
            provider: m.provider_id,
            name: m.model_id,
        })
        .collect();
    Json(ModelsResponse { models })
}

// ---------- 配置 ----------

#[derive(Serialize)]
pub struct ConfigResponse {
    pub config: PlaygroundConfig,
    pub resolved_storage_dir: String,
}

pub async fn get_config(State(state): State<AppState>) -> Response {
    let home = state.core.config_home();
    match tokio::task::spawn_blocking(move || {
        let cfg = config::load(&home)?;
        let resolved = cfg.resolved_storage_dir(&home);
        Ok::<_, anyhow::Error>((cfg, resolved))
    })
    .await
    {
        Ok(Ok((cfg, resolved))) => Json(ConfigResponse {
            config: cfg,
            resolved_storage_dir: resolved.to_string_lossy().into_owned(),
        })
        .into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn put_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut next): Json<PlaygroundConfig>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let home = state.core.config_home();
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        next.validate()?;
        let resolved = next.resolved_storage_dir(&home);
        std::fs::create_dir_all(&resolved)?;
        let saved = config::save(&home, next)?;
        Ok::<_, anyhow::Error>((saved, resolved))
    })
    .await
    {
        Ok(Ok((cfg, resolved))) => Json(ConfigResponse {
            config: cfg,
            resolved_storage_dir: resolved.to_string_lossy().into_owned(),
        })
        .into_response(),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn open_storage_dir(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _ = tokio::fs::create_dir_all(&dir).await;
    match reveal::reveal_in_file_manager(&dir).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

// ---------- 会话 ----------

#[derive(Serialize)]
pub struct SessionsResponse {
    pub sessions: Vec<PlaygroundSessionSummary>,
}

pub async fn list_sessions(State(state): State<AppState>) -> Response {
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    match tokio::task::spawn_blocking(move || store::load_index(&dir).map(|i| i.summaries())).await
    {
        Ok(Ok(sessions)) => Json(SessionsResponse { sessions }).into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize, Default)]
pub struct CreateSessionBody {
    pub title: Option<String>,
    pub model: Option<String>,
    pub params: Option<ImageGenParams>,
}

#[derive(Serialize)]
pub struct SessionDetailResponse {
    pub session: PlaygroundSession,
    pub images: HashMap<String, ImageDto>,
}

pub async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<CreateSessionBody>>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let body = body.map(|b| b.0).unwrap_or_default();
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let now = store::now_ms();
        let session = PlaygroundSession {
            id: store::new_id(),
            title: body
                .title
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| t!("web.playground.default_session_title").into_owned()),
            created_at_ms: now,
            updated_at_ms: now,
            model: body.model,
            params: body.params.unwrap_or_default(),
            turns: Vec::new(),
        };
        index.sessions.push(session.clone());
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(session)
    })
    .await
    {
        Ok(Ok(session)) => Json(SessionDetailResponse {
            session,
            images: HashMap::new(),
        })
        .into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    match tokio::task::spawn_blocking(move || {
        let index = store::load_index(&dir)?;
        let Some(session) = index.sessions.iter().find(|s| s.id == id).cloned() else {
            return Ok(None);
        };
        let mut images = HashMap::new();
        for turn in &session.turns {
            for out_id in &turn.outputs {
                if let Some(img) = index.images.get(out_id) {
                    images.insert(
                        out_id.clone(),
                        ImageDto::from_stored(
                            img,
                            index.is_favorite(out_id),
                            index.favorite_folder_of(out_id),
                        ),
                    );
                }
            }
            for input in &turn.inputs {
                if let Some(img_id) = &input.image_id
                    && let Some(img) = index.images.get(img_id)
                {
                    images.insert(
                        img_id.clone(),
                        ImageDto::from_stored(
                            img,
                            index.is_favorite(img_id),
                            index.favorite_folder_of(img_id),
                        ),
                    );
                }
            }
        }
        Ok::<_, anyhow::Error>(Some(SessionDetailResponse { session, images }))
    })
    .await
    {
        Ok(Ok(Some(detail))) => Json(detail).into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.session_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize)]
pub struct PatchSessionBody {
    pub title: Option<String>,
    pub model: Option<String>,
    pub params: Option<ImageGenParams>,
}

pub async fn patch_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PatchSessionBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(session) = index.sessions.iter_mut().find(|s| s.id == id) else {
            return Ok(None);
        };
        if let Some(title) = body.title.map(|t| t.trim().to_owned())
            && !title.is_empty()
        {
            session.title = title;
        }
        if let Some(model) = body.model {
            session.model = if model.trim().is_empty() {
                None
            } else {
                Some(model)
            };
        }
        if let Some(params) = body.params {
            session.params = params;
        }
        session.updated_at_ms = store::now_ms();
        let updated = session.clone();
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(Some(updated))
    })
    .await
    {
        Ok(Ok(Some(session))) => Json(serde_json::json!({ "session": session })).into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.session_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize, Default)]
pub struct DeleteSessionQuery {
    pub delete_unfavorited: Option<bool>,
}

pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<DeleteSessionQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    state.image_playground.cancel_session(&id);
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let delete_unfavorited = query.delete_unfavorited.unwrap_or(true);
    let _guard = state.image_playground.write_lock.lock().await;
    // 持锁期间再次取消，确保命中在拿锁前瞬间完成校验并注册的生成请求。
    state.image_playground.cancel_session(&id);
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let removed = index.remove_session(&id, delete_unfavorited);
        store::save_index(&dir, &index)?;
        let mut removed_ids = Vec::with_capacity(removed.len());
        for (img_id, path) in removed {
            removed_ids.push(img_id);
            store::trash_or_remove(&path);
        }
        Ok::<_, anyhow::Error>(removed_ids)
    })
    .await
    {
        Ok(Ok(removed_image_ids)) => Json(serde_json::json!({
            "ok": true,
            "removed_image_ids": removed_image_ids,
        }))
        .into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

// ---------- 生成 / 编辑 ----------

#[derive(Debug, Clone, Deserialize)]
pub struct UploadRefInput {
    pub upload_id: String,
    #[serde(default)]
    pub filename: Option<String>,
}

#[derive(Serialize)]
pub struct GenerateResponse {
    pub turn: PlaygroundTurn,
    pub images: Vec<ImageDto>,
    pub session_title: String,
}

enum PrepOutcome {
    Ready(Vec<ImageInputFile>, Vec<TurnInputRef>),
    Cancelled,
}

enum TranscodeOutcome {
    Ready(Vec<StoredImage>),
    Cancelled,
}

pub async fn generate(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let upload_permit = match UPLOAD_READ_SEMAPHORE.acquire().await {
        Ok(permit) => permit,
        Err(_) => {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, t!("web.playground.upload_permit_unavailable"));
        }
    };

    let mut prompt = String::new();
    let mut model_selector = String::new();
    let mut params = ImageGenParams::default();
    let mut source_image_ids: Vec<String> = Vec::new();
    let mut source_uploads: Vec<UploadRefInput> = Vec::new();
    let mut uploaded_files: Vec<(String, String, Vec<u8>)> = Vec::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return err_response(error.status(), t!("web.playground.multipart_read_failed", error = error));
            }
        };
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "prompt" => {
                prompt = match field.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
            }
            "model" => {
                model_selector = match field.text().await {
                    Ok(text) => text.trim().to_owned(),
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
            }
            "params" => {
                let text = match field.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
                if !text.trim().is_empty() {
                    match serde_json::from_str::<ImageGenParams>(&text) {
                        Ok(parsed) => params = parsed,
                        Err(error) => {
                            return err_response(
                                StatusCode::BAD_REQUEST,
                                t!("web.playground.invalid_params_json", error = error),
                            );
                        }
                    }
                }
            }
            "source_image_ids" => {
                let text = match field.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
                if !text.trim().is_empty() {
                    match serde_json::from_str::<Vec<String>>(&text) {
                        Ok(ids) => source_image_ids = ids,
                        Err(error) => {
                            return err_response(
                                StatusCode::BAD_REQUEST,
                                t!("web.playground.invalid_ref_ids", error = error),
                            );
                        }
                    }
                }
            }
            "source_uploads" => {
                let text = match field.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
                if !text.trim().is_empty() {
                    match serde_json::from_str::<Vec<UploadRefInput>>(&text) {
                        Ok(refs) => source_uploads = refs,
                        Err(error) => {
                            return err_response(
                                StatusCode::BAD_REQUEST,
                                t!("web.playground.invalid_upload_refs", error = error),
                            );
                        }
                    }
                }
            }
            "images" | "image" => {
                let filename = field
                    .file_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "image.png".into());
                let mime = field
                    .content_type()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "application/octet-stream".into());
                match field.bytes().await {
                    Ok(bytes) if !bytes.is_empty() => {
                        uploaded_files.push((filename, mime, bytes.to_vec()));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.upload_read_failed", error = error));
                    }
                }
            }
            _ => {}
        }
    }

    let prompt_trimmed = prompt.trim().to_owned();
    if prompt_trimmed.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.prompt_required"));
    }
    if model_selector.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.model_required"));
    }

    let core_cfg = state.core.config_snapshot();
    let resolved_model = match core_cfg.resolve_image_model(&model_selector) {
        Ok(model) => model,
        Err(error) => return err_response(StatusCode::BAD_REQUEST, error.to_string()),
    };

    let (pg_cfg, storage_dir) = match load_cfg_and_dir(&state).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };

    // 持写锁原子校验会话存在、读取参考图记录并注册取消令牌，消除与删除会话间的竞态窗口。
    let (ref_records, flight) = {
        let _guard = state.image_playground.write_lock.lock().await;
        let dir = storage_dir.clone();
        let sid = session_id.clone();
        let ids = source_image_ids.clone();
        let refs = match tokio::task::spawn_blocking(move || {
            let index = store::load_index(&dir)?;
            if !index.sessions.iter().any(|s| s.id == sid) {
                anyhow::bail!("NOT_FOUND");
            }
            let mut refs = Vec::new();
            for id in ids {
                if let Some(img) = index.images.get(&id) {
                    if img.format.eq_ignore_ascii_case("avif") {
                        anyhow::bail!("AVIF_UNSUPPORTED:{}", img.filename);
                    }
                    refs.push((
                        id,
                        img.filename.clone(),
                        img.mime_type.clone(),
                        PathBuf::from(&img.file_path),
                    ));
                }
            }
            Ok::<_, anyhow::Error>(refs)
        })
        .await
        {
            Ok(Ok(refs)) => refs,
            Ok(Err(error)) if error.to_string() == "NOT_FOUND" => {
                return err_response(StatusCode::NOT_FOUND, t!("web.playground.session_not_found"));
            }
            Ok(Err(error)) if error.to_string().starts_with("AVIF_UNSUPPORTED:") => {
                let filename = error
                    .to_string()
                    .trim_start_matches("AVIF_UNSUPPORTED:")
                    .to_owned();
                return err_response(
                    StatusCode::BAD_REQUEST,
                    t!("web.playground.avif_ref_unsupported", filename = filename),
                );
            }
            Ok(Err(error)) => {
                return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
            Err(error) => {
                return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
            }
        };
        let flight = state.image_playground.register_flight(&session_id);
        (refs, flight)
    };

    let has_requested_inputs =
        !ref_records.is_empty() || !source_uploads.is_empty() || !uploaded_files.is_empty();

    // 准备 Edit 输入图（含参考图保护压缩与本地去重归档），每步均检查取消状态。
    let input_compress = pg_cfg.input_compress.clone();
    let dir_for_inputs = storage_dir.clone();
    let cancel_for_prep = flight.cancel.clone();
    let prep_outcome = match tokio::task::spawn_blocking(move || {
        let _upload_permit = upload_permit;
        let mut api_inputs = Vec::new();
        let mut turn_inputs = Vec::new();

        for (img_id, filename, mime, path) in ref_records {
            if cancel_for_prep.is_cancelled() {
                return Ok(PrepOutcome::Cancelled);
            }
            let raw = std::fs::read(&path)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        t!("web.playground.ref_read_failed", filename = filename, error = e)
                    )
                })?;
            let prepared = codec::prepare_edit_input(&raw, Some(&mime), &input_compress)?;
            api_inputs.push(ImageInputFile {
                filename: replace_ext(&filename, &prepared.extension),
                mime_type: prepared.mime_type.clone(),
                bytes: prepared.bytes,
            });
            turn_inputs.push(TurnInputRef {
                image_id: Some(img_id),
                upload_id: None,
                filename,
                mime_type: mime,
            });
        }

        for up_ref in source_uploads {
            if cancel_for_prep.is_cancelled() {
                return Ok(PrepOutcome::Cancelled);
            }
            if !is_safe_upload_id(&up_ref.upload_id) {
                anyhow::bail!("{}", t!("web.playground.invalid_upload_id"));
            }
            let path = dir_for_inputs.join("uploads").join(&up_ref.upload_id);
            let raw = std::fs::read(&path)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        t!(
                            "web.playground.temp_ref_read_failed",
                            id = up_ref.upload_id,
                            error = e
                        )
                    )
                })?;
            let meta = codec::inspect_image(&raw, None)?;
            let prepared = codec::prepare_edit_input(&raw, Some(&meta.mime_type), &input_compress)?;
            let filename = up_ref
                .filename
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&up_ref.upload_id)
                .to_owned();
            api_inputs.push(ImageInputFile {
                filename: replace_ext(&filename, &prepared.extension),
                mime_type: prepared.mime_type.clone(),
                bytes: prepared.bytes,
            });
            turn_inputs.push(TurnInputRef {
                image_id: None,
                upload_id: Some(up_ref.upload_id),
                filename,
                mime_type: meta.mime_type,
            });
        }

        for (filename, mime, raw) in uploaded_files {
            if cancel_for_prep.is_cancelled() {
                return Ok(PrepOutcome::Cancelled);
            }
            let meta = codec::inspect_image(&raw, Some(&mime))?;
            let (upload_id, _) = store::write_edit_upload(&dir_for_inputs, &meta.extension, &raw)?;
            let prepared = codec::prepare_edit_input(&raw, Some(&mime), &input_compress)?;
            api_inputs.push(ImageInputFile {
                filename: replace_ext(&filename, &prepared.extension),
                mime_type: prepared.mime_type.clone(),
                bytes: prepared.bytes,
            });
            turn_inputs.push(TurnInputRef {
                image_id: None,
                upload_id: Some(upload_id),
                filename,
                mime_type: meta.mime_type,
            });
        }

        Ok::<_, anyhow::Error>(PrepOutcome::Ready(api_inputs, turn_inputs))
    })
    .await
    {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => return err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };

    let started = Instant::now();
    let (call_result, turn_inputs, mode) = match prep_outcome {
        PrepOutcome::Cancelled => (
            Err(moc_llm::ImageGenError::Transport(t!("web.playground.generation_cancelled").into_owned())),
            Vec::new(),
            if has_requested_inputs {
                "edit"
            } else {
                "generate"
            }
            .to_owned(),
        ),
        PrepOutcome::Ready(api_inputs, turn_inputs) => {
            let mut effective_params = params.clone();
            if !api_inputs.is_empty() && effective_params.edit_encoding.is_none() {
                effective_params.edit_encoding = Some(pg_cfg.default_edit_encoding);
            }
            let mode = if api_inputs.is_empty() {
                "generate".to_owned()
            } else {
                "edit".to_owned()
            };
            let llm_client = state.core.llm_client();
            let llm_cfg = resolved_model.llm.clone();
            let res = if flight.cancel.is_cancelled() {
                Err(moc_llm::ImageGenError::Transport(t!("web.playground.generation_cancelled").into_owned()))
            } else {
                tokio::select! {
                    biased;
                    _ = flight.cancel.cancelled() => Err(moc_llm::ImageGenError::Transport(t!("web.playground.generation_cancelled").into_owned())),
                    res = llm_client.generate_images(&llm_cfg, &prompt_trimmed, &api_inputs, &effective_params) => res,
                }
            };
            (res, turn_inputs, mode)
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let turn_id = store::new_id();
    let now = store::now_ms();

    let final_error = match call_result {
        Ok(resp) => {
            // 阻塞线程池执行本地转码并写盘，期间持续响应取消。
            let compression = pg_cfg.compression.clone();
            let dir_for_write = storage_dir.clone();
            let sid_clone = session_id.clone();
            let tid_clone = turn_id.clone();
            let model_clone = model_selector.clone();
            let prompt_clone = prompt_trimmed.clone();
            let cancel_for_transcode = flight.cancel.clone();

            let transcode_outcome = match tokio::task::spawn_blocking(move || {
                let mut stored: Vec<StoredImage> = Vec::with_capacity(resp.images.len());
                for item in resp.images {
                    if cancel_for_transcode.is_cancelled() {
                        for img in &stored {
                            let _ = std::fs::remove_file(&img.file_path);
                        }
                        return Ok(TranscodeOutcome::Cancelled);
                    }
                    let encoded = codec::transcode_output(
                        item.bytes,
                        item.mime_hint.as_deref(),
                        &compression,
                    )?;
                    let img_id = store::new_id();
                    let file_path =
                        store::write_generated_image(&dir_for_write, &img_id, &encoded)?;
                    let filename = format!("{img_id}.{}", encoded.extension);
                    stored.push(StoredImage {
                        id: img_id,
                        origin: ImageOrigin::Generated,
                        session_id: Some(sid_clone.clone()),
                        turn_id: Some(tid_clone.clone()),
                        created_at_ms: now,
                        model: Some(model_clone.clone()),
                        prompt: Some(prompt_clone.clone()),
                        revised_prompt: item.revised_prompt,
                        width: encoded.width,
                        height: encoded.height,
                        format: encoded.format,
                        extension: encoded.extension,
                        mime_type: encoded.mime_type,
                        is_animated: encoded.is_animated,
                        size_bytes: encoded.bytes.len() as u64,
                        original_size_bytes: encoded.original_size_bytes,
                        original_format: encoded.original_format,
                        file_path: file_path.to_string_lossy().into_owned(),
                        filename,
                    });
                }
                Ok::<_, anyhow::Error>(TranscodeOutcome::Ready(stored))
            })
            .await
            {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    return err_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        t!("web.playground.save_or_compress_failed", error = error),
                    );
                }
                Err(error) => {
                    return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
                }
            };

            match transcode_outcome {
                TranscodeOutcome::Cancelled => {
                    Some(moc_llm::ImageGenError::Transport(t!("web.playground.generation_cancelled").into_owned()))
                }
                TranscodeOutcome::Ready(encoded_items) if flight.cancel.is_cancelled() => {
                    for img in &encoded_items {
                        let _ = std::fs::remove_file(&img.file_path);
                    }
                    Some(moc_llm::ImageGenError::Transport(t!("web.playground.generation_cancelled").into_owned()))
                }
                TranscodeOutcome::Ready(encoded_items) => {
                    let revised_prompt = encoded_items
                        .iter()
                        .find_map(|img| img.revised_prompt.clone());
                    let output_ids: Vec<String> =
                        encoded_items.iter().map(|img| img.id.clone()).collect();
                    let turn = PlaygroundTurn {
                        id: turn_id,
                        created_at_ms: now,
                        model: model_selector.clone(),
                        mode,
                        prompt: prompt_trimmed.clone(),
                        params,
                        inputs: turn_inputs,
                        outputs: output_ids,
                        revised_prompt,
                        duration_ms,
                        error: None,
                    };

                    let _guard = state.image_playground.write_lock.lock().await;
                    let dir_for_save = storage_dir.clone();
                    let turn_for_save = turn.clone();
                    let sid_for_save = session_id.clone();
                    let dtos: Vec<ImageDto> = encoded_items
                        .iter()
                        .map(|img| ImageDto::from_stored(img, false, None))
                        .collect();

                    return match tokio::task::spawn_blocking(move || {
                        let mut index = store::load_index(&dir_for_save)?;
                        let Some(session) =
                            index.sessions.iter_mut().find(|s| s.id == sid_for_save)
                        else {
                            // 会话已被删除：清理刚写入磁盘的图片文件，不写入索引，避免留下孤立图片。
                            for img in &encoded_items {
                                let _ = std::fs::remove_file(&img.file_path);
                            }
                            return Ok::<_, anyhow::Error>(None);
                        };
                        if session.turns.is_empty() && matches!(session.title.as_str(), "New session" | "新生成会话") {
                            session.title = derive_title(&prompt_trimmed);
                        }
                        session.model = Some(model_selector);
                        session.updated_at_ms = now;
                        session.turns.push(turn_for_save);
                        let session_title = session.title.clone();
                        for img in encoded_items {
                            index.images.insert(img.id.clone(), img);
                        }
                        store::save_index(&dir_for_save, &index)?;
                        Ok::<_, anyhow::Error>(Some(session_title))
                    })
                    .await
                    {
                        Ok(Ok(Some(session_title))) => Json(GenerateResponse {
                            turn,
                            images: dtos,
                            session_title,
                        })
                        .into_response(),
                        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.session_not_found")),
                        Ok(Err(error)) => {
                            err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                        }
                        Err(error) => {
                            err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                        }
                    };
                }
            }
        }
        Err(error) => Some(error),
    };

    let error = final_error.expect("成功路径已提前返回");
    // 失败或取消轮次仅在会话仍存在时落盘，便于用户原样查看报错并一键重试。
    let turn_error = TurnError {
        status: error.upstream_status(),
        message: error.to_string(),
        raw_body: error.raw_body().map(str::to_owned),
    };
    let turn = PlaygroundTurn {
        id: turn_id,
        created_at_ms: now,
        model: model_selector.clone(),
        mode,
        prompt: prompt_trimmed.clone(),
        params,
        inputs: turn_inputs,
        outputs: Vec::new(),
        revised_prompt: None,
        duration_ms,
        error: Some(turn_error),
    };
    let _guard = state.image_playground.write_lock.lock().await;
    let dir_for_save = storage_dir.clone();
    let turn_for_save = turn.clone();
    let sid_for_save = session_id.clone();
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir_for_save)?;
        let Some(session) = index.sessions.iter_mut().find(|s| s.id == sid_for_save) else {
            return Ok::<_, anyhow::Error>(None);
        };
        if session.turns.is_empty() && matches!(session.title.as_str(), "New session" | "新生成会话") {
            session.title = derive_title(&prompt_trimmed);
        }
        session.model = Some(model_selector);
        session.updated_at_ms = now;
        session.turns.push(turn_for_save);
        let title = session.title.clone();
        store::save_index(&dir_for_save, &index)?;
        Ok::<_, anyhow::Error>(Some(title))
    })
    .await
    {
        Ok(Ok(Some(session_title))) => Json(GenerateResponse {
            turn,
            images: Vec::new(),
            session_title,
        })
        .into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.session_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn cancel_generate(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    state.image_playground.cancel_session(&session_id);
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ---------- 图片、画廊与收藏 ----------

#[derive(Debug, Clone, Serialize)]
pub struct ImageDto {
    #[serde(flatten)]
    pub image: StoredImage,
    pub favorite: bool,
    pub favorite_folder_id: Option<String>,
}

impl ImageDto {
    fn from_stored(
        image: &StoredImage,
        favorite: bool,
        favorite_folder_id: Option<String>,
    ) -> Self {
        Self {
            image: image.clone(),
            favorite,
            favorite_folder_id,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct ListImagesQuery {
    pub scope: Option<String>,
}

#[derive(Serialize)]
pub struct ImagesResponse {
    pub images: Vec<ImageDto>,
}

pub async fn list_images(
    State(state): State<AppState>,
    Query(query): Query<ListImagesQuery>,
) -> Response {
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let scope = query.scope.unwrap_or_else(|| "gallery".into());
    match tokio::task::spawn_blocking(move || {
        let index = store::load_index(&dir)?;
        let list = if scope == "favorites" {
            index
                .favorites
                .iter()
                .filter_map(|id| index.images.get(id))
                .map(|img| ImageDto::from_stored(img, true, index.favorite_folder_of(&img.id)))
                .collect::<Vec<_>>()
        } else {
            let mut items: Vec<_> = index
                .images
                .values()
                .filter(|img| img.origin == ImageOrigin::Generated)
                .map(|img| {
                    ImageDto::from_stored(
                        img,
                        index.is_favorite(&img.id),
                        index.favorite_folder_of(&img.id),
                    )
                })
                .collect();
            items.sort_by(|a, b| b.image.created_at_ms.cmp(&a.image.created_at_ms));
            items
        };
        Ok::<_, anyhow::Error>(list)
    })
    .await
    {
        Ok(Ok(images)) => Json(ImagesResponse { images }).into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// 直接上传本地参考图到收藏夹（支持可选 `folder_id` 字段归入指定收藏文件夹）。
pub async fn upload_favorites(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let upload_permit = match UPLOAD_READ_SEMAPHORE.acquire().await {
        Ok(permit) => permit,
        Err(_) => {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, t!("web.playground.upload_permit_unavailable"));
        }
    };
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
    let mut folder_id: Option<String> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => {
                return err_response(error.status(), t!("web.playground.multipart_read_failed", error = error));
            }
        };
        match field.name() {
            Some("folder_id") => {
                let text = match field.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.field_read_failed", error = error));
                    }
                };
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    folder_id = Some(trimmed.to_owned());
                }
            }
            Some("files" | "file" | "images" | "image") => {
                let filename = field
                    .file_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "reference.png".into());
                let mime = field
                    .content_type()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "application/octet-stream".into());
                match field.bytes().await {
                    Ok(bytes) if !bytes.is_empty() => {
                        files.push((filename, mime, bytes.to_vec()));
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return err_response(error.status(), t!("web.playground.upload_read_failed", error = error));
                    }
                }
            }
            _ => {}
        }
    }
    if files.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.at_least_one_image"));
    }

    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let _upload_permit = upload_permit;
        // 先校验全部上传文件，避免批量上传中途失败留下孤立磁盘文件。
        let mut inspected = Vec::with_capacity(files.len());
        for (filename, mime, bytes) in files {
            let meta = codec::inspect_image(&bytes, Some(&mime))?;
            inspected.push((filename, bytes, meta));
        }

        let mut index = store::load_index(&dir)?;
        let valid_folder = folder_id.filter(|fid| index.has_favorite_folder(fid));
        let mut added = Vec::new();
        for (filename, bytes, meta) in inspected {
            let id = store::new_id();
            let file_path = store::write_favorite_upload(&dir, &id, &meta.extension, &bytes)?;
            let img = StoredImage {
                id: id.clone(),
                origin: ImageOrigin::UploadedFavorite,
                session_id: None,
                turn_id: None,
                created_at_ms: store::now_ms(),
                model: None,
                prompt: None,
                revised_prompt: None,
                width: meta.width,
                height: meta.height,
                format: meta.format.clone(),
                extension: meta.extension,
                mime_type: meta.mime_type,
                is_animated: meta.is_animated,
                size_bytes: bytes.len() as u64,
                original_size_bytes: bytes.len() as u64,
                original_format: meta.format,
                file_path: file_path.to_string_lossy().into_owned(),
                filename,
            };
            index.images.insert(id.clone(), img.clone());
            index.set_favorite(&id, true);
            if let Some(ref fid) = valid_folder {
                let _ = index.set_favorite_image_folder(&id, Some(fid.clone()));
            }
            added.push(ImageDto::from_stored(&img, true, valid_folder.clone()));
        }
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(added)
    })
    .await
    {
        Ok(Ok(images)) => Json(ImagesResponse { images }).into_response(),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

/// 区分 JSON 中字段未提供 (`None`) 与显式设为 `null` (`Some(None)`)。
fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
pub struct ToggleFavoriteBody {
    pub favorite: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub folder_id: Option<Option<String>>,
}

pub async fn toggle_favorite(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ToggleFavoriteBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        if !index.images.contains_key(&id) {
            return Ok(None);
        }
        if let Some(fav) = body.favorite {
            index.set_favorite(&id, fav);
        }
        if let Some(folder_opt) = body.folder_id {
            let normalized = folder_opt.and_then(|s| {
                let trimmed = s.trim().to_owned();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            });
            index.set_favorite_image_folder(&id, normalized)?;
        }
        let is_fav = index.is_favorite(&id);
        let folder = index.favorite_folder_of(&id);
        let to_trash = if is_fav {
            Vec::new()
        } else {
            index.prune_orphan_preset_uploads(std::slice::from_ref(&id))
        };
        let removed = !to_trash.is_empty();
        store::save_index(&dir, &index)?;
        for (_, path) in to_trash {
            store::trash_or_remove(&path);
        }
        Ok::<_, anyhow::Error>(Some((is_fav, folder, removed)))
    })
    .await
    {
        Ok(Ok(Some((favorite, favorite_folder_id, removed)))) => Json(serde_json::json!({
            "ok": true,
            "favorite": favorite,
            "favorite_folder_id": favorite_folder_id,
            "removed": removed,
        }))
        .into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.image_not_found")),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

// ---------- 收藏文件夹与提示词/组合收藏 ----------

#[derive(Serialize)]
pub struct FavoritesMetaResponse {
    pub folders: Vec<FavoriteFolder>,
    pub presets: Vec<FavoritePreset>,
    /// 组合预设引用到的、但未必在单图收藏列表中的图片（避免前端缺图）。
    pub preset_images: Vec<ImageDto>,
}

pub async fn list_favorites_meta(State(state): State<AppState>) -> Response {
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    match tokio::task::spawn_blocking(move || {
        let index = store::load_index(&dir)?;
        let mut seen = std::collections::HashSet::new();
        let mut preset_images = Vec::new();
        for preset in &index.favorite_presets {
            for img_id in &preset.image_ids {
                if seen.insert(img_id.clone())
                    && let Some(img) = index.images.get(img_id)
                {
                    preset_images.push(ImageDto::from_stored(
                        img,
                        index.is_favorite(img_id),
                        index.favorite_folder_of(img_id),
                    ));
                }
            }
        }
        Ok::<_, anyhow::Error>(FavoritesMetaResponse {
            folders: index.favorite_folders,
            presets: index.favorite_presets,
            preset_images,
        })
    })
    .await
    {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize)]
pub struct FolderNameBody {
    pub name: String,
}

pub async fn create_favorite_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FolderNameBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.folder_name_empty"));
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let now = store::now_ms();
        let folder = FavoriteFolder {
            id: store::new_id(),
            name,
            created_at_ms: now,
            updated_at_ms: now,
        };
        index.favorite_folders.push(folder.clone());
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(folder)
    })
    .await
    {
        Ok(Ok(folder)) => Json(folder).into_response(),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn rename_favorite_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<FolderNameBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let name = body.name.trim().to_owned();
    if name.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.folder_name_empty"));
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(folder) = index.favorite_folders.iter_mut().find(|f| f.id == id) else {
            return Ok(None);
        };
        folder.name = name;
        folder.updated_at_ms = store::now_ms();
        let updated = folder.clone();
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(Some(updated))
    })
    .await
    {
        Ok(Ok(Some(folder))) => Json(folder).into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.folder_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn delete_favorite_folder(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        if !index.remove_favorite_folder(&id) {
            return Ok(false);
        }
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(true)
    })
    .await
    {
        Ok(Ok(true)) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.folder_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize)]
pub struct CreatePresetBody {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub image_ids: Vec<String>,
    #[serde(default)]
    pub source_uploads: Vec<UploadRefInput>,
    pub folder_id: Option<String>,
}

pub async fn create_favorite_preset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreatePresetBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let prompt = body.prompt.trim().to_owned();
    let title = body.title.trim().to_owned();
    if prompt.is_empty() && body.image_ids.is_empty() && body.source_uploads.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.preset_empty"));
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let folder_id = body.folder_id.and_then(|s| {
            let t = s.trim().to_owned();
            if t.is_empty() { None } else { Some(t) }
        });
        if let Some(ref fid) = folder_id
            && !index.has_favorite_folder(fid)
        {
            anyhow::bail!("{}", t!("web.playground.target_folder_not_found"));
        }
        let mut deduped_ids = Vec::new();
        for img_id in body.image_ids {
            if index.images.contains_key(&img_id) && !deduped_ids.contains(&img_id) {
                deduped_ids.push(img_id);
            }
        }
        // 先校验全部待晋升的临时上传图，避免中途失败留下孤立收藏文件。
        let mut prepared_uploads = Vec::with_capacity(body.source_uploads.len());
        for up_ref in body.source_uploads {
            if !is_safe_upload_id(&up_ref.upload_id) {
                anyhow::bail!("{}", t!("web.playground.invalid_upload_id"));
            }
            let path = dir.join("uploads").join(&up_ref.upload_id);
            let raw = std::fs::read(&path)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "{}",
                        t!(
                            "web.playground.temp_ref_read_failed",
                            id = up_ref.upload_id,
                            error = e
                        )
                    )
                })?;
            let meta = codec::inspect_image(&raw, None)?;
            let filename = up_ref
                .filename
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&up_ref.upload_id)
                .to_owned();
            prepared_uploads.push((filename, raw, meta));
        }

        if prompt.is_empty() && deduped_ids.is_empty() && prepared_uploads.is_empty() {
            anyhow::bail!("{}", t!("web.playground.preset_valid_empty"));
        }

        let now = store::now_ms();
        for (filename, raw, meta) in prepared_uploads {
            let img_id = store::new_id();
            let file_path = store::write_favorite_upload(&dir, &img_id, &meta.extension, &raw)?;
            let img = StoredImage {
                id: img_id.clone(),
                origin: ImageOrigin::UploadedFavorite,
                session_id: None,
                turn_id: None,
                created_at_ms: now,
                model: None,
                prompt: None,
                revised_prompt: None,
                width: meta.width,
                height: meta.height,
                format: meta.format.clone(),
                extension: meta.extension,
                mime_type: meta.mime_type,
                is_animated: meta.is_animated,
                size_bytes: raw.len() as u64,
                original_size_bytes: raw.len() as u64,
                original_format: meta.format,
                file_path: file_path.to_string_lossy().into_owned(),
                filename,
            };
            index.images.insert(img_id.clone(), img);
            deduped_ids.push(img_id);
        }

        let final_title = if !title.is_empty() {
            title
        } else if !prompt.is_empty() {
            derive_title(&prompt)
        } else {
            t!("web.playground.preset_refs_title", count = deduped_ids.len()).into_owned()
        };
        let preset = FavoritePreset {
            id: store::new_id(),
            title: final_title,
            prompt,
            image_ids: deduped_ids,
            folder_id,
            created_at_ms: now,
            updated_at_ms: now,
        };
        index.favorite_presets.insert(0, preset.clone());
        store::save_index(&dir, &index)?;
        Ok::<_, anyhow::Error>(preset)
    })
    .await
    {
        Ok(Ok(preset)) => Json(preset).into_response(),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Deserialize)]
pub struct PatchPresetBody {
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub image_ids: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub folder_id: Option<Option<String>>,
}

#[derive(Serialize)]
struct PatchPresetResponse {
    #[serde(flatten)]
    preset: FavoritePreset,
    removed_image_ids: Vec<String>,
}

pub async fn patch_favorite_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PatchPresetBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(pos) = index.favorite_presets.iter().position(|p| p.id == id) else {
            return Ok(None);
        };
        if let Some(ref folder_opt) = body.folder_id
            && let Some(fid) = folder_opt
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            && !index.has_favorite_folder(fid)
        {
            anyhow::bail!("{}", t!("web.playground.target_folder_not_found"));
        }
        let next_image_ids = body.image_ids.map(|ids| {
            let mut out = Vec::new();
            for img_id in ids {
                if index.images.contains_key(&img_id) && !out.contains(&img_id) {
                    out.push(img_id);
                }
            }
            out
        });
        let mut removed_ids = Vec::new();
        let updated = {
            let preset = &mut index.favorite_presets[pos];
            if let Some(title) = body.title {
                let trimmed = title.trim().to_owned();
                if !trimmed.is_empty() {
                    preset.title = trimmed;
                }
            }
            if let Some(prompt) = body.prompt {
                preset.prompt = prompt.trim().to_owned();
            }
            if let Some(ids) = next_image_ids {
                for old_id in &preset.image_ids {
                    if !ids.contains(old_id) {
                        removed_ids.push(old_id.clone());
                    }
                }
                preset.image_ids = ids;
            }
            if let Some(folder_opt) = body.folder_id {
                preset.folder_id = folder_opt.and_then(|s| {
                    let t = s.trim().to_owned();
                    if t.is_empty() { None } else { Some(t) }
                });
            }
            if preset.prompt.is_empty() && preset.image_ids.is_empty() {
                anyhow::bail!("{}", t!("web.playground.preset_empty"));
            }
            preset.updated_at_ms = store::now_ms();
            preset.clone()
        };
        let to_trash = index.prune_orphan_preset_uploads(&removed_ids);
        store::save_index(&dir, &index)?;
        let mut removed_image_ids = Vec::with_capacity(to_trash.len());
        for (img_id, path) in to_trash {
            removed_image_ids.push(img_id);
            store::trash_or_remove(&path);
        }
        Ok::<_, anyhow::Error>(Some(PatchPresetResponse {
            preset: updated,
            removed_image_ids,
        }))
    })
    .await
    {
        Ok(Ok(Some(resp))) => Json(resp).into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.preset_not_found")),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn delete_favorite_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(pos) = index.favorite_presets.iter().position(|p| p.id == id) else {
            return Ok(None);
        };
        let removed_preset = index.favorite_presets.remove(pos);
        let to_trash = index.prune_orphan_preset_uploads(&removed_preset.image_ids);
        store::save_index(&dir, &index)?;
        let mut removed_image_ids = Vec::with_capacity(to_trash.len());
        for (img_id, path) in to_trash {
            removed_image_ids.push(img_id);
            store::trash_or_remove(&path);
        }
        Ok::<_, anyhow::Error>(Some(removed_image_ids))
    })
    .await
    {
        Ok(Ok(Some(removed_image_ids))) => Json(serde_json::json!({
            "ok": true,
            "removed_image_ids": removed_image_ids,
        }))
        .into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.preset_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn delete_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(path) = index.remove_image(&id) else {
            return Ok(false);
        };
        store::save_index(&dir, &index)?;
        store::trash_or_remove(&path);
        Ok::<_, anyhow::Error>(true)
    })
    .await
    {
        Ok(Ok(true)) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.image_not_found")),
        Ok(Err(error)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvertFormat {
    Webp,
    WebpLossless,
    Avif,
    Jpeg,
    Png,
    WebpAnimated,
    Apng,
}

#[derive(Deserialize)]
pub struct ConvertImageBody {
    pub format: ConvertFormat,
    pub quality: Option<u8>,
}

#[derive(Serialize)]
pub struct ConvertImageResponse {
    pub image: ImageDto,
    pub previous_size_bytes: u64,
    pub previous_format: String,
}

/// 将已保存的本地图片原地转换为指定格式（原子写入新文件并更新索引）。
pub async fn convert_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ConvertImageBody>,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let (cfg, dir) = match load_cfg_and_dir(&state).await {
        Ok(pair) => pair,
        Err(resp) => return resp,
    };
    let _guard = state.image_playground.write_lock.lock().await;
    match tokio::task::spawn_blocking(move || {
        let mut index = store::load_index(&dir)?;
        let Some(existing) = index.images.get(&id).cloned() else {
            return Ok(None);
        };
        if existing.format.eq_ignore_ascii_case("avif") {
            anyhow::bail!("{}", t!("web.playground.avif_convert_unsupported"));
        }
        let old_path = PathBuf::from(&existing.file_path);
        let raw_bytes = std::fs::read(&old_path).map_err(|error| {
            anyhow::anyhow!("{}", t!("web.playground.source_read_failed", error = error))
        })?;

        let static_q = body
            .quality
            .unwrap_or(cfg.compression.quality)
            .clamp(1, 100);
        let anim_q = body
            .quality
            .unwrap_or(cfg.compression.animated_quality)
            .clamp(1, 100);

        let transcode_cfg = if existing.is_animated {
            let anim_fmt = match body.format {
                ConvertFormat::WebpAnimated | ConvertFormat::Webp => {
                    AnimatedOutputFormat::WebpAnimated
                }
                ConvertFormat::Apng | ConvertFormat::Png => AnimatedOutputFormat::Apng,
                _ => anyhow::bail!("{}", t!("web.playground.animated_convert_only")),
            };
            OutputCompressionConfig {
                format: OutputCompressionFormat::Original,
                quality: static_q,
                animated_format: anim_fmt,
                animated_quality: anim_q,
            }
        } else {
            let static_fmt = match body.format {
                ConvertFormat::Webp | ConvertFormat::WebpAnimated => OutputCompressionFormat::Webp,
                ConvertFormat::WebpLossless => OutputCompressionFormat::WebpLossless,
                ConvertFormat::Avif => OutputCompressionFormat::Avif,
                ConvertFormat::Jpeg => OutputCompressionFormat::Jpeg,
                ConvertFormat::Png | ConvertFormat::Apng => OutputCompressionFormat::Png,
            };
            OutputCompressionConfig {
                format: static_fmt,
                quality: static_q,
                animated_format: AnimatedOutputFormat::Original,
                animated_quality: anim_q,
            }
        };

        let encoded =
            codec::transcode_output(raw_bytes, Some(&existing.mime_type), &transcode_cfg)?;
        let parent = old_path.parent().unwrap_or(&dir);
        std::fs::create_dir_all(parent)?;
        let new_path = parent.join(format!("{id}.{}", encoded.extension));
        let tmp = tempfile::NamedTempFile::new_in(parent)?;
        std::fs::write(tmp.path(), &encoded.bytes)?;
        tmp.persist(&new_path).map_err(|error| error.error)?;

        let previous_size_bytes = existing.size_bytes;
        let previous_format = existing.format.clone();
        let is_fav = index.is_favorite(&id);
        let folder_id = index.favorite_folder_of(&id);

        let updated_stored = {
            let entry = index.images.get_mut(&id).expect("已校验存在");
            entry.width = encoded.width;
            entry.height = encoded.height;
            entry.format = encoded.format;
            entry.extension = encoded.extension.clone();
            entry.mime_type = encoded.mime_type;
            entry.is_animated = encoded.is_animated;
            entry.size_bytes = encoded.bytes.len() as u64;
            entry.file_path = new_path.to_string_lossy().into_owned();
            entry.filename = replace_ext(&entry.filename, &encoded.extension);
            entry.clone()
        };

        store::save_index(&dir, &index)?;
        if new_path != old_path {
            let _ = std::fs::remove_file(&old_path);
        }

        Ok::<_, anyhow::Error>(Some(ConvertImageResponse {
            image: ImageDto::from_stored(&updated_stored, is_fav, folder_id),
            previous_size_bytes,
            previous_format,
        }))
    })
    .await
    {
        Ok(Ok(Some(resp))) => Json(resp).into_response(),
        Ok(Ok(None)) => err_response(StatusCode::NOT_FOUND, t!("web.playground.image_not_found")),
        Ok(Err(error)) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
        Err(error) => err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn reveal_image(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let path = match tokio::task::spawn_blocking(move || {
        let index = store::load_index(&dir)?;
        Ok::<_, anyhow::Error>(index.images.get(&id).map(|i| PathBuf::from(&i.file_path)))
    })
    .await
    {
        Ok(Ok(Some(path))) => path,
        Ok(Ok(None)) => return err_response(StatusCode::NOT_FOUND, t!("web.playground.image_not_found")),
        Ok(Err(error)) => {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
        Err(error) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };
    match reveal::reveal_in_file_manager(&path).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(error) => err_response(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

#[derive(Deserialize, Default)]
pub struct FileQuery {
    pub download: Option<u8>,
}

pub async fn get_image_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Response {
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let record = match tokio::task::spawn_blocking(move || {
        let index = store::load_index(&dir)?;
        Ok::<_, anyhow::Error>(index.images.get(&id).cloned())
    })
    .await
    {
        Ok(Ok(Some(img))) => img,
        Ok(Ok(None)) => return err_response(StatusCode::NOT_FOUND, t!("web.playground.image_not_found")),
        Ok(Err(error)) => {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string());
        }
        Err(error) => return err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    };

    stream_file(
        PathBuf::from(&record.file_path),
        &record.mime_type,
        &record.filename,
        query.download == Some(1),
        false,
    )
    .await
}

pub async fn get_upload_file(
    State(state): State<AppState>,
    Path(upload_id): Path<String>,
) -> Response {
    if !is_safe_upload_id(&upload_id) {
        return err_response(StatusCode::BAD_REQUEST, t!("web.playground.invalid_filename"));
    }
    let dir = match resolve_storage_dir(&state).await {
        Ok(dir) => dir,
        Err(resp) => return resp,
    };
    let path = dir.join("uploads").join(&upload_id);
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("avif") => "image/avif",
        _ => "application/octet-stream",
    };
    stream_file(path, mime, &upload_id, false, true).await
}

/// 严格校验单段文件名，防止路径穿越。
fn is_safe_upload_id(upload_id: &str) -> bool {
    !upload_id.is_empty()
        && upload_id != "."
        && !upload_id.contains('/')
        && !upload_id.contains('\\')
        && !upload_id.contains("..")
}

async fn stream_file(
    path: PathBuf,
    mime: &str,
    filename: &str,
    download: bool,
    immutable: bool,
) -> Response {
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            return err_response(StatusCode::NOT_FOUND, t!("web.playground.local_file_not_found"));
        }
    };
    let Ok(metadata) = file.metadata().await else {
        return err_response(StatusCode::NOT_FOUND, t!("web.err.file_stat"));
    };
    let mut headers = HeaderMap::new();
    if let Ok(ct) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string()).expect("长度是数字"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "private, max-age=31536000, immutable"
        } else {
            "private, no-cache"
        }),
    );
    if download {
        let safe_name = filename.replace(['"', '\\', '\r', '\n'], "_");
        if let Ok(disp) = HeaderValue::from_str(&format!("attachment; filename=\"{safe_name}\"")) {
            headers.insert(header::CONTENT_DISPOSITION, disp);
        }
    }
    let stream = tokio_util::io::ReaderStream::new(file);
    (headers, Body::from_stream(stream)).into_response()
}

async fn load_cfg_and_dir(state: &AppState) -> Result<(PlaygroundConfig, PathBuf), Response> {
    let home = state.core.config_home();
    tokio::task::spawn_blocking(move || {
        let cfg = config::load(&home)?;
        let dir = cfg.resolved_storage_dir(&home);
        Ok::<_, anyhow::Error>((cfg, dir))
    })
    .await
    .map_err(|error| err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
    .map_err(|error| err_response(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

async fn resolve_storage_dir(state: &AppState) -> Result<PathBuf, Response> {
    load_cfg_and_dir(state).await.map(|(_, dir)| dir)
}

fn replace_ext(filename: &str, ext: &str) -> String {
    let stem = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("image");
    format!("{stem}.{ext}")
}

fn derive_title(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or(prompt).trim();
    let chars: Vec<char> = first_line.chars().collect();
    if chars.len() <= 28 {
        first_line.to_owned()
    } else {
        let prefix: String = chars.into_iter().take(28).collect();
        format!("{prefix}…")
    }
}

/// 进程内 Image Playground 并发与取消协调器。
#[derive(Clone, Default)]
pub struct ImagePlaygroundService {
    pub(crate) write_lock: Arc<tokio::sync::Mutex<()>>,
    flights: Arc<std::sync::Mutex<HashMap<String, (u64, CancellationToken)>>>,
    next_flight_id: Arc<AtomicU64>,
}

/// 单个生成请求的在途租约：Drop 时按精确 ID 自动释放槽位，防止提前返回或任务中止时泄漏。
pub(crate) struct FlightGuard {
    service: ImagePlaygroundService,
    session_id: String,
    flight_id: u64,
    pub(crate) cancel: CancellationToken,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.service.clear_flight(&self.session_id, self.flight_id);
    }
}

impl ImagePlaygroundService {
    pub(crate) fn register_flight(&self, session_id: &str) -> FlightGuard {
        let flight_id = self.next_flight_id.fetch_add(1, Ordering::Relaxed) + 1;
        let cancel = CancellationToken::new();
        let mut map = self.flights.lock().expect("flights lock");
        if let Some((_, prev)) = map.insert(session_id.to_owned(), (flight_id, cancel.clone())) {
            prev.cancel();
        }
        FlightGuard {
            service: self.clone(),
            session_id: session_id.to_owned(),
            flight_id,
            cancel,
        }
    }

    pub(crate) fn clear_flight(&self, session_id: &str, flight_id: u64) {
        let mut map = self.flights.lock().expect("flights lock");
        if map
            .get(session_id)
            .is_some_and(|(current_id, _)| *current_id == flight_id)
        {
            map.remove(session_id);
        }
    }

    pub(crate) fn cancel_session(&self, session_id: &str) {
        if let Some((_, token)) = self
            .flights
            .lock()
            .expect("flights lock")
            .remove(session_id)
        {
            token.cancel();
        }
    }
}
