//! 路由模块。

use axum::http::StatusCode;

use crate::error::ErrorResponse;

pub mod auth;
pub mod playlists;

/// 路径命中但方法不匹配 → 405 `http_error`。
///
/// axum 默认返回 405 + **空响应体**，且不经过 router 的 fallback，客户端
/// 会拿到"状态码对、body 解析失败"。每个 `MethodRouter` 都要显式挂它。
pub async fn method_not_allowed() -> ErrorResponse {
    ErrorResponse::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "http_error",
        "Method Not Allowed",
    )
}
