//! `/videos*` —— 视频条目五个端点。
//!
//! # 与上游 `src/api/routers/videos/items.py` 的对应
//!
//! prefix `/videos`。
//!
//! | 上游端点 | 状态码 | 响应模型 |
//! |---|---|---|
//! | `GET ""` | 200 | `PageResponse[VideoItemListItemResource]` |
//! | `POST ""` | **201** | `VideoItemDetailResource` |
//! | `GET /{id}` | 200 | `VideoItemDetailResource` |
//! | `PATCH /{id}` | 200 | `VideoItemDetailResource` |
//! | `DELETE /{id}` | **204** | —— |
//!
//! # 列表与详情是**两个不同的响应模型**
//!
//! `VideoItemListItemResource`（列表）vs `VideoItemDetailResource`（详情）——
//! **不要**用同一个类型。详情是**列表项的派生**：14 字段原样在顶层
//! （`#[serde(flatten)]`），外加 `media_items: list[MovieMediaResource]`
//! （上游 `schema/videos/items.py:37-38`）。
//!
//! 写 / `PATCH` 返回的是**详情**模型（不是列表模型）—— 所以「改完拿到的
//! 对象」与「列表里那个对象」字段不同。这是刻意的，别统一。
//!
//! # 详情的 `media_items[].play_url`
//!
//! 逐条签名，交付方式取 provider 的 `playback_deliveries[0]`。两条细节：
//!
//! - **失效媒体给空串**（不是 `null`）：`play_url` 在详情里是**非空 `str`**，
//!   空串是明确的「这条播不了」。别套用合集成员那条「空串 ≠ null」的红线。
//! - **provider 查不到就是 500**（不是 503）：上游这里漏了 `try/except`，
//!   见 `detail_resource`。
//!
//! # 与 `video_collections.rs` 是**两个资源**
//!
//! - 本文件：`/videos` —— 视频条目本身（CRUD）
//! - `video_collections.rs`：`/video-collections` —— 视频的集合（歌单式）
//!
//! 两者独立：一个视频可以不属于任何集合，一个集合可以为空。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use sm_service::videos::{
    Field, VideoItemCreate, VideoItemDetail, VideoItemService, VideoItemUpdate,
};

use crate::auth::CurrentUser;
use crate::dto::{
    deserialize_double_option, MovieMediaResource, VideoItemDetailResource,
    VideoItemListItemResource,
};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::extract::Query as EnvelopeQuery;
use crate::query::{one, twenty};
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/videos",
            get(list_videos)
                .post(create_video)
                .fallback(method_not_allowed),
        )
        .route(
            "/videos/{video_id}",
            get(get_video)
                .patch(update_video)
                .delete(delete_video)
                .fallback(method_not_allowed),
        )
}

/// `POST ""` 的请求体（上游 `VideoItemCreateRequest`）。
///
/// ⚠️ 骨架期写成 `{ title, source_uri }` —— 上游是
/// `{ title（必填、`min_length=1`）、summary（缺省空串）、release_date }`。
/// **`source_uri` 上游没有这个键**（那是下载域的概念）。已按上游更正。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequest {
    pub title: String,
    /// 缺省空串。上游 `summary: str = ""`。
    #[serde(default)]
    pub summary: String,
    pub release_date: Option<chrono::NaiveDateTime>,
}

/// `PATCH /{id}` 的请求体（上游 `VideoItemUpdateRequest`）。
///
/// ⚠️ 骨架期写成 `{ title, description }` —— 上游是
/// `{ title, summary, release_date, cover_thumbnail_id }`。
///
/// # 四个字段的 null 语义**各不相同**，所以必须保住三态
///
/// ```text
/// {"cover_thumbnail_id": null}  -> 422 `video_cover_thumbnail_required`
/// {"title": null}               -> 忽略该字段（但算「非空更新」，200）
/// {"summary": null}             -> 忽略该字段
/// {"release_date": null}        -> **清空**发布日期
/// {}                            -> 422 `validation_error`
/// ```
///
/// 见 [`deserialize_double_option`] 与 `sm_service::videos::Field`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateRequest {
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub title: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub summary: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub release_date: Option<Option<chrono::NaiveDateTime>>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub cover_thumbnail_id: Option<Option<i32>>,
}

/// `GET ""` 的查询参数。
///
/// 上游四个都是**裸**参数（`query: str | None = Query(default=None)`、
/// `page: int = 1`、`page_size: int = 20`）—— **没有 `ge` / `le`**，
/// 上下界由 service 的 `validate_page` 兜，错误码是 `invalid_video_filter`。
#[derive(Debug, Deserialize)]
pub struct ListVideosQuery {
    /// 关键词。**给出但归一后为空 → 422**（见 `sm_service::videos::normalize_query`）。
    pub query: Option<String>,
    /// `field:direction`，白名单见 `sm_service::videos::ITEM_SORT_KEYS`。
    pub sort: Option<String>,
    #[serde(default = "one")]
    pub page: i64,
    #[serde(default = "twenty")]
    pub page_size: i64,
}

/// 每请求解析出的签名密钥（封面要签图片 URL）。
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

