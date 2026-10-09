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
//! **不要**用同一个类型。列表项不该带完整详情字段（描述、完整元数据），
//! 否则 20 条一页会显著变大。
//!
//! 写 / `PATCH` 返回的是**详情**模型（不是列表模型）—— 所以「改完拿到的
//! 对象」与「列表里那个对象」字段不同。这是刻意的，别统一。
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
use serde::{Deserialize, Serialize};

use sm_service::videos::VideoItemService;

use crate::auth::CurrentUser;
use crate::dto::{deserialize_double_option, VideoItemListItemResource};
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

/// 详情 —— = 列表项 + `media_items`。
///
/// ⚠️ **本轮未接线，且这个形状本身就是错的**：骨架期写成了
/// `{ id, title, duration_seconds, thumbnail_url, description, metadata }`，
/// 而上游 `VideoItemDetailResource` 是
/// **`VideoItemListItemResource` + `media_items: list[MovieMediaResource]`**
/// —— `description` / `metadata` / `thumbnail_url` 三个键前端一个都不读，
/// 真正的 14 个字段反而一个没有。
///
/// **不接线的原因是缺依赖，不是缺代码**：`media_items[].play_url` 要
/// `MEDIA_PROVIDER_REGISTRY.require(provider_key)` 拿 `playback_deliveries[0]`
/// 才能签出播放地址，而插件 ABI 还没落地。**不要用空串冒充** —— 客户端会把
/// 空地址当成「不可播放」而禁用播放，那是**可见的功能回退**。
///
/// 接线时改成内嵌 [`VideoItemListItemResource`] +
/// `media_items`（`MovieMediaResource` 也还没在 Rust 侧建模）。
#[derive(Debug, Clone, Serialize)]
pub struct VideoItemDetail {
    pub id: i64,
    pub title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub thumbnail_url: Option<String>,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
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

/// `POST ""` —— **201** + **详情**模型。
async fn create_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    EnvelopeJson(_payload): EnvelopeJson<CreateRequest>,
) -> Result<(StatusCode, Json<VideoItemDetail>), ErrorResponse> {
    todo!("骨架：接创建（201；返回详情模型 —— 卡在 media_items 的签名播放地址要插件 ABI）")
}

/// `GET /{id}` —— **详情**模型；不存在 → 404。
async fn get_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i32>,
) -> Result<Json<VideoItemDetail>, ErrorResponse> {
    todo!("骨架：接详情查询 —— 卡在 media_items 的签名播放地址要插件 ABI")
}

/// `PATCH /{id}` —— 部分更新，返回**详情**模型。
async fn update_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i32>,
    EnvelopeJson(_payload): EnvelopeJson<UpdateRequest>,
) -> Result<Json<VideoItemDetail>, ErrorResponse> {
    todo!("骨架：接部分更新 —— 卡在 media_items 的签名播放地址要插件 ABI")
}

/// `DELETE /{id}` —— **204，无 body**。
async fn delete_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i32>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接删除 —— 上游还要走 MediaService.delete_media 清磁盘文件，同样卡在插件")
}
