//! `/media-points*` —— 媒体点三个端点。
//!
//! # 与上游 `src/api/routers/playback/media_points.py` 的对应
//!
//! 上游这个 router **没有 prefix**，路径自带 `/media-points`。
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `DELETE /media-points/{point_id}` | **204** |
//! | `GET /media-points` | 200 |
//! | `GET /media-points/{point_id}/collections` | 200 |
//!
//! # 路径前缀与 `routes/media.rs` 刻意不同
//!
//! | | 前缀 | 语义 |
//! |---|---|---|
//! | 本文件 | `/media-points/{point_id}` | **按点 id** 寻址（跨媒体） |
//! | `routes/media.rs` | `/media/{media_id}/points` | **按媒体 id** 寻址（该媒体下的点） |
//!
//! **两种寻址都存在，且都能删点** —— 前者 204，后者也 204。**不是重复**：
//! 一个是「这个点在哪个媒体下不知道，但我知道点 id」，另一个是「明确知道
//! 媒体，要删它的某个点」。合并成一个会让其中一种用法消失。
//!
//! # `list_media_points` 的分页**没有边界**
//!
//! 上游 `page: int = Query(default=1)`、`page_size: int = Query(default=20)` ——
//! **无 `ge` / `le`**。所以 `page_size=100000` 合法。照抄，不夹取。
//!
//! # `collections` 端点返回的是**瞬时集合**摘要
//!
//! `list[MomentCollectionSummary]` —— 注意是 `MomentCollection`（瞬时片段
//! 集合）而不是 `playlists`。这是个容易搞混的命名：上游的
//! `moment_collections` 与 `playlists` 是**两个不同的东西**，
//! 而它们的端点都在 `collections` router 下。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;

use crate::auth::CurrentUser;
use crate::dto::MediaPointListItemResource;
use crate::error::ErrorResponse;
// ⚠️ 用包装过的 `Query`：axum 原生的 rejection 是 400 + 纯文本，而项目契约是
// 422 + 错误信封（见 `crate::extract` 的说明）。
use crate::extract::Query as EnvelopeQuery;
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

use sm_core::pagination::Paginated;
use sm_service::collections::{MomentCollectionService, MomentCollectionSummary};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/media-points", get(list_media_points))
        .route(
            "/media-points/{point_id}/collections",
            get(list_media_point_collections),
        )
        .route("/media-points/{point_id}", delete(delete_media_point))
}

/// `GET /media-points` 的查询参数 —— 上游 `media_points.py:25-38`。
///
/// ⚠️ 骨架期这里写着「只有 `page` / `page_size` / `sort` —— **少了三个**上游
/// 有的参数」。**现在不缺了**：`kind`（默认 `jav`）/ `keyword` /
/// `exclude_collection_id`（`ge=1`）都在下面，服务层
/// [`sm_service::playback::media::MediaService::list_media_points`] 与仓储层
/// `MediaPointRepository::{list,count}_filtered` 也都在用它们。
///
/// 留着这段只为说明**当初漏参数的后果**：客户端按上游发 `?kind=video` 会被
/// 静默忽略，「筛选视频」的结果里混着 JAV —— 补参数**不是**「功能少一点」。
///
/// `sort` 的形状是 `field:direction`（只认 `created_at:asc` / `created_at:desc`），
/// 非法值 **422**（不降级）—— 见服务层那张校验表。
#[derive(Debug, Default, Deserialize)]
pub struct ListMediaPointsQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
    pub sort: Option<String>,
    /// 媒体点类型；缺省 = `jav`（与上游 `kind: MediaPointKind = Query(default=JAV)` 一致）。
    pub kind: Option<String>,
    /// 关键词：JAV 匹配番号，video 匹配标题。
    pub keyword: Option<String>,
    /// 排除「已在这个合集里」的点。上游声明 `ge=1`。
    pub exclude_collection_id: Option<i64>,
}

