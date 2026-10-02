//! 认证令牌载荷：`POST /auth/tokens` 的请求与响应。
//!
//! 对应客户端：
//!   lib/features/auth/data/auth_tokens_dto.dart
//!   lib/core/session/session_token_payload.dart
//!
//! # 严格与宽松共存（重写时最容易搞错的地方）
//!
//! 同一个响应里，两类字段的校验强度完全不同：
//!
//! | 字段 | 强度 | 缺失或非法时 |
//! |---|---|---|
//! | `access_token` | **严格** | 抛 `invalid_auth_response` |
//x| `refresh_token` | **严格** | 抛 `invalid_auth_response` |
//x| `expires_at` | **严格** | 抛 `invalid_auth_response` |
//x| `token_type` | 宽松 | 回落 `"Bearer"` |
//x| `expires_in` | 宽松 | 回落 `0` |
//x| `refresh_expires_at` | 宽松 | 回落 **epoch(0)**，不是 null |
//x| `user.username` | 宽松 | 回落空串 |
//!
//! 推论：**后端必须保证前三个字段永远合法**。客户端一旦收到空 token
//x 就会直接抛异常，表现为「登录莫名失败」而不是降级。

use std::str::FromStr;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 认证失败时的错误码，与客户端 `SessionTokenPayload.invalidResponseCode` 一致。
pub const INVALID_AUTH_RESPONSE: &str = "invalid_auth_response";
/// 对应的中文提示。
pub const INVALID_AUTH_RESPONSE_MESSAGE: &str = "认证响应格式错误";

/// 令牌解析失败。客户端对应抛 `ApiException`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidAuthResponse;

impl std::fmt::Display for InvalidAuthResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(INVALID_AUTH_RESPONSE_MESSAGE)
    }
}

impl std::error::Error for InvalidAuthResponse {}

/// 登录用户。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUser {
    pub username: String,
}

impl AuthUser {
    /// 宽松构造：字段缺失或非字符串时回落为空串。
    pub fn from_body(body: Option<&Value>) -> Self {
        let username = body
            .and_then(|value| value.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Self { username }
    }
}

/// 令牌响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: i64,
    pub expires_at: DateTime<Utc>,
    pub refresh_expires_at: DateTime<Utc>,
    pub user: AuthUser,
}

/// 严格解析：字段必须是字符串、trim 后非空。对应客户端 `_requiredToken`。
fn required_token(body: &Value, key: &str) -> Result<String, InvalidAuthResponse> {
    let raw = body
        .get(key)
        .and_then(Value::as_str)
        .ok_or(InvalidAuthResponse)?;
    let normalized = raw.trim();
    if normalized.is_empty() {
        return Err(InvalidAuthResponse);
    }
    Ok(normalized.to_owned())
}

/// 严格解析时间：必须是字符串、trim 后非空、且能被解析。
///
/// 对应客户端 `_requiredExpiresAt`，注意它最后会 `.toUtc()`。
fn required_expires_at(body: &Value, key: &str) -> Result<DateTime<Utc>, InvalidAuthResponse> {
    let raw = body
        .get(key)
        .and_then(Value::as_str)
        .ok_or(InvalidAuthResponse)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(InvalidAuthResponse);
    }
    DateTime::parse_from_rfc3339(trimmed)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|_| InvalidAuthResponse)
}

/// 宽松解析时间：失败回落 epoch(0)。
///
/// 对应客户端的 `DateTime.fromMillisecondsSinceEpoch(0, isUtc: true)` —
/// 注意兜底值是 **1970-01-01** 而不是 null，序列化出去会是一个合法的时间戳。
fn lenient_expires_at(body: &Value, key: &str) -> DateTime<Utc> {
    body.get(key)
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw.trim()).ok())
        .map_or_else(epoch, |parsed| parsed.with_timezone(&Utc))
}

/// Unix epoch（UTC）。`refresh_expires_at` 的兜底值。
pub fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("epoch 恒可表示")
}

impl AuthTokens {
    /// 从响应体解析，严格字段不合法时返回 `InvalidAuthResponse`。
    pub fn from_body(body: &Value) -> Result<Self, InvalidAuthResponse> {
        let access_token = required_token(body, "access_token")?;
        let refresh_token = required_token(body, "refresh_token")?;
        let expires_at = required_expires_at(body, "expires_at")?;

        let token_type = body
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_owned();
        let expires_in = crate::json::as_int_or_null(body.get("expires_in").unwrap_or(&Value::Null))
            .unwrap_or(0);

        Ok(Self {
            access_token,
            refresh_token,
            token_type,
            expires_in,
            expires_at,
            refresh_expires_at: lenient_expires_at(body, "refresh_expires_at"),
            user: AuthUser::from_body(body.get("user")),
        })
    }

