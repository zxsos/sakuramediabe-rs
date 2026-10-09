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
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/videos",
            get(list_videos).post(create_video).fallback(method_not_allowed),
        )
        .route(
            "/videos/{video_id}",
            get(get_video)
                .patch(update_video)
                .delete(delete_video)
                .fallback(method_not_allowed),
        )
}

/// 列表项 —— **比详情少得多**。
#[derive(Debug, Clone, Serialize)]
pub struct VideoItemListItem {
    pub id: i64,
    pub title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub thumbnail_url: Option<String>,
}

/// 详情 —— 字段比列表项多。
#[derive(Debug, Clone, Serialize)]
pub struct VideoItemDetail {
    pub id: i64,
    pub title: Option<String>,
    pub duration_seconds: Option<i64>,
    pub thumbnail_url: Option<String>,
    /// 描述等长文本。**只在详情里出现。**
    pub description: Option<String>,
    /// 完整元数据。
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateRequest {
    pub title: Option<String>,
    /// 源地址。创建时必填，**但不要建模成必填字段** —— 上游可能有其他来源
    /// 形态，校验放在 service 层更合适。
    pub source_uri: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateRequest {
    pub title: Option<String>,
    pub description: Option<String>,
}

/// `GET ""` —— 分页。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListVideosQuery {
    /// 关键词。
    pub query: Option<String>,
    pub sort: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
}

async fn list_videos(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Query(_query): axum::extract::Query<ListVideosQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接视频分页列表（列表项模型，不带 description）")
}

/// `POST ""` —— **201** + **详情**模型。
async fn create_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<CreateRequest>,
) -> Result<(StatusCode, Json<VideoItemDetail>), ErrorResponse> {
    todo!("骨架：接创建（201；返回详情模型）")
}

/// `GET /{id}` —— **详情**模型；不存在 → 404。
async fn get_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i64>,
) -> Result<Json<VideoItemDetail>, ErrorResponse> {
    todo!("骨架：接详情查询")
}

/// `PATCH /{id}` —— 部分更新，返回**详情**模型。
async fn update_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<UpdateRequest>,
) -> Result<Json<VideoItemDetail>, ErrorResponse> {
    todo!("骨架：接部分更新（返回详情模型，非列表模型）")
}

/// `DELETE /{id}` —— **204，无 body**。
async fn delete_video(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_video_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接删除（204 无 body）")
}