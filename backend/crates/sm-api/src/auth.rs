//! 鉴权提取器 —— 复刻上游 `get_current_user` 依赖。
//!
//! # 上游的判定顺序（`src/service/system/auth_service.py:111-128`）
//!
//! ```text
//! 1. 取不到 token        -> 401 unauthorized "Authentication required"
//! 2. jwt.decode 失败     -> 401 unauthorized "Invalid access token"
//! 3. type != "access"    -> 401 unauthorized "Invalid access token"
//! 4. 按 sub 查 User 为空 -> 401 unauthorized "Invalid access token"
//! ```
//!
//! **第 4 步容易漏。** 只验签不查库的话，一个签名正确但用户已删除的 token
//! 会被当成有效 —— 而上游明确会 401。所以这里接了
//! `UserRepository::find_by_id`，不是"先跑通再说"。
//!
//! # 为什么不返回 `WWW-Authenticate`
//!
//! 上游用 `OAuth2PasswordBearer(auto_error=False)`，缺失时**自己抛** ApiError
//! 而不是让 FastAPI 返回标准 401。所以这里同样不带该响应头。

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chrono::Utc;
use sm_core::auth::{AuthFailure, InvalidAccessToken};
use sm_core::jwt;
use sm_db::repo::api_key::{self, ApiKeyRepository};
use sm_db::repo::UserRepository;

use crate::error::ErrorResponse;
use crate::state::AppState;

/// 通过认证的请求方。
///
/// 目前只带 id —— 上游是单用户部署，路由拿到 `current_user` 后几乎不用，
/// 但要**证明认证发生过**。等 `system` 域移植到再按需扩展字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurrentUser {
    pub id: i32,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ErrorResponse;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());

        let token = sm_core::auth::extract_bearer_token(header).map_err(AuthFailure::from)?;

        // 上游 `deps.py:get_current_user`：`sk-` 前缀走 API key，否则走 JWT。
        if token.starts_with(api_key::API_KEY_PREFIX) {
            return Self::via_api_key(token, state).await;
        }

        let access = jwt::decode_access_token(token, &state.auth().secret, Utc::now())
            .map_err(AuthFailure::from)?;

        // 上游第 4 步：用户不存在同样是 "Invalid access token"。
        let users = UserRepository::new(state.db().clone());
        match users.find_by_id(access.user_id as i32).await {
            Ok(Some(user)) => Ok(CurrentUser { id: user.id }),
            Ok(None) => Err(AuthFailure::Invalid(InvalidAccessToken).into()),
            Err(err) => Err(ErrorResponse::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            )),
        }
    }
}

impl CurrentUser {
    /// API key 鉴权。对应用游 `ApiKeyService.authenticate`。
    ///
    /// 1. 按 `sha256(raw_key)` 查 `api_keys`，找不到 → 401
    /// 2. `last_used_at` 超过 5 分钟未更新则刷新（节流写库）
    /// 3. 取单用户（上游 `User.select().order_by(User.id).first()`），找不到 → 401
    async fn via_api_key(token: &str, state: &AppState) -> Result<Self, ErrorResponse> {
        let keys = ApiKeyRepository::new(state.db().clone());
        let key_hash = api_key::hash_key(token);
        let row = keys
            .find_by_hash(&key_hash)
            .await
            .map_err(|err| {
                ErrorResponse::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    err.to_string(),
                )
            })?
            .ok_or_else(|| AuthFailure::Invalid(InvalidAccessToken))?;

        // last_used_at 节流更新。
        let now = Utc::now().naive_utc();
        if api_key::needs_touch(row.last_used_at, now) {
            let _ = keys.touch_last_used(row.id, now).await;
        }

        // 上游是单用户部署，取第一个用户。
        let users = UserRepository::new(state.db().clone());
        match users.find_primary().await {
            Ok(Some(user)) => Ok(CurrentUser { id: user.id }),
            Ok(None) => Err(AuthFailure::Invalid(InvalidAccessToken).into()),
            Err(err) => Err(ErrorResponse::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                err.to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use sm_core::jwt::encode_access_token;

    #[test]
    fn token_shape_is_what_the_extractor_expects() {
        // 自签自解，确认提取器用的 secret/now 参数顺序没搞反。
        let secret = "s3cret";
        let expires = Utc::now() + Duration::hours(1);
        let token = encode_access_token(42, expires, secret);
        let decoded = jwt::decode_access_token(&token, secret, Utc::now()).unwrap();
        assert_eq!(decoded.user_id, 42);
    }

    #[test]
    fn expired_token_is_rejected_before_the_db_lookup() {
        let secret = "s3cret";
        let token = encode_access_token(42, Utc::now() - Duration::hours(1), secret);
        assert!(matches!(
            jwt::decode_access_token(&token, secret, Utc::now()),
            Err(jwt::JwtError::Expired)
        ));
    }

    #[test]
    fn wrong_secret_is_a_signature_mismatch() {
        let expires = Utc::now() + Duration::hours(1);
        let token = encode_access_token(42, expires, "right");
        assert!(matches!(
            jwt::decode_access_token(&token, "wrong", Utc::now()),
            Err(jwt::JwtError::SignatureMismatch)
        ));
    }
}
