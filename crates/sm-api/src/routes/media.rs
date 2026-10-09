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
use crate::dto::{MediaPointResource, MediaProgressResource, MediaThumbnailResource};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/media", get(list_media))
        .route(
            "/media/thumbnail-generation/reset",
            post(reset_terminal_media_thumbnails),
        )
        .route("/media/invalid", get(list_invalid_media))
        .route("/media/duplicates", get(list_duplicate_media_groups))
        .route(
            "/media/multi-version-movies",
            get(list_multi_version_movies),
        )
        .route(
            "/media/{media_id}/points",
            get(list_media_points_for_media).post(create_media_point),
        )
        .route(
            "/media/{media_id}/points/{point_id}",
            delete(delete_media_point),
        )
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
/// `POST /media/{media_id}/points` 的请求体（上游 `MediaPointCreateRequest`）。
///
/// **只有一个字段，且是必填**（上游 `Field(gt=0)` + validator）。
/// 时刻的秒偏移**不在这里** —— 它取自 `thumbnail.offset`，见
/// `MediaService::create_point` 的文档（两个真相源会打架）。
#[derive(Debug, Deserialize)]
pub struct MediaPointCreateRequest {
    pub thumbnail_id: i32,
}

/// `PUT /media/{media_id}/progress` 的请求体（上游
/// `MediaProgressUpdateRequest`）。
#[derive(Debug, Deserialize)]
pub struct MediaProgressUpdateRequest {
    /// 上游 `Field(ge=0)`。负值在 service 层拦成 422 `validation_error`。
    pub position_seconds: i32,
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

/// 每请求解析出的签名密钥（时刻点的图片要签 URL）。
fn secret(state: &AppState) -> Result<String, ErrorResponse> {
    let config = crate::config::snapshot_or_500(state)?;
    Ok(
        crate::config::string_at(&config, "auth", "file_signature_secret")
            .unwrap_or_default()
            .to_owned(),
    )
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// 路径里的 id 是 `i64`（见模块文档），而库里的列是 `i32`。
///
/// # 超出 `i32` 时是 **404，不是 400**
///
/// 上游是 Python 的 `int`，**没有上界** —— 那种 id 会一路走到查询、查不到、
/// 报「不存在」。用 `Path<i32>` 会在提取阶段就变成 400，与上游不一致。
/// 所以这里保留 `i64` 再显式收窄，溢出按「不存在」处理。
fn narrow_media_id(media_id: i64) -> Result<i32, ErrorResponse> {
    i32::try_from(media_id).map_err(|_| {
        let mut details = serde_json::Map::new();
        details.insert("media_id".to_owned(), serde_json::Value::from(media_id));
        ErrorResponse::from(sm_service::error::ServiceError::not_found_with(
            "media_not_found",
            "Media not found",
            details,
        ))
    })
}

/// 同上，但用于 `point_id`：**404 而不是 400**，理由见 [`narrow_media_id`]。
fn narrow_point_id(point_id: i64) -> Result<i32, ErrorResponse> {
    i32::try_from(point_id).map_err(|_| {
        let mut details = serde_json::Map::new();
        details.insert(
            "media_point_id".to_owned(),
            serde_json::Value::from(point_id),
        );
        ErrorResponse::from(sm_service::error::ServiceError::not_found_with(
            "media_point_not_found",
            "media_point not found",
            details,
        ))
    })
}

/// `GET /media/{media_id}/points` —— **裸列表**（不分页）。
async fn list_media_points_for_media(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(media_id): Path<i64>,
) -> Result<Json<Vec<MediaPointResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let values = state
        .media_service()
        .list_points(narrow_media_id(media_id)?)
        .await?;
    let now = now_seconds();
    Ok(Json(
        values
            .iter()
            .map(|value| MediaPointResource::from_value(&secret, now, value))
            .collect(),
    ))
}

/// `POST /media/{media_id}/points` —— **新建 201 / 已存在 200**。
///
/// 状态码取决于 service 的第二个返回值：上游 `create_point` 是幂等的，
/// 命中既有行时报 `False`，路由据此返回 **200**（不是 201）。
/// 这不是「返回码随意」—— 客户端靠它区分「我这次真的建了一个点」。
async fn create_media_point(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(media_id): Path<i64>,
    EnvelopeJson(payload): EnvelopeJson<MediaPointCreateRequest>,
) -> Result<(StatusCode, Json<MediaPointResource>), ErrorResponse> {
    let secret = secret(&state)?;
    let (value, created) = state
        .media_service()
        .create_point(narrow_media_id(media_id)?, payload.thumbnail_id)
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(MediaPointResource::from_value(
            &secret,
            now_seconds(),
            &value,
        )),
    ))
}

/// `DELETE /media/{media_id}/points/{point_id}` —— **204 且无 body**。
///
/// 服务侧会连带清掉那张只服务于这个时刻的图（记录 + 磁盘文件，经
/// `ImageCleanupService`）。
async fn delete_media_point(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path((media_id, point_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ErrorResponse> {
    state
        .media_service()
        .delete_point(narrow_media_id(media_id)?, narrow_point_id(point_id)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /media/{media_id}/progress`
async fn update_media_progress(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(media_id): Path<i64>,
    EnvelopeJson(payload): EnvelopeJson<MediaProgressUpdateRequest>,
) -> Result<Json<MediaProgressResource>, ErrorResponse> {
    let value = state
        .media_service()
        .update_progress(narrow_media_id(media_id)?, payload.position_seconds)
        .await?;
    Ok(Json(MediaProgressResource::from_value(&value)))
}

/// `GET /media/{media_id}/thumbnails` —— **裸列表**（不分页）。
async fn list_media_thumbnails(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(media_id): Path<i64>,
) -> Result<Json<Vec<MediaThumbnailResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let values = state
        .media_service()
        .list_thumbnails(narrow_media_id(media_id)?)
        .await?;
    let now = now_seconds();
    Ok(Json(
        values
            .iter()
            .map(|value| MediaThumbnailResource::from_value(&secret, now, value))
            .collect(),
    ))
}

/// `DELETE /media/{media_id}` —— **204 且无 body**。
///
/// `sync_video_member` 上游是**关键字参数、默认 `True`**，而这个端点**不传它**
/// —— 所以走的是「非 JAV 媒体连它的视频条目一起删」那一支。把它做成查询参数
/// 会改变契约（默认行为不同）。
async fn delete_media(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(media_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    state.media_service().delete_media(media_id, true).await?;
    Ok(StatusCode::NO_CONTENT)
}
