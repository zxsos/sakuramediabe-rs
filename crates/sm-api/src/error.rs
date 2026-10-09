//! 错误信封 → HTTP 响应。
//!
//! # 这张表必须和上游逐条对齐
//!
//! `src/api/exception/exception.py:29-73` 的转换规则：
//!
//! | 来源 | status | code | message |
//! |---|---|---|---|
//! | `ApiError` | 原码 | 原 code | 原 message |
//! | HTTP 401/403 | 原码 | `unauthorized` / `forbidden` | — |
//! | 其他 HTTPException | 原码 | **`http_error`** | Starlette 文案 |
//! | `RequestValidationError` | 422 | `validation_error` | — |
//! | 兜底 `Exception` | 500 | `internal_error` | — |
//!
//! 客户端按 `code` 分支、按状态码决定是否重试，两边错了任何一边都会让
//! 「该重试的没重试」或「不该重试的一直重试」。
//!
//! # 已刻意留的一处缺口（不要当成已完成）
//!
//! **`details` 的形状**：上游 `RequestValidationError` 的 details 是
//! `{"detail": [...], "body": ...}`，这里只给了 `{"detail": <文本>}`。
//!
//! # 曾经是缺口、现已闭合的一处
//!
//! **405**：axum 的方法不匹配**不经过** router 的 fallback，所以每个
//! `MethodRouter` 都必须显式挂 `.fallback(method_not_allowed)`，否则客户端
//! 拿到的是「405 + 空响应体」—— 状态码对、body 解析失败。
//!
//! 挂对了不等于不会退化：新增路由时漏挂就是静默的契约破坏，而这种破坏在
//! 客户端才显形。所以 `tests/method_not_allowed_http.rs` 对**每一条**已注册
//! 路由逐个发方法不匹配的请求并断言信封形状。

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use sm_core::auth::AuthFailure;
use sm_core::signing::SignatureError;
use sm_core::{ApiError, ErrorEnvelope};
use sm_service::error::ServiceError;

/// HTTP 状态码 + 响应体的 `error` 对象。
///
/// 与 [`ServiceError`] 的区别只是**这一层知道怎么变成 HTTP 响应**，
/// 业务语义（状态码与错误码的绑定）仍然由 service 层决定。
///
/// `error` 装箱的理由与 [`ServiceError`] 一致（clippy 的
/// `result_large_err`）：不装箱时 `Err` 变体 128 字节，每个 handler 的
/// `?` 都在搬它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorResponse {
    pub status: StatusCode,
    pub error: Box<ApiError>,
}

impl ErrorResponse {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            error: Box::new(ApiError::new(code, message)),
        }
    }

    pub fn with_details(mut self, details: serde_json::Map<String, serde_json::Value>) -> Self {
        self.error.details = Some(details);
        self
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorEnvelope::new(*self.error))).into_response()
    }
}

impl From<ServiceError> for ErrorResponse {
    /// 状态码与错误码一起搬过来 —— service 层已经把它们绑好了。
    /// `api` 本来就是 `Box`，这里直接转移所有权，不再重新装箱。
    fn from(value: ServiceError) -> Self {
        let status =
            StatusCode::from_u16(value.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self {
            status,
            error: value.api,
        }
    }
}

impl From<AuthFailure> for ErrorResponse {
    /// 两种情况都是 401 `unauthorized`，只有 message 不同。
    fn from(value: AuthFailure) -> Self {
        Self::new(
            StatusCode::from_u16(AuthFailure::STATUS).unwrap_or(StatusCode::UNAUTHORIZED),
            AuthFailure::ERROR_CODE,
            value.message(),
        )
    }
}

impl From<SignatureError> for ErrorResponse {
    /// 签名 URL 的三种失败都是 403，只有错误码不同。
    fn from(value: SignatureError) -> Self {
        Self::new(
            StatusCode::from_u16(SignatureError::STATUS).unwrap_or(StatusCode::FORBIDDEN),
            value.code(),
            value.message(),
        )
    }
}

impl From<JsonRejection> for ErrorResponse {
    /// 请求体解析失败 → 422 `validation_error`，与上游 `RequestValidationError` 对应。
    fn from(value: JsonRejection) -> Self {
        let mut details = serde_json::Map::new();
        details.insert(
            "detail".to_owned(),
            serde_json::Value::from(value.body_text()),
        );
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        )
        .with_details(details)
    }
}

