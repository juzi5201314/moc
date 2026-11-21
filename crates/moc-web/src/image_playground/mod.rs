//! Image Playground 服务端模块：独立于 Agent 会话，复用 `moc-core` 供应商与模型配置。

mod codec;
mod config;
mod reveal;
mod routes;
mod store;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post};

use crate::AppState;

pub use routes::ImagePlaygroundService;

/// 构建挂在 `/image-playground/api` 下的全部受令牌保护路由。
pub fn router() -> Router<AppState> {
    let upload_limit = DefaultBodyLimit::max(128 * 1024 * 1024);
    Router::new()
        .route("/image-playground/api/models", get(routes::list_models))
        .route(
            "/image-playground/api/config",
            get(routes::get_config).put(routes::put_config),
        )
        .route(
            "/image-playground/api/config/open-dir",
            post(routes::open_storage_dir),
        )
        .route(
            "/image-playground/api/sessions",
            get(routes::list_sessions).post(routes::create_session),
        )
        .route(
            "/image-playground/api/sessions/{id}",
            get(routes::get_session)
                .patch(routes::patch_session)
                .delete(routes::delete_session),
        )
        .route(
            "/image-playground/api/sessions/{id}/generate",
            post(routes::generate).route_layer(upload_limit),
        )
        .route(
            "/image-playground/api/sessions/{id}/cancel",
            post(routes::cancel_generate),
        )
        .route("/image-playground/api/images", get(routes::list_images))
        .route(
            "/image-playground/api/favorites",
            get(routes::list_favorites_meta),
        )
        .route(
            "/image-playground/api/favorites/upload",
            post(routes::upload_favorites).route_layer(upload_limit),
        )
        .route(
            "/image-playground/api/favorites/folders",
            post(routes::create_favorite_folder),
        )
        .route(
            "/image-playground/api/favorites/folders/{id}",
            patch(routes::rename_favorite_folder).delete(routes::delete_favorite_folder),
        )
        .route(
            "/image-playground/api/favorites/presets",
            post(routes::create_favorite_preset),
        )
        .route(
            "/image-playground/api/favorites/presets/{id}",
            patch(routes::patch_favorite_preset).delete(routes::delete_favorite_preset),
        )
        .route(
            "/image-playground/api/images/{id}/favorite",
            patch(routes::toggle_favorite),
        )
        .route(
            "/image-playground/api/images/{id}",
            delete(routes::delete_image),
        )
        .route(
            "/image-playground/api/images/{id}/convert",
            post(routes::convert_image),
        )
        .route(
            "/image-playground/api/images/{id}/reveal",
            post(routes::reveal_image),
        )
        .route(
            "/image-playground/api/images/{id}/file",
            get(routes::get_image_file),
        )
        .route(
            "/image-playground/api/uploads/{id}",
            get(routes::get_upload_file),
        )
}
