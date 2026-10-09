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

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/media-points", get(list_media_points))
        .route(
            "/media-points/{point_id}/collections",
            get(list_media_point_collections),
        )
        .route("/media-points/{point_id}", delete(delete_media_point))
}

/// 媒体点列表条目。
#[derive(Debug, Serialize)]
pub struct MediaPointListItem {
    pub id: i64,
    pub media_id: i64,
    pub kind: String,
    /// 时间轴位置（秒）。**不是所有 kind 都有** —— 章节有，帧可能没有。
    pub offset_seconds: Option<i64>,
    pub title: Option<String>,
}

/// 瞬时集合摘要。
///
/// 是 `MomentCollectionSummary` 而**不是** playlist 摘要 —— 上游两者是
/// 不同实体，混用会让客户端把片段集合当歌单用。
#[derive(Debug, Serialize)]
pub struct MomentCollectionSummaryResponse {
    pub id: i64,
    pub name: String,
    pub item_count: i64,
}

/// `GET /media-points` —— 分页查询。
///
/// **`page` / `page_size` 无边界**（见模块文档）。
#[derive(Debug, Default, Deserialize)]
pub struct ListMediaPointsQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
    /// 非法值**降级**（见 [`crate::query`]）。
    pub sort: Option<String>,
}

async fn list_media_points(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Query(_query): Query<ListMediaPointsQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接媒体点分页列表（page/page_size 不夹取）")
}

/// `GET /media-points/{point_id}/collections` —— 该点所属的瞬时集合。
///
/// 点不存在 → **404**。
async fn list_media_point_collections(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_point_id): Path<i64>,
) -> Result<Json<Vec<MomentCollectionSummaryResponse>>, ErrorResponse> {
    todo!("骨架：接媒体点的瞬时集合摘要（不是 playlist）")
}

/// `DELETE /media-points/{point_id}` —— **204，无 body**。
///
/// 与 `routes/media.rs` 里的 `DELETE /media/{media_id}/points/{point_id}`
/// **是两套独立寻址**，语义都是 204，别合并。
async fn delete_media_point(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_point_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接媒体点删除（成功返回 204 不带 body）")
}