async fn list_media_points(
    State(state): State<AppState>,
    _user: CurrentUser,
    EnvelopeQuery(query): EnvelopeQuery<ListMediaPointsQuery>,
) -> Result<Json<Paginated<MediaPointListItemResource>>, ErrorResponse> {
    let invalid = |field: &str, value: &str| {
        ErrorResponse::from(sm_service::error::ServiceError::validation(
            "validation_error",
            format!("{field} 取值非法：{value}"),
        ))
    };
    // `kind` 是枚举（jav / video / all），非法取值由上游的 FastAPI 枚举校验拦成
    // 422 —— 服务层只认 jav/video，别的一律当「不过滤」，所以必须在这里拦。
    if let Some(kind) = query.kind.as_deref() {
        if !matches!(kind, "jav" | "video" | "all") {
            return Err(invalid("kind", kind));
        }
    }
    // 上游 `exclude_collection_id: int | None = Query(default=None, ge=1)`。
    if let Some(collection_id) = query.exclude_collection_id {
        if collection_id < 1 {
            return Err(invalid("exclude_collection_id", &collection_id.to_string()));
        }
    }

    let page = query.page.unwrap_or(1);
    let page_size = query.page_size.unwrap_or(20);
    let secret = signing_secret(&state)?;
    // 一批用同一个时间戳（见 [`crate::signing`]）。
    let now = now_seconds();
    let value = state
        .media_service()
        .list_media_points(
            page,
            page_size,
            query.sort.as_deref(),
            query.kind.as_deref(),
            query.keyword.as_deref(),
            query.exclude_collection_id,
        )
        .await?;
    // ★ 服务层给的是**未签名**的 `image_origin`，这里必须换成签名 URL —— 否则
    // 客户端拿着裸路径去取图会被 403（这正是 [`MediaPointListItemResource`]
    // 存在的理由）。
    let items: Vec<MediaPointListItemResource> = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| MediaPointListItemResource::from_list_item(&secret, now, row))
                .collect()
        })
        .unwrap_or_default();
    let total = value
        .get("total")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    Ok(Json(Paginated::new(items, page, page_size, total)))
}

/// `GET /media-points/{point_id}/collections` —— 该点所属的瞬时集合。
///
/// 点不存在 → **404** `media_point_not_found`（详情键 `point_id`）。
async fn list_media_point_collections(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(point_id): Path<i64>,
) -> Result<Json<Vec<MomentCollectionSummary>>, ErrorResponse> {
    Ok(Json(
        MomentCollectionService::new(state.db())
            .list_point_collections(narrow_point_id_for_collections(point_id)?)
            .await?,
    ))
}

/// `DELETE /media-points/{point_id}` —— **204，无 body**。
///
/// 与 `routes/media.rs` 里的 `DELETE /media/{media_id}/points/{point_id}`
/// **是两套独立寻址**，语义都是 204，别合并。
async fn delete_media_point(
    State(state): State<AppState>,
    _user: CurrentUser,
    Path(point_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    state
        .media_service()
        .delete_point_by_id(super::media::narrow_point_id(point_id)?)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `i64` 路径参数 → `i32`（库列是 `i32`），溢出 → 404。
///
/// 与 `super::media::narrow_point_id` 的唯一区别是**详情键**：本文件的两个
/// 端点走的是上游 `MomentCollectionService._require_point`，它的
/// `error_details_key` 是 `point_id`；而删除走的是 `MediaService` 的
/// `require_by_id(MediaPoint, ...)`，详情键是默认的 `media_point_id`。
/// 两个键**确实不同**，不是笔误。
fn narrow_point_id_for_collections(point_id: i64) -> Result<i32, ErrorResponse> {
    i32::try_from(point_id).map_err(|_| {
        let mut details = serde_json::Map::new();
        details.insert("point_id".to_owned(), serde_json::Value::from(point_id));
        ErrorResponse::from(sm_service::error::ServiceError::not_found_with(
            "media_point_not_found",
            "Media point not found",
            details,
        ))
    })
}
