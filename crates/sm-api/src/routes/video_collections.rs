//! `/video-collections*` —— 视频集合九个端点。
//!
//! # 与上游 `src/api/routers/videos/collections.py` 的对应
//!
//! prefix **`/video-collections`** —— 注意**不含 `videos/` 这一段**，
//! 别照目录结构拼路径。
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET ""` | 200（裸列表）|
//! | `POST ""` | **201** |
//! | `GET /{id}` | 200 |
//! | `PATCH /{id}` | 200 |
//! | `DELETE /{id}` | **204** |
//! | `GET /{id}/items` | 200（`PageResponse`）|
//! | `POST /{id}/items` | **204** |
//! | `DELETE /{id}/items/{item_id}` | **204** |
//! | `POST /{id}/items/reorder` | **200 + 列表** |
//!
//! # 三处与 `moment_collections` 不同，别照抄错
//!
//! | | 本文件 | `routes/moment_collections.rs` |
//! |---|---|---|
//! | 添加成员 | **`POST`** `/items` → 204 | **`PUT`** `/points/{point_id}` → 204 |
//! | 整体重排 | **有** `POST /items/reorder` → 200 + 列表 | **无** |
//! | 成员 id | `item_id`（视频条目）| `point_id`（媒体点）|
//!
//! # `reorder` 是**唯一返回数据的写操作**
//!
//! 其余三个成员操作都返回 204 无 body，只有 `reorder` 返回 **200 + 完整新
//! 列表**。理由：重排是**整体替换语义** —— 客户端要确认服务端理解的顺序与
//! 自己的一致，所以回填权威顺序。**reorder 后不要再让客户端回查。**
//!
//! # `include_play_url` 默认 `false`
//!
//! 生成播放 URL 要走签名流程（见 `routes/media_playback.rs`），逐条生成不
//! 便宜。别「优化」成总是返回。

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
            "/video-collections",
            get(list_collections)
                .post(create_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}",
            get(get_collection)
                .patch(update_collection)
                .delete(delete_collection)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items",
            get(list_collection_items)
                .post(add_collection_item)
                .fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items/reorder",
            post(reorder_collection_items).fallback(method_not_allowed),
        )
        .route(
            "/video-collections/{collection_id}/items/{item_id}",
            delete(remove_collection_item).fallback(method_not_allowed),
        )
}

/// 视频集合。
#[derive(Debug, Clone, Serialize)]
pub struct VideoCollectionResource {
    pub id: i64,
    pub name: String,
    pub item_count: i64,
}

/// 集合条目。
#[derive(Debug, Clone, Serialize)]
pub struct VideoCollectionItemResource {
    pub item_id: i64,
    pub title: Option<String>,
    /// 仅当请求带 `include_play_url=true` 时出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_url: Option<String>,
    pub duration_seconds: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateRequest {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateRequest {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemAddRequest {
    pub item_id: i64,
}

/// 重排请求。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReorderRequest {
    /// 目标顺序，**元素是 item_id**。
    pub item_ids: Vec<i64>,
}

/// `GET ""`
async fn list_collections(
    _user: CurrentUser,
    State(_state): State<AppState>,
) -> Result<Json<Vec<VideoCollectionResource>>, ErrorResponse> {
    todo!("骨架：接视频集合列表")
}

/// `POST ""` —— **201**。重名 → **409**。
async fn create_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<CreateRequest>,
) -> Result<(StatusCode, Json<VideoCollectionResource>), ErrorResponse> {
    todo!("骨架：接创建（201；重名 -> 409）")
}

/// `GET /{id}` —— 404。
async fn get_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
) -> Result<Json<VideoCollectionResource>, ErrorResponse> {
    todo!("骨架：接单个查询")
}

/// `PATCH /{id}` —— 部分更新。
async fn update_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<UpdateRequest>,
) -> Result<Json<VideoCollectionResource>, ErrorResponse> {
    todo!("骨架：接部分更新")
}

/// `DELETE /{id}` —— **204，无 body**。
async fn delete_collection(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接删除（204 无 body）")
}

/// `GET /{id}/items` —— 分页 + `include_play_url`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListItemsQuery {
    pub sort: Option<String>,
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub page_size: Option<i64>,
    /// **默认 `false`**。
    #[serde(default)]
    pub include_play_url: bool,
}

async fn list_collection_items(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Query(_query): axum::extract::Query<ListItemsQuery>,
) -> Result<Json<serde_json::Value>, ErrorResponse> {
    todo!("骨架：接条目分页列表；include_play_url 默认 false")
}

/// `POST /{id}/items` —— **204，无 body**。**是 `POST` 不是 `PUT`**。
/// 已存在 → **409**。
async fn add_collection_item(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<ItemAddRequest>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接添加条目（POST + 204 无 body；已存在 -> 409）")
}

/// `DELETE /{id}/items/{item_id}` —— **204，无 body**，**幂等**。
async fn remove_collection_item(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path((_collection_id, _item_id)): Path<(i64, i64)>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接移除条目（幂等；204 无 body）")
}

/// `POST /{id}/items/reorder` —— **200 + 完整新列表**（唯一返回数据的写操作）。
///
/// 响应里**不含** `play_url` —— reorder 请求没有 `include_play_url`。
async fn reorder_collection_items(
    _user: CurrentUser,
    State(_state): State<AppState>,
    Path(_collection_id): Path<i64>,
    axum::extract::Json(_payload): axum::extract::Json<ReorderRequest>,
) -> Result<Json<Vec<VideoCollectionItemResource>>, ErrorResponse> {
    todo!("骨架：接重排；校验全做完再动数据；返回重排后的权威顺序")
}