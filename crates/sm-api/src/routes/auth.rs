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
//! # `/auth/docs-token` **刻意不实现**（是决定，不是待办）
//!
//! 上游有个 `include_in_schema=False` 的 `POST /auth/docs-token`
//! （`routers/system/auth.py:46-55`），只做两件事：
//!
//! 1. 收 **form 编码**的 `username` / `password`（`OAuth2PasswordRequestForm`，
//!    不是 JSON —— 与本模块两个端点的 `EnvelopeJson` 不同）
//! 2. 只回 `{access_token, token_type}` 两个字段（**没有** `refresh_token`）
//!
//! 它的**唯一消费方是 Swagger UI 的 OAuth2 密码表单**
//! （`deps.py:8` 的 `tokenUrl="/auth/docs-token"`）。
//!
//! ## 本仓库没有那个消费方
//!
//! 实测：`sm-api` 无 `/docs` 路由，全仓无 `utoipa` 依赖（0 处引用），
//! 无 Swagger / Redoc / Rapidoc / Scalar。`enable_docs` 这个配置键只在
//! `sm_core::config_schema.rs:132` 的**键描述**里出现（从上游 schema 抄来的），
//! 没有任何代码读它。
//!
//! ## 所以不实现
//!
//! 实现它要付出：为 `axum` 加 `form` feature、写一个新的 form 提取器
//! （本仓库的 `extract.rs` 只有 `Json` / `Query` / `Multipart`）、
//! 加一个没人调用的 handler。**用一个没有调用方的端点去把上游的 126 凑齐，
//! 会让「端点数」这个指标失去意义** —— 它本来就不该计入契约。
//!
//! 什么时候该做：等仓库真的接了 OpenAPI/Swagger（那时 `include_in_schema`
//! 才有对应物，`utoipa` 的 `#[utoipa(path(exclude))]` 才能落地）。
//!
//! 参考：`docs/handoff.md` 第五节「上游的缺陷刻意照抄」是另一回事 ——
//! 那条是**照抄缺陷**，这条是**不实现无消费方的端点**。

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
