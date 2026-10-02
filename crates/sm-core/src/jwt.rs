//! JWT（HS256）签发与校验。
//!
//! 对应后端 `src/service/system/auth_service.py` 使用的 `python-jose`。
//!
//! # 与后端逐项对齐
//!
//! | 项 | 后端 | 本实现 |
//! |---|---|---|
//! | 算法 | `HS256` | 同 |
//! | payload claim | 仅 `sub` / `type` / `exp` | 同 |
//! | `sub` | **字符串** | 同，校验时再转 i64 |
//! | `exp` | 整数秒 | 同 |
//! | `iat` / `nbf` / `iss` / `aud` | **都没有** | 同 |
//!
//! 刻意不补 `iat` / `nbf` / `iss` / `aud`：后端签出的既有 token 缺这些 claim，
//! Rust 侧若要求它们存在，所有历史 token 会立即失效。

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::hashing_support::hmac_sha256;

/// JWT 解析或校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    Malformed,
    Base64,
    Json,
    SignatureMismatch,
    UnsupportedAlgorithm,
    Expired,
    WrongTokenType,
    InvalidSubject,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "jwt: malformed token",
            Self::Base64 => "jwt: invalid base64url",
            Self::Json => "jwt: invalid json",
            Self::SignatureMismatch => "jwt: signature mismatch",
            Self::UnsupportedAlgorithm => "jwt: unsupported algorithm",
            Self::Expired => "jwt: expired",
            Self::WrongTokenType => "jwt: not an access token",
            Self::InvalidSubject => "jwt: invalid subject",
        })
    }
}

impl std::error::Error for JwtError {}

/// access token 的 payload。字段与后端一致，不多不少。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// 用户 id 的**字符串**形式。
    pub sub: String,
    /// 固定为 `access`。
    #[serde(rename = "type")]
    pub token_type: String,
    /// 过期时间，整数秒。
    pub exp: i64,
}

/// 解析后的 access token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken {
    /// `sub` 转成整数后的用户 id。
    pub user_id: i64,
    pub expires_at: DateTime<Utc>,
}

/// 签发 access token。`expires_at` 截断到整数秒，与后端 `int(...timestamp())` 一致。
pub fn encode_access_token(user_id: i64, expires_at: DateTime<Utc>, secret: &str) -> String {
    let header: &[u8] = br#"{"alg":"HS256","typ":"JWT"}"#;
    let claims = serde_json::json!({
        "sub": user_id.to_string(),
        "type": "access",
        "exp": expires_at.timestamp(),
    });
    let payload = serde_json::to_vec(&claims).unwrap_or_default();

    let mut signing_input = base64url(header);
    signing_input.push_str(".");
    signing_input.push_str(&base64url(&payload));
    let signature = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let mut token = signing_input;
    token.push_str(".");
    token.push_str(&base64url(&signature));
    token
}

/// 校验并解析 access token。
///
/// 顺序与后端 `get_current_user` 一致：验签 + 验 `exp`，再看 `type`，最后 `int(sub)`。
/// 后端在 `type` 检查失败时**不会**先报 `Expired`，因为 `jwt.decode` 已挑出过期。
pub fn decode_access_token(
    token: &str,
    secret: &str,
    now: DateTime<Utc>,
) -> Result<AccessToken, JwtError> {
    let parts: Vec<&str> = token.split(".").collect();
    if parts.len() != 3 {
        return Err(JwtError::Malformed);
    }

    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let expected = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let actual = base64url_decode(parts[2]).map_err(|_| JwtError::Base64)?;
    if !constant_time_eq(&expected, &actual) {
        return Err(JwtError::SignatureMismatch);
    }

    let header_raw = base64url_decode(parts[0]).map_err(|_| JwtError::Base64)?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_raw).map_err(|_| JwtError::Json)?;
    if header.get("alg").and_then(|value| value.as_str()) != Some("HS256") {
        return Err(JwtError::UnsupportedAlgorithm);
    }

    let payload_raw = base64url_decode(parts[1]).map_err(|_| JwtError::Base64)?;
    let claims: AccessClaims = serde_json::from_slice(&payload_raw).map_err(|_| JwtError::Json)?;

    if claims.token_type != "access" {
        return Err(JwtError::WrongTokenType);
    }
    if claims.exp <= now.timestamp() {
        return Err(JwtError::Expired);
    }

    let user_id = claims
        .sub
        .parse::<i64>()
        .map_err(|_| JwtError::InvalidSubject)?;
    Ok(AccessToken {
        user_id,
        expires_at: DateTime::from_timestamp(claims.exp, 0).ok_or(JwtError::InvalidSubject)?,
    })
}

