//! `/media*` 的十二个资源端点（播放那两个在 [`super::media_playback`]）。
//!
//! # 与上游 `src/api/routers/playback/media.py` 的对应（prefix `/media`）
//!
//! | 上游端点 | 备注 |
//! |---|---|
//! | `GET /media` | 七个查询参数 |
//! | `POST /media/thumbnail-generation/reset` | |
//! | `GET /media/invalid` | |
//! | `GET /media/duplicates` | **`kind` 必填** |
//! | `GET /media/multi-version-movies` | `include_vr` / `include_fc2` |
//! | `GET /media/{media_id}/points` | |
//! | `POST /media/{media_id}/points` | |
//! | `DELETE /media/{media_id}/points/{point_id}` | 204 |
//! | `PUT /media/{media_id}/progress` | |
//! | `GET /media/{media_id}/thumbnails` | |
//! | `DELETE /media/{media_id}` | 204 |
//!
//! 加上 `media_playback.rs` 的两个播放端点与一个 playback-attempt，本组共
//! 14 个 —— 与上游 `media.py` 一致。
//!
//! # 一处**路由优先级依赖**，注册时别改坏
//!
//! 字面量路径与参数路径混在同一前缀下：
//!
//! ```text
//!   /media/invalid           <- 字面量
//!   /media/{media_id}/points <- 参数
//! ```
//!
//! axum 的 matchit 按段匹配、`/media/invalid`（2 段）与
//! `/media/{media_id}/points`（3 段）**不冲突**。真正的依赖是：
//! `media_id` 的类型**必须是 `i64`**，这样 `GET /media/invalid` 之外的非法值
//! 会 422 而不会去撞字面量路由。**把它改成 `String` 就会让 `media_id="thumbnail"`
//! 之类的路径落进意料之外的分支。**
//!
//! # `kind` 在 `/media/duplicates` 上**必填**，在 `/media` 上有默认
//!
//! - `GET /media`：`kind: MediaPointKind = Query(default=ALL)`
//! - `GET /media/duplicates`：`kind: Literal["jav","video"] = Query(...)` ——
//!   `...` 是 Ellipsis，**没有 default**
//!
//! 同名参数一个必填一个有默认。照抄。给 `/media/duplicates` 补默认值会让
//! 客户端以为可以不带，而它返回的是完全不同的数据（重复组 vs 全部媒体）。
//!
//! # 204 的两个端点**不能带 body**
//!
//! `delete_media_point` 与 `delete_media` 返回 `StatusCode`，**不返回
//! `Json<()>`** —— 那会让 axum 产出 `null` 正文，与 204 语义冲突，
//! 部分客户端会因此报错。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/media", get(list_media))
        .route("/media/thumbnail-generation/reset", post(reset_terminal_media_thumbnails))
        .route("/media/invalid", get(list_invalid_media))
        .route("/media/duplicates", get(list_duplicate_media_groups))
        .route("/media/multi-version-movies", get(list_multi_version_movies))
        .route(
            "/media/{media_id}/points",
            get(list_media_points_for_media).post(create_media_point),
        )
        .route("/media/{media_id}/points/{point_id}", delete(delete_media_point))
        .route("/media/{media_id}/progress", put(update_media_progress))
        .route("/media/{media_id}/thumbnails", get(list_media_thumbnails))
        .route("/media/{media_id}", delete(delete_media))
}

/// 通用分页查询（`page` 默认 1、`page_size` 默认 20）。
///
/// **上游这几个端点的 `page` / `page_size` 没有 `ge` / `le` 边界**（裸
/// `page: int = 1`），所以不做夹取 —— 夹了会改变契约。
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
}
/// `GET /media` 的查询参数。
#[derive(Debug, Default, Deserialize)]
pub struct ListMediaQuery {
    /// 媒体点类型；**默认 `all`**。
    pub kind: Option<String>,
    pub library_id: Option<i64>,
    /// **CSV 字符串**，不是数组 —— 与 image_search 的同名参数一致。
    pub actor_ids: Option<String>,
    pub thumbnail_generation_state: Option<String>,
    /// 非法值**降级**（见 [`crate::query`] 的既有约定）。
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

async fn list_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<ListMediaQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接 playback 域的媒体列表查询")
}

/// `POST /media/thumbnail-generation/reset`
async fn reset_terminal_media_thumbnails(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接缩略图终态重置")
}

/// `GET /media/invalid`
#[derive(Debug, Default, Deserialize)]
pub struct InvalidMediaQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub search: Option<String>,
}

async fn list_invalid_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<InvalidMediaQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接无效媒体列表")
}

/// `GET /media/duplicates` —— **`kind` 必填**。
#[derive(Debug, Default, Deserialize)]
pub struct DuplicatesQuery {
    /// 上游是 `Query(...)`（**无 default**）—— 缺了要 422。
    ///
    /// 用 `Option` 是为了能区分「没传」与「传了空串」，校验放在 handler。
    /// 不能靠 `#[serde(default)]` 出一个默认值 —— 那等于把必填改成可选。
    pub kind: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

async fn list_duplicate_media_groups(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(query): axum::extract::Query<DuplicatesQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    if query.kind.is_none() {
        return Err(sm_service::error::ServiceError::validation(
            "validation_error",
            "kind 是必填项，取值为 jav 或 video",
        )
        .into());
    }
    todo!("骨架：kind 只接受 jav / video；接重复媒体组查询")
}

/// `GET /media/multi-version-movies`
#[derive(Debug, Default, Deserialize)]
pub struct MultiVersionQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    /// 包含番号含 VR 或拥有 VR 标签的影片。
    #[serde(default)]
    pub include_vr: bool,
    /// 包含番号以 `FC2` 开头的影片。
    #[serde(default)]
    pub include_fc2: bool,
}

async fn list_multi_version_movies(
    State(_state): State<AppState>,
    _user: CurrentUser,
    axum::extract::Query(_query): axum::extract::Query<MultiVersionQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接多版本影片查询")
}

/// `GET /media/{media_id}/points`
async fn list_media_points_for_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_media_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接媒体点列表")
}

/// `POST /media/{media_id}/points`
async fn create_media_point(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_media_id): Path<i64>,
    _payload: axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接媒体点创建")
}

/// `DELETE /media/{media_id}/points/{point_id}` —— **204 且无 body**。
async fn delete_media_point(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path((_media_id, _point_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接媒体点删除（成功返回 204，不带 body）")
}

/// `PUT /media/{media_id}/progress`
async fn update_media_progress(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_media_id): Path<i64>,
    _payload: axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接播放进度更新")
}

/// `GET /media/{media_id}/thumbnails`
async fn list_media_thumbnails(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_media_id): Path<i64>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接缩略图列表")
}

/// `DELETE /media/{media_id}` —— **204 且无 body**。
async fn delete_media(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_media_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接媒体删除（成功返回 204，不带 body）")
}