/// 查询串解析失败 → 422 `validation_error`。
///
/// 与 [`JsonRejection`] 同一个理由：axum 的 `QueryRejection` 默认响应是
/// **400 + 纯文本**，不经过错误信封。上游 FastAPI 走
/// `RequestValidationError`，是 422 + 信封。两者状态码与响应体形状
/// 都不同，而客户端是按 `code` 分支的。
///
/// `details.detail` 带 axum 的原始描述（哪个键、什么值、为什么不行），
/// 与 `JsonRejection` 的做法一致 —— 那是定位「客户端拼错了哪个参数」
/// 的唯一线索。
///
/// # 为什么有两个 `from` 实现
///
/// 本仓有两个查询提取器：[`crate::extract::Query`]（`serde_urlencoded`）与
/// [`crate::extract::HtmlFormQuery`]（`serde_html_form`，支持重复键）。它们
/// 失败时的 **rejection 是不同类型**，但语义完全一样。映射只留这一份，
/// 两个 `From` 都转发过来 —— 免得日后改文案只改一边。
fn query_rejection(detail: String) -> ErrorResponse {
    let mut details = serde_json::Map::new();
    details.insert("detail".to_owned(), serde_json::Value::from(detail));
    ErrorResponse::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation_error",
        "Request validation failed",
    )
    .with_details(details)
}

impl From<QueryRejection> for ErrorResponse {
    fn from(value: QueryRejection) -> Self {
        query_rejection(value.body_text())
    }
}

/// [`crate::extract::HtmlFormQuery`] 的 rejection（见 `query_rejection`）。
impl From<axum_extra::extract::QueryRejection> for ErrorResponse {
    fn from(value: axum_extra::extract::QueryRejection) -> Self {
        query_rejection(value.body_text())
    }
}

/// 兜底：未命中的路由 → 404 `http_error`，与上游「其他 HTTPException」一致。
///
/// axum 0.8 的 `Handler` 只实现在 async fn 上，所以这里必须 async
/// —— 即使它不 await 任何东西。
pub async fn not_found() -> ErrorResponse {
    ErrorResponse::new(StatusCode::NOT_FOUND, "http_error", "Not Found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_error_carries_its_status_over() {
        let err = ServiceError::conflict("playlist_name_conflict", "already exists", None);
        let response: ErrorResponse = err.into();
        assert_eq!(response.status, StatusCode::CONFLICT);
        assert_eq!(response.error.code, "playlist_name_conflict");
    }

    #[test]
    fn validation_error_is_422() {
        let err = ServiceError::validation("validation_error", "empty name");
        let response: ErrorResponse = err.into();
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn not_found_carries_the_entity_id_detail() {
        let err = ServiceError::not_found("playlist_not_found", "nope", "playlist_id", 7);
        let response: ErrorResponse = err.into();
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(
            response.error.details.as_ref().unwrap().get("playlist_id"),
            Some(&json!(7))
        );
    }

    #[test]
    fn auth_failures_share_401_but_differ_in_message() {
        let missing: ErrorResponse =
            AuthFailure::from(sm_core::auth::MissingCredentials::NoHeader).into();
        let invalid: ErrorResponse = AuthFailure::from(sm_core::jwt::JwtError::Expired).into();

        assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
        assert_eq!(invalid.status, StatusCode::UNAUTHORIZED);
        assert_eq!(missing.error.code, "unauthorized");
        assert_eq!(invalid.error.code, "unauthorized");
        assert_eq!(missing.error.message, "Authentication required");
        assert_eq!(invalid.error.message, "Invalid access token");
    }

    #[test]
    fn out_of_range_service_status_falls_back_to_500() {
        // `http::StatusCode::from_u16` 接受 100..=999 —— 999 是**合法**的，
        // 会原样透传。真正的防御目标是越界值（0 或 >999）：那必须落到 500，
        // 而不是让 unwrap 把整个请求线程带走。
        let err = ServiceError {
            status: 1000,
            api: Box::new(ApiError::new("weird", "weird")),
        };
        let response: ErrorResponse = err.into();
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn in_range_status_is_passed_through_verbatim() {
        // 999 虽不常见但合法，透传而不是被"修正"成 500 —— 契约要求状态码
        // 由 service 层决定，这一层不该改写。
        let err = ServiceError {
            status: 999,
            api: Box::new(ApiError::new("weird", "weird")),
        };
        let response: ErrorResponse = err.into();
        assert_eq!(response.status, 999);
    }

    #[test]
    fn signature_failures_are_403_with_distinct_codes() {
        // 三种失败共用 403，靠 code 区分 —— 客户端据此决定是刷新页面还是报错
        for (error, expected) in [
            (SignatureError::PathInvalid, "file_path_invalid"),
            (SignatureError::Expired, "file_signature_expired"),
            (SignatureError::Invalid, "file_signature_invalid"),
        ] {
            let response: ErrorResponse = error.into();
            assert_eq!(response.status, StatusCode::FORBIDDEN);
            assert_eq!(response.error.code, expected);
        }
    }

    #[tokio::test]
    async fn fallback_is_404_http_error() {
        let response = not_found().await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.error.code, "http_error");
        assert_eq!(response.error.message, "Not Found");
    }
}
