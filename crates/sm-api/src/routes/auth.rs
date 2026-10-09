//! `POST /auth/tokens` 与 `POST /auth/token-refreshes`。
//!
//! # 两个端点的鉴权要求**不一样**（照抄上游，别"顺手统一"）
//!
//! | 端点 | 是否要 access token | 上游 |
//! |---|---|---|
//! `/auth/tokens` | **不需要** | `routers/system/auth.py:15-26`，无 `get_current_user` |
//! `/auth/token-refreshes` | **需要** | 同文件 `:34-43`，有 `Depends(get_current_user)` |
//!
//! 刷新要带 access token 这件事看起来多余（都有有效 token 了还刷新什么），
//! 但它是上游的既有行为，改掉会让「access 过期、只剩 refresh」的客户端
//! 拿不到新令牌 —— 那正是刷新存在的场景。上游既然这么写，就照搬。
//!
//! # 不实现 `/auth/docs-token`
//!
//! 上游有个 `include_in_schema=False` 的 `docs-token`（`:46-55`），只为
//! Swagger 的 OAuth2 表单服务，不在 OpenAPI 契约里。本批不搬。

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::routing::post;
use axum::Router;
use sm_service::system::auth::{AuthService, TokenPair};

use crate::auth::CurrentUser;
use crate::dto::{TokenCreateRequest, TokenRefreshRequest, TokenResource};
use crate::error::ErrorResponse;
use crate::extract::Json as EnvelopeJson;
use crate::routes::method_not_allowed;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/auth/tokens",
            post(create_token_pair).fallback(method_not_allowed),
        )
        .route(
            "/auth/token-refreshes",
            post(refresh_token_pair).fallback(method_not_allowed),
        )
}

/// 登录。**不带鉴权** —— 这是拿 token 的地方。
async fn create_token_pair(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    EnvelopeJson(payload): EnvelopeJson<TokenCreateRequest>,
) -> Result<(StatusCode, axum::Json<TokenResource>), ErrorResponse> {
    let service = AuthService::new(state.db());
    let pair = service
        .login(
            state.auth(),
            &payload.username,
            &payload.password,
            // 客户端 IP 需要 `ConnectInfo`（真实监听才有 socket addr），
            // 本批用 oneshot 驱动 Router，拿不到，故留空。审计列可空。
            None,
            user_agent_of(&headers).as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(TokenResource::from(pair))))
}

/// 刷新。**带鉴权**（上游行为，见模块文档）。
async fn refresh_token_pair(
    _user: CurrentUser,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    EnvelopeJson(payload): EnvelopeJson<TokenRefreshRequest>,
) -> Result<(StatusCode, axum::Json<TokenResource>), ErrorResponse> {
    let service = AuthService::new(state.db());
    let pair = service
        .refresh(
            state.auth(),
            &payload.refresh_token,
            None,
            user_agent_of(&headers).as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(TokenResource::from(pair))))
}

fn user_agent_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// 让 TokenPair → TokenResource 的转换只写一处。
impl From<TokenPair> for TokenResource {
    fn from(value: TokenPair) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            token_type: value.token_type,
            expires_in: value.expires_in,
            expires_at: crate::dto::format_utc(value.expires_at),
            refresh_expires_at: crate::dto::format_utc(value.refresh_expires_at),
            user: crate::dto::AuthUserSummary {
                username: value.username,
            },
        }
    }
}