async fn list_videos(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListVideosQuery>,
) -> Result<Json<sm_core::pagination::Paginated<VideoItemListItemResource>>, ErrorResponse> {
    let secret = secret(&state)?;
    let service = VideoItemService::new(state.db());
    let (items, total) = service
        .list(
            query.query.as_deref(),
            query.sort.as_deref(),
            query.page,
            query.page_size,
        )
        .await?;
    // 同一批用**同一个** `now` 签名 —— 否则列表里几张图的有效期会差几毫秒。
    let now = now_seconds();
    let resources = items
        .iter()
        .map(|item| VideoItemListItemResource::from_list_item(&secret, now, item))
        .collect();
    Ok(Json(sm_core::pagination::Paginated::new(
        resources,
        query.page,
        query.page_size,
        total,
    )))
}

/// 三态搬运：`Option<Option<T>>` → [`Field<T>`]。
///
/// 与 `video_collections.rs` 的同名函数是一回事，只是这里的四个字段类型不同
/// （`String` / `NaiveDateTime` / `i32`），所以要泛型版。
fn to_field<T>(value: Option<Option<T>>) -> Field<T> {
    match value {
        None => Field::Absent,
        Some(None) => Field::Null,
        Some(Some(value)) => Field::Value(value),
    }
}

/// 服务层详情 → 响应资源。三条详情端点（`POST` / `GET` / `PATCH`）共用。
///
/// # `media_items[].play_url` 的 provider 查找：查不到就是**上游的 500**
///
/// 上游 `_media_items`（`video_item_service.py:309`）直接
/// `MEDIA_PROVIDER_REGISTRY.require(...)`，**没有** `try/except` —— 于是
/// `ProviderUnavailableError` 冒到 FastAPI 的兜底处理器，返回
/// **500 `internal_error`**（`api/exception/exception.py:63-73`）。
///
/// 这里照做：**不**把它降级成 503 或「没有 `play_url`」。理由与
/// `video_collections.rs` 那处**故意不同** —— 那边上游显式 `except` 并只把
/// `can_play` 打成 `false`，是**容错**；这边是上游漏了处理，而 `play_url` 是非空
/// 字段，硬给空串会把「插件没装」伪装成「这条媒体播不了」。
fn detail_resource(
    state: &AppState,
    detail: &VideoItemDetail,
) -> Result<VideoItemDetailResource, ErrorResponse> {
    let secret = secret(state)?;
    // 同一批条目用**同一个** `now` 签名（与列表同一理由）。
    let now = now_seconds();
    let deliveries = state.media_library_service().playback_deliveries();
    let mut media_items = Vec::with_capacity(detail.media_items.len());
    for item in &detail.media_items {
        // provider_key 为 `None`（孤儿媒体：库被删）在上游等价于
        // `require(None)`，同样是 `ProviderUnavailableError` —— 同一个 500。
        let declared = item
            .summary
            .provider_key
            .as_deref()
            .and_then(|key| deliveries.get(key))
            .cloned()
            .ok_or_else(|| {
                ErrorResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error",
                )
            })?;
        media_items.push(MovieMediaResource::from_media_item(
            item, &secret, now, &declared,
        ));
    }
    Ok(VideoItemDetailResource {
        list: VideoItemListItemResource::from_list_item(&secret, now, &detail.list),
        media_items,
    })
}

/// `POST ""` —— **201** + **详情**模型。
async fn create_video(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeJson(payload): EnvelopeJson<CreateRequest>,
) -> Result<(StatusCode, Json<VideoItemDetailResource>), ErrorResponse> {
    let service = VideoItemService::new(state.db());
    // 上游 `create_video` 结尾是 `get_video_detail(video.id)`（`:356-362`）——
    // 所以返回的是**详情**模型，不是刚插进去的那行。
    let created = service
        .create(&VideoItemCreate {
            title: payload.title,
            summary: payload.summary,
            release_date: payload.release_date,
        })
        .await?;
    let detail = service.detail(created.id).await?;
    Ok((StatusCode::CREATED, Json(detail_resource(&state, &detail)?)))
}

/// `GET /{id}` —— **详情**模型；不存在 → 404。
async fn get_video(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(video_id): Path<i32>,
) -> Result<Json<VideoItemDetailResource>, ErrorResponse> {
    let service = VideoItemService::new(state.db());
    let detail = service.detail(video_id).await?;
    Ok(Json(detail_resource(&state, &detail)?))
}

/// `PATCH /{id}` —— 部分更新，返回**详情**模型。
async fn update_video(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(video_id): Path<i32>,
    EnvelopeJson(payload): EnvelopeJson<UpdateRequest>,
) -> Result<Json<VideoItemDetailResource>, ErrorResponse> {
    let service = VideoItemService::new(state.db());
    service
        .update(
            video_id,
            VideoItemUpdate {
                title: to_field(payload.title),
                summary: to_field(payload.summary),
                release_date: to_field(payload.release_date),
                cover_thumbnail_id: to_field(payload.cover_thumbnail_id),
            },
        )
        .await?;
    let detail = service.detail(video_id).await?;
    Ok(Json(detail_resource(&state, &detail)?))
}

/// `DELETE /{id}` —— **204，无 body**。
///
/// # 这条路径上「删了什么」比 204 本身重要
///
/// 逐条媒体各走一遍 `MediaService::delete_media`（远端文件 + 缩略图 +
/// 向量）→ 删条目行 → 回收封面图。所以**没有插件时它是 503
/// `provider_not_installed`**，而不是「删了行、文件留着」—— 后者会让下次巡检
/// 把文件又扫回来，用户看到的是「删了又出现」。
///
/// `media` 这个参数不是可选的：`MediaService` 才拿得到 provider 网关（见
/// `AppState::media_service`）。
async fn delete_video(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(video_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    let media = state.media_service();
    VideoItemService::new(state.db())
        .delete(video_id, &media)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
