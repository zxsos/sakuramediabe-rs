//! `POST/PATCH/DELETE /playlists` —— 让已完成的 `sm-service::collections`
//! 第一次能被 HTTP 触达。
//!
//! # 与上游 `src/api/routers/collections/playlists.py` 的对应
//!
//! 上游 9 个端点里，本批落了 **7** 个。剩下 1 个读端点
//! （`GET /playlists/{id}/movies`）依赖尚未实现的影片卡片聚合
//! （`with_movie_card_relations` / `attach_movie_list_media` /
//! `MovieListItemResource`）—— **不是**路由层写不出来。
//!
//! | 上游 | 本文件 | 状态 |
//! |---|---|---|
//! | `GET ""` | `list_playlists` | 已落（系统列表排序 + 批量计数） |
//! | `POST ""` | `create_playlist` | 已落 |
//! | `GET /{id}` | `get_playlist` | 已落（**含真实 `movie_count`**） |
//! | `PATCH /{id}` | `update_playlist` | 已落（**含真实 `movie_count`**） |
//! | `DELETE /{id}` | `delete_playlist` | 已落 |
//! | `GET /{id}/resolutions` | `list_playlist_resolutions` | 已落（分辨率档位聚合） |
//! | `PUT /{id}/movies/{n}` | `add_movie` | 已落 |
//! | `DELETE /{id}/movies/{n}` | `remove_movie` | 已落 |
//! | `GET /{id}/movies` | —— | **待做**：影片卡片聚合 |
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
// 这里只导入 `get` / `put` 两个**自由函数**：`patch` / `post` / `delete`
// 都以 `MethodRouter` 的方法形式出现（`.post(...)` / `.patch(...)`），
// 那不是自由函数，导入 `axum::routing::post` 反而是未使用的导入。
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use sm_service::collections::playlist::{PlaylistService, PlaylistUpdate};

use crate::auth::CurrentUser;
// 注意是 crate 自己的 Json —— axum 的那个会让解析失败绕过错误信封，
// 见 extract.rs 的文档。
use crate::dto::{
    PlaylistCreateRequest, PlaylistResolutionOption, PlaylistResource, PlaylistUpdateRequest,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{default_true, deser_bool};
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/playlists",
            get(list_playlists)
                .post(create_playlist)
                .fallback(method_not_allowed),
        )
        .route(
            "/playlists/{id}",
            get(get_playlist)
                .patch(update_playlist)
                .delete(delete_playlist)
                .fallback(method_not_allowed),
        )
        .route(
            "/playlists/{id}/resolutions",
            get(list_playlist_resolutions).fallback(method_not_allowed),
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

/// `GET /playlists` 的查询参数。
///
/// `include_system` **默认为真**，且布尔值按 pydantic 的 lax 规则解析 ——
/// 见 [`crate::query`] 的模块文档，那 12 个字面量（`1` / `yes` / `on` …）
/// 与 serde 默认的 `true/false` 不是一回事。
#[derive(Debug, Deserialize)]
struct ListPlaylistsQuery {
    #[serde(default = "default_true", deserialize_with = "deser_bool")]
    include_system: bool,
}

async fn list_playlists(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListPlaylistsQuery>,
) -> Result<Json<Vec<PlaylistResource>>, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    let rows = service.list(query.include_system).await?;
    Ok(Json(rows.iter().map(PlaylistResource::from).collect()))
}

async fn list_playlist_resolutions(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistPath>,
) -> Result<Json<Vec<PlaylistResolutionOption>>, ErrorResponse> {
    let service = PlaylistService::new(state.db());
    let options = service.resolution_options(path.id).await?;
    Ok(Json(
        options.iter().map(PlaylistResolutionOption::from).collect(),
    ))
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
    let playlist = service.get(path.id).await?;
    // 上游 `get_playlist` 返回真实计数，不是 0。
    let movie_count = service.member_count(path.id).await?;
    Ok(Json(PlaylistResource::with_movie_count(
        playlist,
        movie_count,
    )))
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
    // 计数在更新**之后**取：改名不改变成员数，但更新会推进 `updated_at`，
    // 两次查询之间若有人加片，上游同样会读到那个时刻的值。
    let movie_count = service.member_count(path.id).await?;
    Ok(Json(PlaylistResource::with_movie_count(
        playlist,
        movie_count,
    )))
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
