//! API 错误信封：`{"error": {"code", "message", "details"}}`。
//!
//! 对应客户端 `lib/core/network/api_error_dto.dart`。
//!
//! # 契约要点（逐条对齐客户端行为）
//!
//! | 字段 | 规则 |
//! |---|---|
//! | `code` | 缺失或非字符串时客户端回落到 `"unknown_error"` |
//! | `message` | 缺失或非字符串时客户端回落到 `"Unknown error"` |
//! | `details` | 非对象时客户端视为 `null`；为 `null` 时**序列化不输出该键** |
//!
//! `details` 的 `skip_serializing_if` 与客户端的 `if (details != null)` 一致 ——
//! 它会影响响应体字节数，值得被测试锁定。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 单个 API 错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// 机器可读的错误码。客户端按此分支处理（如 `invalid_credentials`）。
    pub code: String,
    /// 面向用户的中文提示。
    pub message: String,
    /// 附加上下文。序列化时为 `None` 则省略该键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Map<String, Value>>,
}

impl ApiError {
    /// 构造不带 details 的错误。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }

    /// 附加 details（builder 风格）。
    pub fn with_details(mut self, details: Map<String, Value>) -> Self {
        self.details = Some(details);
        self
    }

    /// 从响应体解析，宽容度与客户端一致。
    ///
    /// 顶层不是对象、`error` 缺失、`error` 不是对象时返回兜底错误而不是
    /// `Err` —— 客户端在这些情况下同样不报错，而是走 `Unknown error` 分支。
    pub fn from_body(body: &Value) -> Self {
        let fallback = || Self {
            code: "unknown_error".to_owned(),
            message: "Unknown error".to_owned(),
            details: None,
        };

        let Some(envelope) = body.as_object() else {
            return fallback();
        };
        let Some(error) = envelope.get("error").and_then(Value::as_object) else {
            return fallback();
        };

        Self {
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_owned(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown error")
                .to_owned(),
            details: error.get("details").and_then(Value::as_object).cloned(),
        }
    }
}

/// 错误信封：`{"error": {...}}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ApiError,
}

impl ErrorEnvelope {
    pub fn new(error: ApiError) -> Self {
        Self { error }
    }
}

impl From<ApiError> for ErrorEnvelope {
    fn from(value: ApiError) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omits_details_key_when_absent() {
        // 客户端 toJson 的 `if (details != null)` 行为，序列化结果必须一致。
        let error = ApiError::new("invalid_credentials", "用户名或密码错误");
        let text = serde_json::to_string(&error).unwrap();
        assert_eq!(
            text,
            r#"{"code":"invalid_credentials","message":"用户名或密码错误"}"#
        );
        assert!(!text.contains("details"));
    }

    #[test]
    fn includes_details_when_present() {
        let mut details = serde_json::Map::new();
        details.insert("field".into(), json!("username"));
        let error = ApiError::new("invalid_input", "参数错误").with_details(details);
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["details"]["field"], json!("username"));
    }

    #[test]
    fn parses_well_formed_envelope() {
        let body = json!({
            "error": {"code": "not_found", "message": "未找到", "details": {"id": 7}}
        });
        let error = ApiError::from_body(&body);
        assert_eq!(error.code, "not_found");
        assert_eq!(error.message, "未找到");
        assert_eq!(error.details.unwrap().get("id"), Some(&json!(7)));
    }

    #[test]
    fn falls_back_when_fields_missing_or_wrong_type() {
        // 缺 code / message -> 客户端默认值
        let error = ApiError::from_body(&json!({"error": {}}));
        assert_eq!(error.code, "unknown_error");
        assert_eq!(error.message, "Unknown error");

        // code 非字符串 -> 同样回落
        let error = ApiError::from_body(&json!({"error": {"code": 42, "message": true}}));
        assert_eq!(error.code, "unknown_error");
        assert_eq!(error.message, "Unknown error");

        // details 非对象 -> 视为 null
        let error =
            ApiError::from_body(&json!({"error": {"code":"x","message":"y","details":[1,2]}}));
        assert!(error.details.is_none());
    }

    #[test]
    fn falls_back_when_envelope_shape_is_wrong() {
        for body in [
            json!(null),
            json!({}),
            json!({"error": null}),
            json!([]),
            json!("text"),
        ] {
            let error = ApiError::from_body(&body);
            assert_eq!(error.code, "unknown_error");
            assert_eq!(error.message, "Unknown error");
            assert!(error.details.is_none());
        }
    }

    #[test]
    fn envelope_serializes_with_error_key() {
        let envelope = ErrorEnvelope::from(ApiError::new("e", "m"));
        assert_eq!(
            serde_json::to_string(&envelope).unwrap(),
            r#"{"error":{"code":"e","message":"m"}}"#
        );
    }
}
