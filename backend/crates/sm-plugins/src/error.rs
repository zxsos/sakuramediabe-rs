//! 插件侧失败 → HTTP 错误信封。
//!
//! # 上游对应
//!
//! `src/service/transfers/downloads/common.py` 的 `provider_error()`：
//!
//! ```python
//! status_code = {
//!     "invalid_config": 422, "authentication_failed": 401,
//!     "source_not_found": 404, "task_not_managed": 409,
//!     "source_blacklisted": 422, "unsupported": 422, "unavailable": 503,
//! }.get(exc.code, 502)
//! return ApiError(status_code, f"provider_{exc.code}", exc.safe_message,
//!                 {"provider_key": ..., "operation": ...})
//! ```
//!
//! 这张表**逐条照搬**，包括那个 502 兜底 —— 「插件报了一个宿主不认识的码」
//! 不能当成 500（那是我们自己的缺陷），也不能当成 400。
//!
//! # 只用 `safe_message`
//!
//! 上游字段名是 `safe_message`：插件可以带内部细节（堆栈、远端响应体），但那些
//! 不该进响应体。宿主**只转发安全文案**，从不透传原始错误。
//!
//! # 为什么返回 `sm_core::ApiError` 而不是 `ServiceError`
//!
//! `sm-plugins` 不依赖 `sm-service`（见 Cargo.toml）。状态码与错误码的**绑定**
//! 属于契约层，不该因为「谁要用」而搬家；`sm-service` 拿到后自己包成
//! `ServiceError` 即可。

use serde_json::{Map, Value};
use sm_core::ApiError;

/// 插件声明的失败码 → HTTP 状态码。
///
/// 未知码一律 502：那是「插件坏了 / 版本不一致」，不是用户请求的问题，
/// 也不是我们自己的缺陷（所以不是 500）。
pub fn status_for(code: &str) -> u16 {
    match code {
        "invalid_config" => 422,
        "authentication_failed" => 401,
        "source_not_found" => 404,
        "task_not_managed" => 409,
        "source_blacklisted" => 422,
        "unsupported" => 422,
        "unavailable" => 503,
        _ => 502,
    }
}

/// 响应体里的错误码：`provider_{code}`。
///
/// 加前缀是为了让客户端一眼看出「这是插件报的」，而不是宿主自己的校验错误
/// （那些是 `invalid_movie_filter` / `tag_not_found` 这类裸名）。
pub fn error_code_for(code: &str) -> String {
    format!("provider_{code}")
}

/// 一次插件调用失败的信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// 插件给的失败码（上面那张表的键）。
    pub code: String,
    /// 插件给的**安全**文案。
    pub message: String,
    pub provider_key: String,
    /// 失败发生在哪个操作上（上游 details 里有这个键）。
    pub operation: Option<String>,
}

impl ProviderFailure {
    /// HTTP 状态码。
    pub fn status(&self) -> u16 {
        status_for(&self.code)
    }

    /// 响应体的 `error` 对象。**不含**任何插件内部细节。
    pub fn to_api_error(&self) -> ApiError {
        let mut details = Map::new();
        details.insert(
            "provider_key".to_owned(),
            Value::from(self.provider_key.clone()),
        );
        if let Some(operation) = &self.operation {
            details.insert("operation".to_owned(), Value::from(operation.clone()));
        }
        ApiError::new(error_code_for(&self.code), self.message.clone()).with_details(details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_code_maps_to_the_upstream_status() {
        for (code, expected) in [
            ("invalid_config", 422),
            ("authentication_failed", 401),
            ("source_not_found", 404),
            ("task_not_managed", 409),
            ("source_blacklisted", 422),
            ("unsupported", 422),
            ("unavailable", 503),
        ] {
            assert_eq!(status_for(code), expected, "{code}");
        }
    }

    #[test]
    fn an_unknown_code_is_502_not_500() {
        // 插件报了宿主不认识的码：那是插件/版本问题，不是我们自己的缺陷。
        assert_eq!(status_for("something_new"), 502);
        assert_eq!(status_for(""), 502);
    }

    #[test]
    fn the_error_code_is_prefixed_and_details_carry_the_context() {
        let failure = ProviderFailure {
            code: "unavailable".to_owned(),
            message: "远端无响应".to_owned(),
            provider_key: "local".to_owned(),
            operation: Some("browse".to_owned()),
        };

        assert_eq!(failure.status(), 503);
        let error = failure.to_api_error();
        assert_eq!(error.code, "provider_unavailable");
        assert_eq!(error.message, "远端无响应");

        let details = error.details.expect("应当带 details");
        assert_eq!(details.get("provider_key"), Some(&Value::from("local")));
        assert_eq!(details.get("operation"), Some(&Value::from("browse")));
    }

    #[test]
    fn the_operation_is_omitted_when_absent() {
        // 没有 operation 时不该塞一个空串键进去 —— 客户端会对键做存在性判断。
        let failure = ProviderFailure {
            code: "unsupported".to_owned(),
            message: "不支持".to_owned(),
            provider_key: "local".to_owned(),
            operation: None,
        };
        let details = failure.to_api_error().details.expect("应当带 details");
        assert!(details.get("provider_key").is_some());
        assert!(details.get("operation").is_none());
    }
}
