//! `POST/PATCH/DELETE /playlists` —— 让已完成的 `sm-service::collections`
//! 第一次能被 HTTP 触达。
//!
//! # 与上游 `src/api/routers/collections/playlists.py` 的对应
//!
//! 上游 9 个端点里，本批落了 5 个。没落的 4 个都依赖 service 尚未实现的
//! 查询编排（`list_playlists` / `list_playlist_movies` / `list_playlist_resolutions`
//! 需要 `movie_resolution_service` 的聚合），**不是**路由层写不出来。
//!
//! # 鉴权挂在 handler 上而不是 router 上
//!
//! 上游这个 router 用的是 `dependencies=[Depends(db_deps)]` + 逐个 handler
//! 声明 `current_user`（见该文件 `:18` 与 `:27`）。Rust 侧把
//! `CurrentUser` 写成提取器参数，形状一致：**每个 handler 显式声明它需要
//! 认证**，而不是靠一个 router 级的 layer 悄悄生效。后者会让"这个端点其实
//! 没鉴权"变得看不见。
//!
//! # 405 必须走 [`method_not_allowed`]
//!
//! axum 对「路径命中、方法不匹配」默认返回 405 + **空响应体**，且**不经过**
//! router 的 fallback。客户端拿到的会是"状态码对、body 解析失败"。
//! 每个 `MethodRouter` 都要显式挂 `.fallback()` 才能把 405 变成信封。

use axum::extract::{Path, State};
use axum::http::StatusCode;
// `patch` / `delete` 是 MethodRouter 上的方法而非自由函数，只需要 `get/post/put`。
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use sm_service::collections::playlist::{PlaylistService, PlaylistUpdate};

use crate::auth::CurrentUser;
// 注意是 crate 自己的 Json —— axum 的那个会让解析失败绕过错误信封，
// 见 extract.rs 的文档。
use crate::dto::{PlaylistCreateRequest, PlaylistResource, PlaylistUpdateRequest};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/playlists",
            post(create_playlist).fallback(method_not_allowed),
        )
        .route(
            "/playlists/{id}",
            get(get_playlist)
                .patch(update_playlist)
                .delete(delete_playlist)
                .fallback(method_not_allowed),
        )
        .route(
            "/playlists/{id}/movies/{movie_number}",
            put(add_movie)
                .delete(remove_movie)
                .fallback(method_not_allowed),
        )
}

/// axum 0.8 不再支持 `Path((a, b))` 解构，多个路径参数必须走具名结构体。
#[derive(Debug, Deserialize)]
struct PlaylistPath {
    id: i32,
}

#[derive(Debug, Deserialize)]
struct PlaylistMoviePath {
    id: i32,
    movie_number: String,
}

async fn create_playlist(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<PlaylistCreateRequest>,
) -> Result<(StatusCode, Json<PlaylistResource>), ErrorResponse> {
    let service = PlaylistService::new(state.db());
    let playlist = service
        .create(&payload.name, Some(&payload.description))
        .await?;
    Ok((StatusCode::CREATED, Json(PlaylistResource::from(playlist))))
}

async fn get_playlist(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistPath>,
) -> Result<Json<PlaylistResource>, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    Ok(Json(PlaylistResource::from(service.get(path.id).await?)))
}

async fn update_playlist(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistPath>,
    EnvelopeJson(payload): EnvelopeJson<PlaylistUpdateRequest>,
) -> Result<Json<PlaylistResource>, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    let playlist = service
        .update(
            path.id,
            PlaylistUpdate {
                name: payload.name,
                description: payload.description,
            },
        )
        .await?;
    Ok(Json(PlaylistResource::from(playlist)))
}

/// 上游对删除返回 `Response(status_code=204)` —— **空响应体**。
async fn delete_playlist(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistPath>,
) -> Result<StatusCode, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    service.delete(path.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_movie(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistMoviePath>,
) -> Result<StatusCode, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    service.add_movie(path.id, &path.movie_number).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_movie(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistMoviePath>,
) -> Result<StatusCode, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    service.remove_movie(path.id, &path.movie_number).await?;
    Ok(StatusCode::NO_CONTENT)
}
