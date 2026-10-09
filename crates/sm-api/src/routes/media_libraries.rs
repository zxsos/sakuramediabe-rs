//! `/media-libraries*` —— 媒体库 CRUD 五个端点。
//!
//! # 与上游 `src/api/routers/playback/media_libraries.py` 的对应
//!
//! | 上游端点 | 状态码 |
//! |---|---|
//! | `GET /media-libraries` | 200 |
//! | `GET /media-libraries/providers` | 200 |
//! | `POST /media-libraries` | **201** |
//! | `PATCH /media-libraries/{library_id}` | 200 |
//! | `DELETE /media-libraries/{library_id}` | **204** |
//!
//! # 用 `PATCH` 而不是 `PUT` —— 语义是「部分更新」
//!
//! 上游是 `PATCH /{library_id}`。所以 `MediaLibraryUpdateRequest` 的字段
//! **全部是 `Option`**，且**不传 = 不改**（不是置空）。
//!
//! 这里有个容易写错的点：`Option<T>` 在 serde 里的默认语义是
//! **`None` 与「显式 `null`」不分** —— 两者都会得到 `None`。所以
//! 「把某字段清成 null」和「不改这个字段」在 PATCH 里**无法区分**。
//!
//! 上游也有同样的问题（Pydantic 的 `exclude_unset` 能区分，但要走
//! `model_fields_set`）。**照抄上游的宽松行为，并在字段文档里标注** ——
//! 擅自改成 `Option<Option<T>>` 会让「清空字段」这个操作变得可达，
//! 而那需要配套的 DB 语义，不该在路由层单方面开。
//!
//! # `POST` 返回 **201** 且带 body，`DELETE` 返回 **204** 不带 body
//!
//! 这两个状态码是契约的一部分：201 让客户端知道「已创建」并可以直接用
//! 返回的 id；204 表示删除成功且**无正文**。返回 `Json<()>` 会产出
//! `null` 正文，与 204 冲突。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media-libraries",
            get(list_media_libraries).post(create_media_library),
        )
        .route(
            "/media-libraries/providers",
            get(list_media_library_providers),
        )
        .route(
            "/media-libraries/{library_id}",
            patch(update_media_library).delete(delete_media_library),
        )
}

/// 媒体库条目。
#[derive(Debug, Serialize)]
pub struct MediaLibraryResponse {
    pub id: i64,
    pub name: String,
    pub provider: String,
    /// provider 侧的库标识（路径 / remote id 等），**由插件解释**。
    pub handle: String,
    pub enabled: bool,
}

/// 媒体库 provider 条目。
#[derive(Debug, Serialize)]
pub struct MediaLibraryProviderResponse {
    pub provider: String,
    pub display_name: String,
    /// 该 provider 支持的库类型。
    pub kinds: Vec<String>,
}

/// `GET /media-libraries`
async fn list_media_libraries(
    State(_state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<MediaLibraryResponse>>, ErrorResponse> {
    todo!("骨架：接媒体库列表")
}

/// `GET /media-libraries/providers`
///
/// 列出**可用的 provider 类型**。这份数据来自**插件注册表**（`sm-plugins`），
/// 不是数据库 —— 没有插件时返回空列表而**不是 503**。
async fn list_media_library_providers(
    State(_state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<MediaLibraryProviderResponse>>, ErrorResponse> {
    todo!("骨架：接插件注册表（无插件时返回空列表，不 503）")
}

/// `POST /media-libraries` —— **201 Created**。
async fn create_media_library(
    State(_state): State<AppState>,
    _user: CurrentUser,
    _payload: axum::extract::Json<serde_json::Value>,
) -> Result<(StatusCode, Json<MediaLibraryResponse>), ErrorResponse> {
    todo!("骨架：接媒体库创建；成功返回 201 + body")
}

/// `PATCH /media-libraries/{library_id}` —— 部分更新。
///
/// 库不存在 → **404**。
async fn update_media_library(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_library_id): Path<i64>,
    _payload: axum::extract::Json<serde_json::Value>,
) -> Result<Json<MediaLibraryResponse>, ErrorResponse> {
    todo!("骨架：接媒体库部分更新（不传 = 不改，见模块文档）")
}

/// `DELETE /media-libraries/{library_id}` —— **204，无 body**。
///
/// 库不存在时**仍然 204**：删除是幂等的语义。库非空时**不**级联删媒体 ——
/// 那个决定要看上游怎么做，实现时照上游。
async fn delete_media_library(
    State(_state): State<AppState>,
    _user: CurrentUser,
    Path(_library_id): Path<i64>,
) -> Result<StatusCode, ErrorResponse> {
    todo!("骨架：接媒体库删除（幂等；成功返回 204 不带 body）")
}
