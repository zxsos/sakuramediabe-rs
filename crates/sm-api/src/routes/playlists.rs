//! `POST/PATCH/DELETE /playlists` —— 让已完成的 `sm-service::collections`
//! 第一次能被 HTTP 触达。
//!
//! # 与上游 `src/api/routers/collections/playlists.py` 的对应
//!
//! 上游 9 个端点**全部落地**。
//!
//! | 上游 | 本文件 | 状态 |
//! |---|---|---|
//! | `GET ""` | `list_playlists` | 已落（系统列表排序 + 批量计数） |
//! | `POST ""` | `create_playlist` | 已落 |
//! | `GET /{id}` | `get_playlist` | 已落（**含真实 `movie_count`**） |
//! | `PATCH /{id}` | `update_playlist` | 已落（**含真实 `movie_count`**） |
//! | `DELETE /{id}` | `delete_playlist` | 已落 |
//! | `GET /{id}/resolutions` | `list_playlist_resolutions` | 已落（分辨率档位聚合） |
//! | `GET /{id}/movies` | `list_playlist_movies` | 已落（影片卡片，见下） |
//! | `PUT /{id}/movies/{n}` | `add_movie` | 已落 |
//! | `DELETE /{id}/movies/{n}` | `remove_movie` | 已落 |
//!
//! # `GET /{id}/movies` 的四个查询参数都不校验
//!
//! 上游写的是 `page: int = 1, page_size: int = 20, sort: str | None = None,
//! resolution: str | None = None` —— 全是裸类型，没有 `ge` / `le`。这里照抄，
//! **不加** `validate_page`：给 `page_size` 加个上限是行为变更，而客户端已经
//! 按「传多少给多少」用它。分页与筛选的规则都在
//! [`PlaylistService::list_playlist_movies`]。
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
use sm_core::pagination::Paginated;
use sm_service::collections::playlist::{PlaylistService, PlaylistUpdate};

use crate::auth::CurrentUser;
// 注意是 crate 自己的 Json —— axum 的那个会让解析失败绕过错误信封，
// 见 extract.rs 的文档。
use crate::dto::{
    PlaylistCreateRequest, PlaylistMovieListItemResource, PlaylistResolutionOption,
    PlaylistResource, PlaylistUpdateRequest,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{default_true, deser_bool};
use crate::routes::method_not_allowed;
use crate::signing::{now_seconds, signing_secret};
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
            "/playlists/{id}/movies",
            get(list_playlist_movies).fallback(method_not_allowed),
        )
        // 与上面那条不是同一个形状（多一段静态以外的路径段），matchit 按段数
        // 区分，不冲突。
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

/// `GET /playlists/{id}/movies` 的查询参数。
///
/// 四个参数都是**裸类型**，缺省值照抄上游 `page=1, page_size=20` ——
/// 为什么不加 `ge` / `le` 见本模块文档。
#[derive(Debug, Deserialize)]
struct ListPlaylistMoviesQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
    /// `field:direction`，四个字段见 `parse_playlist_sort`。非法值 422
    /// `invalid_playlist_filter`（`details.sort` 回显原始输入）。
    #[serde(default)]
    sort: Option<String>,
    /// 分辨率档位标签（`4K` / `1080P` …）。非法值同样是 422
    /// `invalid_playlist_filter`，但 `details` 的键是 `resolution` ——
    /// 客户端据此高亮不同的控件。
    #[serde(default)]
    resolution: Option<String>,
}

fn default_page() -> i64 {
    1
}

fn default_page_size() -> i64 {
    20
}

async fn list_playlist_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<PlaylistPath>,
    EnvelopeQuery(query): EnvelopeQuery<ListPlaylistMoviesQuery>,
) -> Result<Json<Paginated<PlaylistMovieListItemResource>>, ErrorResponse> {
    let page = PlaylistService::new(state.db())
        .list_playlist_movies(
            path.id,
            query.page,
            query.page_size,
            query.sort.as_deref(),
            query.resolution.as_deref(),
        )
        .await?;

    // 密钥与当前时间每次请求现取 —— 见 [`crate::signing`]。
    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| PlaylistMovieListItemResource::from_card(card, &secret, now))
        .collect();

    // **回显请求里的 page / page_size**，而不是 service 归一后的值：上游
    // `PageResponse` 装的就是入参，客户端据此拼下一页的 URL。
    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
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