    /// 序列化为客户端期望的形态。
    ///
    /// 时间格式必须用 `SecondsFormat::Millis` + `Z`：Dart 的
    /// `DateTime.toIso8601String()` 对 UTC 输出 `2026-10-02T12:00:00.000Z`，
    /// 而 Rust 默认的 `to_rfc3339()` 是 `+00:00` 且不带毫秒。
    /// 客户端两种都能解析，但保持一致可避免对拍时的字节差异。
    pub fn to_client_json(&self) -> Value {
        serde_json::json!({
            "access_token": self.access_token,
            "refresh_token": self.refresh_token,
            "token_type": self.token_type,
            "expires_in": self.expires_in,
            "expires_at": self.expires_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "refresh_expires_at": self.refresh_expires_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "user": { "username": self.user.username },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body() -> serde_json::Value {
        json!({
            "access_token": "at",
            "refresh_token": "rt",
            "token_type": "Bearer",
            "expires_in": 3600,
            "expires_at": "2026-10-02T12:00:00Z",
            "refresh_expires_at": "2026-11-02T12:00:00Z",
            "user": {"username": "account"}
        })
    }

    #[test]
    fn parses_well_formed_response() {
        let tokens = AuthTokens::from_body(&body()).unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "rt");
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in, 3600);
        assert_eq!(tokens.user.username, "account");
        assert_eq!(tokens.expires_at.to_rfc3339(), "2026-10-02T12:00:00+00:00");
    }

    #[test]
    fn strict_fields_reject_missing_or_blank() {
        for key in ["access_token", "refresh_token", "expires_at"] {
            for bad in [json!(null), json!(""), json!("   "), json!(42)] {
                let mut payload = body();
                payload[key] = bad.clone();
                assert_eq!(
                    AuthTokens::from_body(&payload),
                    Err(InvalidAuthResponse),
                    "key={key} value={bad}"
                );
            }
            let mut payload = body();
            payload.as_object_mut().unwrap().remove(key);
            assert_eq!(AuthTokens::from_body(&payload), Err(InvalidAuthResponse));
        }
    }

    #[test]
    fn strict_fields_reject_malformed_timestamp() {
        let mut payload = body();
        payload["expires_at"] = json!("not-a-date");
        assert_eq!(AuthTokens::from_body(&payload), Err(InvalidAuthResponse));
    }

    #[test]
    fn lenient_fields_use_client_defaults() {
        let mut payload = body();
        let obj = payload.as_object_mut().unwrap();
        obj.remove("token_type");
        obj.remove("expires_in");
        obj.remove("refresh_expires_at");
        obj.remove("user");

        let tokens = AuthTokens::from_body(&payload).unwrap();
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in, 0);
        assert_eq!(tokens.refresh_expires_at, epoch());
        assert_eq!(tokens.user.username, "");
    }

    #[test]
    fn refresh_expires_at_falls_back_to_epoch_not_null() {
        let mut payload = body();
        payload["refresh_expires_at"] = json!("garbage");
        let tokens = AuthTokens::from_body(&payload).unwrap();
        assert_eq!(tokens.refresh_expires_at.timestamp(), 0);
    }

    #[test]
    fn lenient_fields_tolerate_wrong_types() {
        let mut payload = body();
        payload["token_type"] = json!(7);
        payload["expires_in"] = json!("900");
        payload["user"] = json!("not-an-object");
        let tokens = AuthTokens::from_body(&payload).unwrap();
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.expires_in, 900, "数字字符串按宽松层转 int");
        assert_eq!(tokens.user.username, "");
    }

    #[test]
    fn tokens_are_trimmed() {
        let mut payload = body();
        payload["access_token"] = json!("  at  ");
        payload["refresh_token"] = json!("  rt  ");
        let tokens = AuthTokens::from_body(&payload).unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token, "rt");
    }

    #[test]
    fn client_json_uses_dart_iso_format() {
        let tokens = AuthTokens::from_body(&body()).unwrap();
        let json = tokens.to_client_json();
        assert_eq!(json["expires_at"], "2026-10-02T12:00:00.000Z");
        assert_eq!(json["refresh_expires_at"], "2026-11-02T12:00:00.000Z");
        assert_eq!(json["user"]["username"], "account");
    }

    #[test]
    fn offset_timestamp_is_normalized_to_utc() {
        let mut payload = body();
        payload["expires_at"] = json!("2026-10-02T20:00:00+08:00");
        let tokens = AuthTokens::from_body(&payload).unwrap();
        assert_eq!(tokens.expires_at.to_rfc3339(), "2026-10-02T12:00:00+00:00");
    }
}