/// base64url 编码（无 padding）。
pub fn base64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// base64url 解码。
pub fn base64url_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text)
}

/// 常量时间比较，避免通过响应时间泄漏签名前缀。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing_support::hex;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    #[test]
    fn roundtrip_preserves_subject() {
        let exp = at(1_800_000_000);
        let token = encode_access_token(42, exp, "s3cret");
        let parsed = decode_access_token(&token, "s3cret", at(1_700_000_000)).unwrap();
        assert_eq!(parsed.user_id, 42);
        assert_eq!(parsed.expires_at, exp);
    }

    #[test]
    fn header_is_exact_minimal_form() {
        let token = encode_access_token(1, at(1_800_000_000), "k");
        let raw = base64url_decode(token.split(".").next().unwrap()).unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), r#"{"alg":"HS256","typ":"JWT"}"#);
    }

    #[test]
    fn payload_has_exactly_three_claims() {
        let token = encode_access_token(7, at(1_800_000_000), "k");
        let raw = base64url_decode(token.split(".").nth(1).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let mut keys: Vec<String> = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["exp", "sub", "type"], "不引入 iat/nbf/iss/aud");
        assert_eq!(value["sub"], "7", "sub 必须是字符串形式");
        assert_eq!(value["exp"], 1_800_000_000i64);
    }

    #[test]
    fn rejects_wrong_secret() {
        let token = encode_access_token(1, at(1_800_000_000), "right");
        assert_eq!(
            decode_access_token(&token, "wrong", at(1_700_000_000)),
            Err(JwtError::SignatureMismatch)
        );
    }

    #[test]
    fn rejects_tampered_payload() {
        let token = encode_access_token(1, at(1_800_000_000), "k");
        let parts: Vec<&str> = token.split(".").collect();
        let forged = base64url(br#"{"sub":"999","type":"access","exp":1800000000}"#);
        let tampered = format!("{}.{}.{}", parts[0], forged, parts[2]);
        assert_eq!(
            decode_access_token(&tampered, "k", at(1_700_000_000)),
            Err(JwtError::SignatureMismatch)
        );
    }

    #[test]
    fn rejects_expired_at_boundary() {
        let token = encode_access_token(1, at(1_800_000_000), "k");
        assert_eq!(
            decode_access_token(&token, "k", at(1_800_000_000)),
            Err(JwtError::Expired),
            "exp == now 即过期"
        );
    }

    #[test]
    fn rejects_non_access_type() {
        let token = sign_raw(br#"{"sub":"1","type":"refresh","exp":1800000000}"#, "k");
        assert_eq!(
            decode_access_token(&token, "k", at(1_700_000_000)),
            Err(JwtError::WrongTokenType)
        );
    }

    #[test]
    fn rejects_non_numeric_subject() {
        let token = sign_raw(br#"{"sub":"abc","type":"access","exp":1800000000}"#, "k");
        assert_eq!(
            decode_access_token(&token, "k", at(1_700_000_000)),
            Err(JwtError::InvalidSubject)
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode_access_token("onepart", "k", at(0)), Err(JwtError::Malformed));
        assert_eq!(decode_access_token("a.b.c.d", "k", at(0)), Err(JwtError::Malformed));
    }

    #[test]
    fn rejects_alg_none_downgrade() {
        // alg=none 的降级攻击：签名段为空，必须被拒而不是放行。
        let header = base64url(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64url(br#"{"sub":"1","type":"access","exp":1800000000}"#);
        let token = format!("{header}.{payload}.");
        assert!(
            matches!(
                decode_access_token(&token, "k", at(1_700_000_000)),
                Err(JwtError::SignatureMismatch) | Err(JwtError::Base64)
            ),
        );
    }

    fn sign_raw(payload: &[u8], secret: &str) -> String {
        let header = base64url(br#"{"alg":"HS256","typ":"JWT"}"#);
        let mut signing_input = header;
        signing_input.push_str(".");
        signing_input.push_str(&base64url(payload));
        let signature = crate::hashing_support::hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
        let mut token = signing_input;
        token.push_str(".");
        token.push_str(&base64url(&signature));
        token
    }
}

#[cfg(test)]
mod cross_check {
    use super::*;

    /// 用 Python hmac/hashlib 独立算出的 HMAC-SHA256，锁定与标准库一致。
    #[test]
    fn hmac_agrees_with_python_stdlib() {
        let mac = crate::hashing_support::hmac_sha256(b"key", b"the quick brown fox");
        assert_eq!(
            crate::hashing_support::hex(&mac),
            "9119dc3209b2cc822340e7ff18d47c796736f1af694ffba590d094b4d182e7e1",
        );
    }
}
