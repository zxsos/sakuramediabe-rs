//! `/media-transfers*` —— 存储转存两个端点。
//!
//! # 与上游 `src/api/routers/transfers/media_transfer.py` 的对应
//!
//! | 上游端点 | 状态码 | 作用 |
//! |---|---|---|
//! | `POST /media-transfers/candidates` | 200 | 列出可转存的目标 |
//! | `POST /media-transfers` | **202** | 发起转存 |
//!
//! 两个都是 `POST` 但**第一个是查询**（用 `POST` 是因为请求体复杂，不是
//! 因为有副作用）。所以 200 / 202 的分界是「有没有后台工作在跑」，
//! **不是「方法是不是 POST」** —— 与 `media_import.rs` 里同一条分界一致。
//!
//! # 与 `download-requests` 的区别：转存不需要下载器
//!
//! `POST /download-requests` 是「从索引器下种子到下载器」，要 provider 插件。
//! 本文件是「在媒体库之间搬文件」，只依赖**媒体库句柄**（`library_handle`，
//! 同样是插件提供）。
//!
//! 两个都依赖插件，但依赖的**能力不同** —— 移植时要分开确认，别以为接了一个
//! 另一个就通了。
//!
//! # `candidates` 用 POST 的代价
//!
//! 因为走 `POST /media-transfers/candidates`，浏览器与 CDN **不会缓存**它。
//! 这是上游的选择，照抄。**不要**「优化」成 `GET` 加 query 字符串 —— 那会让
//! 大批量目标列表撞上 URL 长度限制。

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::CurrentUser;
use crate::error::ErrorResponse;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/media-transfers/candidates",
            post(list_media_transfer_candidates).fallback(method_not_allowed),
        )
        .route(
            "/media-transfers",
            post(create_media_transfer).fallback(method_not_allowed),
        )
}

/// 可转存目标。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferTarget {
    pub library_id: i64,
    pub library_name: String,
    /// 目标路径（库内相对路径）。
    pub path: String,
    /// 该目标是否可写。**不可写的也要列出来** —— 客户端要显示「为什么不行」。
    pub writable: bool,
    /// 不可写的原因。
    pub blocked_reason: Option<String>,
}

/// 候选查询响应。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferCandidatesResponse {
    pub media_id: i64,
    pub targets: Vec<MediaStorageTransferTarget>,
}

/// 候选查询请求。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaStorageTransferCandidatesRequest {
    pub media_id: i64,
    /// 限定只返回这些库；`None` = 全部库。
    pub library_ids: Option<Vec<i64>>,
}

/// 已受理的转存。
#[derive(Debug, Clone, Serialize)]
pub struct MediaStorageTransferAcceptedResponse {
    pub task_run_id: i64,
    pub accepted: i32,
}

/// `POST /media-transfers/candidates` —— **200**（查询）。
///
/// 媒体不存在 → **404**。目标库不存在 → 同样 404（不是空列表）——
/// 传了 `library_ids` 却有一个不存在，说明客户端状态与服务端不一致。
async fn list_media_transfer_candidates(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(payload): axum::extract::Json<MediaStorageTransferCandidatesRequest>,
) -> Result<Json<MediaStorageTransferCandidatesResponse>, ErrorResponse> {
    let _ = payload.media_id;
    todo!("骨架：接媒体库句柄（插件）；不可写目标也要列出并带原因")
}

/// `POST /media-transfers` —— **202 Accepted**（长任务）。
async fn create_media_transfer(
    _user: CurrentUser,
    State(_state): State<AppState>,
    axum::extract::Json(_payload): axum::extract::Json<serde_json::Value>,
) -> Result<(StatusCode, Json<MediaStorageTransferAcceptedResponse>), ErrorResponse> {
    todo!("骨架：接转存流水线；成功返回 202 + task_run_id")
}