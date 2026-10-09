//! `/moment-collections*` —— 瞬时集合九个端点。
//!
//! # 与上游 `src/api/routers/collections/moment_collections.py` 的对应
//!
//! prefix `/moment-collections`。
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET ""` | 200（`list`，**不分页**）|
//! | `POST ""` | **201** |
//! | `GET /{id}` | 200 |
//! | `PATCH /{id}` | 200 |
//! | `DELETE /{id}` | **204** |
//! | `GET /{id}/points` | 200（`PageResponse`，**分页**）|
//! | `PUT /{id}/points/{point_id}` | **204** |
//! | `DELETE /{id}/points/{point_id}` | **204** |
//! | `PUT /{id}/points` | **204** |
//!
//! # 四个点位操作**全部 204 且无 body** —— 包括「添加」这个非幂等动作
//!
//! `PUT /{id}/points/{point_id}` 是「添加一个点」，语义上**不是**幂等的
//! （加两次理应报错或加两条），但它返回 **204 无 body** —— 与「设置整个列表」
//! 和「移除」**完全一样**。
//!
//! 所以**不要**给「添加」加一个返回新增成员的 JSON body：客户端拿不到
//! `point_id` 就得回查一次列表。照抄。
//!
//! # 列表**不分页**，点位列表**分页**
//!
//! `GET ""` 返回 `list[MomentCollectionResource]`（裸列表），
//! `GET /{id}/points` 返回 `PageResponse[...]`。
//!
//! 同一资源组里两种分页形态。集合数量天然少（用户手建），点位数量多
//! （一个集合可挂很多片段）—— **这个区分有道理，不要「统一」**。
//!
//! # 与 `routes/playlists.rs` 的对比
//!
//! `playlists`（歌单）与 `moment_collections`（瞬时片段集合）是**两个不同
//! 实体**，形状几乎一样（CRUD + 成员管理），容易混。判别：歌单里放**音频
//! 文件**，瞬时集合放**媒体点**（视频片段），数据库表也不同。
//! **不要**因为形状像就复用同一个路由文件。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/moment-collections",
            get(list_moment_collections)
                .post(create_moment_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}",
            get(get_moment_collection)
                .patch(update_moment_collection)
                .delete(delete_moment_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}/points",
            get(list_moment_collection_points)
                .put(set_moment_collection_points)
                .fallback(method_not_allowed),
        )
        .route(
            "/moment-collections/{collection_id}/points/{point_id}",
            put(add_point_to_moment_collection)
                .delete(remove_point_from_moment_collection)
                .fallback(method_not_allowed),
        )
}

/// 瞬时集合。
#[derive(Debug, Clone, Serialize)]
pub struct MomentCollectionResource {
    pub id: i64,
    pub name: String,
    /// 成员数量。**冗余字段** —— 列表接口靠它免掉 N+1 查询。
    pub item_count: i64,
    pub created_at: Option<String>,
}

/// 创建请求。
#[derive(Debug, Clone, Deserialize)]
pub struct MomentCollectionCreateRequest {
    pub name: String,
}

/// 更新请求 —— 部分更新，字段全 `Option`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MomentCollectionUpdateRequest {
    pub name: Option<String>,
}

/// 设置整个点位列表的请求。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MomentCollectionSetPointsRequest {
    /// 目标点位 id 列表，**顺序即播放顺序**。
    pub point_ids: Vec<i64>,
}

/// 集合内的一个点位。
#[derive(Debug, Clone, Serialize)]
pub struct MomentCollectionPointItem {
    pub point_id: i64,
    pub media_id: i64,
    pub kind: String,
    pub offset_seconds: Option<i64>,
    pub thumbnail_url: Option<String>,
}
/// `GET ""` —— **裸列表，不分页**。
async fn list_moment_collections(
    _user: CurrentUser,
    State(_state): State<AppState>,
) -> Result<Json<Vec<MomentCollectionResource>>, ErrorResponse> {
    todo!("骨架：接瞬时集合列表（裸列表，带 item_count）")
}

/// `POST ""` —— **201 Created**。重名 → **409**。
async fn create_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<MomentCollectionCreateRequest>,
) -> Result<(StatusCode, Json<MomentCollectionResource>), ErrorResponse> {
    todo!("骨架：接创建（重名 -> 409）")
}

/// `GET /{collection_id}` —— 不存在 → **404**。
async fn get_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
) -> Result<Json<MomentCollectionResource>, ErrorResponse> {
    todo!("骨架：接单个查询（404）")
}

/// `PATCH /{collection_id}` —— 部分更新。
async fn update_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<MomentCollectionUpdateRequest>,
) -> Result<Json<MomentCollectionResource>, ErrorResponse> {
    todo!("骨架：接部分更新")
}

/// `DELETE /{collection_id}` —— **204，无 body**。
async fn delete_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接删除（204 无 body）")
}

/// `GET /{collection_id}/points` —— **分页**（与上面的裸列表不同）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListPointsQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
}

async fn list_moment_collection_points(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Query(_query): axum::extract::Query<ListPointsQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接点位分页列表（分页，而集合列表是裸列表）")
}

/// `PUT /{collection_id}/points/{point_id}` —— **204，无 body**。
///
/// 点不存在 → 404；**已存在 → 409**（不是 204，要能区分）。
/// 「添加」不返回新增成员（见模块文档）。
async fn add_point_to_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_collection_id, _point_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接添加点位（已存在 -> 409；成功 204 无 body）")
}

/// `DELETE /{collection_id}/points/{point_id}` —— **204，无 body**。
///
/// **幂等**：该点不在集合里时**仍然 204**。与上面「添加」的 409 形成对照 ——
/// 同一个资源组里两个方向错误码不同，这是有意的。
async fn remove_point_from_moment_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_collection_id, _point_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接移除点位（幂等；成功 204 无 body）")
}

/// `PUT /{collection_id}/points` —— **整体替换，204，无 body**。
///
/// 覆盖式：先清空该集合的点位再按 `point_ids` 写入。**校验必须全部做完再动
/// 数据** —— 同一原则见 `routes/clip_collections.rs` 里那个测试
/// `set_clips_validates_everything_before_touching_anything`。
async fn set_moment_collection_points(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<MomentCollectionSetPointsRequest>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接整体替换（先全量校验再动数据；成功 204 无 body）")
}